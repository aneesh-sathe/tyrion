#!/usr/bin/env python3
"""Production Tyrion adapter for OpenCode.

OpenCode runs as a headless server inside the sandbox. The Assignment is sent
with `prompt_async`, progress is read live from the server's event stream,
and interruption uses the abort endpoint, so it stops the model's turn rather
than killing a process. `opencode run --attach` was tried first and rejected:
when its output is piped it prints nothing until the turn ends, so a long
Worker would look stalled to Tyrion's Watchdog.
"""

import base64
import json
import os
import queue
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

from native_skill import RequiredSkillFailure, exclude_runtime_byproducts

# A settled turn with no event for this long is treated as hung.
ABORT_GRACE_SECONDS = 20


def emit(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def git(*arguments, cwd=None):
    subprocess.run(["git", *arguments], cwd=cwd, check=True, stdout=subprocess.DEVNULL)


def prepare_workspace():
    base = os.environ.get("TYRION_BASE_BUNDLE")
    if not base:
        return os.environ.get("TYRION_WORKSPACE_ROOT", "/sandbox"), None
    root = tempfile.mkdtemp(
        prefix="tyrion-opencode-",
        dir=os.environ.get("TYRION_WORKSPACE_ROOT", "/sandbox"),
    )
    repository = os.path.join(root, "repository")
    git("clone", "-q", "-b", "tyrion-base", base, repository)
    exclude_runtime_byproducts(repository)
    return repository, root


def finish_workspace(repository):
    destination = os.environ.get("TYRION_CANDIDATE_BUNDLE")
    if not destination:
        return
    git("add", "-A", cwd=repository)
    staged = subprocess.run(
        ["git", "diff", "--cached", "--quiet"], cwd=repository, check=False
    ).returncode == 1
    worker_committed = subprocess.run(
        ["git", "diff", "--quiet", "origin/tyrion-base", "HEAD"],
        cwd=repository,
        check=False,
    ).returncode == 1
    # As in every adapter: commit uncommitted work, record one empty commit
    # only when the Worker changed nothing, never stack one on its commits.
    if staged or not worker_committed:
        git(
            "-c",
            "user.name=Tyrion Worker",
            "-c",
            "user.email=worker@tyrion.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "feat: save worker result",
            cwd=repository,
        )
    git("branch", "-f", "tyrion-result", "HEAD", cwd=repository)
    git("bundle", "create", destination, "refs/heads/tyrion-result", cwd=repository)


def write_config():
    """The sandbox is the boundary, so OpenCode asks no permission inside it,
    and it neither updates itself nor shares sessions."""
    directory = os.path.join(os.environ.get("HOME", "/sandbox"), ".config", "opencode")
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, "opencode.json"), "w", encoding="utf-8") as config:
        json.dump(
            {
                "permission": {"edit": "allow", "bash": "allow", "webfetch": "deny"},
                "autoupdate": False,
                "share": "disabled",
            },
            config,
        )


class Server:
    def __init__(self, binary, repository):
        self.password = secrets.token_urlsafe(24)
        environment = dict(os.environ, OPENCODE_SERVER_PASSWORD=self.password)
        self.environment = environment
        self.process = subprocess.Popen(
            [binary, "serve", "--port", "0", "--hostname", "127.0.0.1"],
            cwd=repository,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
        )
        self.url = None
        for line in self.process.stdout:
            match = re.search(r"listening on (http://127\.0\.0\.1:\d+)", line)
            if match:
                self.url = match.group(1)
                break
        if not self.url:
            raise RuntimeError("OpenCode server exited before it was listening")
        threading.Thread(target=self._drain, daemon=True).start()
        token = base64.b64encode(f"opencode:{self.password}".encode()).decode()
        self.authorization = f"Basic {token}"

    def _drain(self):
        for _ in self.process.stdout:
            pass

    def _open(self, method, path, body=None, timeout=30):
        request = urllib.request.Request(
            self.url + path,
            method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": self.authorization, "Content-Type": "application/json"},
        )
        return urllib.request.urlopen(request, timeout=timeout)

    def request(self, method, path, body=None):
        with self._open(method, path, body) as response:
            content = response.read()
        return json.loads(content) if content else None

    def stream_events(self, session, sink):
        """Forward this session's server events to `sink` until the stream ends."""
        with self._open("GET", "/event", timeout=None) as response:
            for raw in response:
                line = raw.decode("utf-8", "replace").strip()
                if not line.startswith("data:"):
                    continue
                try:
                    event = json.loads(line[5:])
                except json.JSONDecodeError:
                    continue
                if event.get("properties", {}).get("sessionID") == session:
                    sink.put(event)

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def clarification_text(control):
    return (
        "Assignment clarification only. The accepted goal, Authority Envelope, "
        "Acceptance Criteria, and resource ceilings remain immutable.\n"
        f"Clarification: {control['clarification']}"
    )


def prompt(launch):
    context_strategy = launch["worker_configuration"]["context"]["strategy"]
    context_instruction = {
        "fresh": "Start from only this accepted Assignment context; do not resume prior session context.",
        "fresh_with_retrieval": "Start fresh, use the accepted context below, and retrieve relevant workspace context with the configured tools before acting.",
    }[context_strategy]
    return "\n".join(
        [
            context_instruction,
            "Complete this Tyrion Assignment inside the current workspace.",
            f"Goal: {launch['goal']}",
            f"Acceptance criteria: {json.dumps(launch['criteria'], separators=(',', ':'))}",
            f"Authority Envelope: {json.dumps(launch['authority'], separators=(',', ':'))}",
            f"Declared write scopes: {json.dumps(launch['declared_write_scopes'])}",
            "Do not cause external effects. Respect every scope and resource ceiling.",
            "Finish with a JSON object: a non-empty summary and known_effects as an empty array.",
        ]
    )


def result_summary(text):
    start, end = text.find("{"), text.rfind("}")
    if start != -1 and end > start:
        try:
            value = json.loads(text[start : end + 1])
            if isinstance(value, dict) and isinstance(value.get("summary"), str):
                return value["summary"]
        except json.JSONDecodeError:
            pass
    return text.strip()


def main():
    launch = json.loads(sys.stdin.readline())
    if launch.get("type") != "tyrion.assignment.launch":
        raise RuntimeError("first input must be tyrion.assignment.launch")
    configuration = launch["worker_configuration"]
    context_strategy = configuration.get("context", {}).get("strategy")
    if context_strategy not in {"fresh", "fresh_with_retrieval"}:
        raise RuntimeError(f"unsupported context strategy: {context_strategy}")
    settings = configuration.get("settings", {})
    unknown = sorted(set(settings) - {"variant"})
    if unknown:
        raise RuntimeError(f"unsupported OpenCode settings: {', '.join(unknown)}")
    # This adapter does not deliver native Skills; a required one fails
    # typed, so routing can choose another configuration.
    if launch.get("skill_defaults"):
        raise RequiredSkillFailure(
            launch["skill_defaults"][0], "the OpenCode adapter does not load native Skills"
        )
    provider, _, model = configuration["model"].partition("/")
    if not provider or not model:
        raise RuntimeError("OpenCode models are named provider/model")

    binary = os.environ.get("TYRION_OPENCODE_BINARY", "opencode")
    repository, temporary_root = prepare_workspace()
    write_config()
    server = Server(binary, repository)
    controls = queue.Queue()

    def read_controls():
        for line in sys.stdin:
            controls.put(json.loads(line))

    threading.Thread(target=read_controls, daemon=True).start()
    try:
        session = server.request("POST", "/session", {})["id"]
        emit(
            {
                "type": "tyrion.adapter.ready",
                "native_session_id": session,
                "native_skills": [],
                "native_skill_preparations": [],
                "configuration_fingerprint": os.environ["TYRION_CONFIGURATION_FINGERPRINT"],
            }
        )
        events = queue.Queue()
        threading.Thread(
            target=server.stream_events, args=(session, events), daemon=True
        ).start()
        body = {
            "parts": [{"type": "text", "text": prompt(launch)}],
            "model": {"providerID": provider, "modelID": model},
        }
        if settings.get("variant"):
            body["variant"] = settings["variant"]
        server.request("POST", f"/session/{session}/prompt_async", body)
        emit({"type": "tyrion.opencode.started", "session_id": session})
        interrupted = False
        failed = False
        busy = False
        last_text = ""
        aborted_at = None
        while True:
            try:
                control = controls.get_nowait()
                if control.get("type") == "tyrion.worker.interrupt" and not interrupted:
                    server.request("POST", f"/session/{session}/abort")
                    interrupted = True
                    aborted_at = time.monotonic()
                    emit({"type": "tyrion.opencode.interrupt"})
                elif control.get("type") == "tyrion.worker.steer":
                    server.request(
                        "POST",
                        f"/session/{session}/prompt_async",
                        {
                            "parts": [{"type": "text", "text": clarification_text(control)}],
                            "model": {"providerID": provider, "modelID": model},
                        },
                    )
            except queue.Empty:
                pass
            if aborted_at and time.monotonic() - aborted_at > ABORT_GRACE_SECONDS:
                break
            try:
                event = events.get(timeout=0.05)
            except queue.Empty:
                continue
            kind = event.get("type")
            properties = event.get("properties", {})
            part = properties.get("part", {})
            if kind == "session.status":
                busy = busy or properties.get("status", {}).get("type") == "busy"
            elif kind == "session.idle" and busy:
                break
            elif kind == "session.error":
                error = properties.get("error") or {}
                # Aborting is how an interrupt works; OpenCode reports the
                # aborted message as an error, which is expected, not a failure.
                if interrupted and error.get("name") == "MessageAbortedError":
                    continue
                failed = True
                emit({"type": "error", "error": error})
            elif kind == "message.part.updated" and part.get("type") == "step-start":
                emit({"type": "step_start", "part": part})
            elif kind == "message.part.updated" and part.get("type") == "step-finish":
                emit({"type": "step_finish", "part": part})
            elif kind == "message.part.updated" and part.get("type") == "tool":
                if part.get("state", {}).get("status") in {"completed", "error"}:
                    emit({"type": "tool_use", "part": part})
            elif kind == "message.part.updated" and part.get("type") == "text":
                if not part.get("synthetic") and isinstance(part.get("text"), str):
                    last_text = part["text"]
                    if part.get("time", {}).get("end"):
                        emit({"type": "text", "part": part})
        # The server's own totals are authoritative, however the turn ended.
        input_tokens = output_tokens = 0
        for message in server.request("GET", f"/session/{session}/message") or []:
            info = message.get("info", {})
            tokens = info.get("tokens") or {}
            if info.get("role") != "assistant":
                continue
            cache = tokens.get("cache") or {}
            input_tokens += (tokens.get("input") or 0) + (cache.get("read") or 0) + (cache.get("write") or 0)
            output_tokens += (tokens.get("output") or 0) + (tokens.get("reasoning") or 0)
        emit(
            {
                "type": "tyrion.opencode.usage",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            }
        )
        status = "interrupted" if interrupted else "failed" if failed else "completed"
        emit({"type": "tyrion.opencode.settled", "status": status})
        if status == "completed":
            finish_workspace(repository)
            emit(
                {
                    "type": "tyrion.result",
                    "commission_id": launch["commission_id"],
                    "assignment_id": launch["assignment_id"],
                    "attempt_id": launch["attempt_id"],
                    "mandate_revision": launch["mandate_revision"],
                    "plan_revision": launch["plan_revision"],
                    "summary": result_summary(last_text),
                    "known_effects": [],
                    "cost_cents": 0,
                }
            )
    finally:
        server.close()
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except RequiredSkillFailure as error:
        emit(error.event())
        raise
    except Exception as error:
        emit({"type": "error", "error": {"message": str(error)}})
        raise
