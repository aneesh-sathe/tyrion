#!/usr/bin/env python3
"""Production Tyrion adapter for the Codex app-server protocol."""

import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading


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
        prefix="tyrion-codex-",
        dir=os.environ.get("TYRION_WORKSPACE_ROOT", "/sandbox"),
    )
    repository = os.path.join(root, "repository")
    git("clone", "-q", "-b", "tyrion-base", base, repository)
    return repository, root


def finish_workspace(repository):
    destination = os.environ.get("TYRION_CANDIDATE_BUNDLE")
    if not destination:
        return
    git("add", "-A", cwd=repository)
    staged = subprocess.run(
        ["git", "diff", "--cached", "--quiet"],
        cwd=repository,
        check=False,
    )
    if staged.returncode == 1:
        git(
            "-c",
            "user.name=Tyrion Worker",
            "-c",
            "user.email=worker@tyrion.invalid",
            "commit",
            "-qm",
            "feat: save worker result",
            cwd=repository,
        )
    elif staged.returncode != 0:
        raise RuntimeError("could not inspect staged Codex workspace changes")
    git("branch", "-f", "tyrion-result", "HEAD", cwd=repository)
    git("bundle", "create", destination, "refs/heads/tyrion-result", cwd=repository)


class AppServer:
    def __init__(self):
        binary = os.environ.get("TYRION_CODEX_BINARY", "codex")
        self.process = subprocess.Popen(
            [binary, "app-server", "--strict-config"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
            bufsize=1,
        )
        self.messages = queue.Queue()
        self.next_id = 1
        self.buffered = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError as error:
                self.messages.put({"method": "error", "params": {"message": str(error)}})
        self.messages.put(None)

    def send(self, value):
        self.process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self.messages.get()
            if message is None:
                raise RuntimeError(f"Codex app-server exited during {method}")
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(f"Codex app-server rejected {method}: {message['error']}")
                return message.get("result", {})
            if "method" in message and "id" in message:
                self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "error": {
                            "code": -32000,
                            "message": "Tyrion denies interactive requests",
                        },
                    }
                )
                continue
            self.buffered.append(message)

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()

    def take_buffered(self):
        buffered = self.buffered
        self.buffered = []
        return buffered


def text_input(text):
    return [{"type": "text", "text": text, "text_elements": []}]


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
        "fresh_with_retrieval": "Start fresh, use the accepted context below, and retrieve relevant workspace context with the configured tools and Skills before acting.",
    }[context_strategy]
    required_skills = " ".join(f"${name}" for name in launch["worker_configuration"].get("skills", []))
    return "\n".join(
        [
            context_instruction,
            f"Required native Skills: {required_skills}" if required_skills else "No native Skill is required.",
            "Complete this Tyrion Assignment inside the current workspace.",
            f"Goal: {launch['goal']}",
            f"Acceptance criteria: {json.dumps(launch['criteria'], separators=(',', ':'))}",
            f"Authority Envelope: {json.dumps(launch['authority'], separators=(',', ':'))}",
            f"Declared write scopes: {json.dumps(launch['declared_write_scopes'])}",
            "Do not cause external effects. Respect every scope and resource ceiling.",
            "Return JSON with a non-empty summary and known_effects as an empty array.",
        ]
    )


def result_summary(text):
    try:
        value = json.loads(text)
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
    resource_limits = launch["resource_limits"]
    context_strategy = configuration.get("context", {}).get("strategy")
    if context_strategy not in {"fresh", "fresh_with_retrieval"}:
        raise RuntimeError(f"unsupported context strategy: {context_strategy}")
    if resource_limits["max_model_spend_cents"] != 0:
        raise RuntimeError(
            "Codex app-server has no hard monetary budget control; "
            "use an unmetered provider and reserve zero model-spend cents"
        )
    if resource_limits["max_paid_service_spend_cents"] != 0:
        raise RuntimeError("Codex adapter does not permit paid service spend")
    settings = configuration.get("settings", {})
    supported = {"reasoning_effort", "reasoning_summary", "service_tier", "personality"}
    unknown = sorted(set(settings) - supported)
    if unknown:
        raise RuntimeError(f"unsupported Codex settings: {', '.join(unknown)}")
    repository, temporary_root = prepare_workspace()
    app = AppServer()
    controls = queue.Queue()

    def read_controls():
        for line in sys.stdin:
            controls.put(json.loads(line))

    threading.Thread(target=read_controls, daemon=True).start()
    try:
        app.request(
            "initialize",
            {"clientInfo": {"name": "tyrion", "version": "1"}, "capabilities": None},
        )
        app.send({"jsonrpc": "2.0", "method": "initialized"})
        thread = app.request(
            "thread/start",
            {
                "model": configuration["model"],
                "serviceTier": settings.get("service_tier"),
                "cwd": repository,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
                "personality": settings.get("personality"),
                "config": (
                    {"model_reasoning_effort": settings["reasoning_effort"]}
                    if settings.get("reasoning_effort") is not None
                    else None
                ),
                "ephemeral": True,
            },
        )
        if thread.get("model") != configuration["model"]:
            raise RuntimeError("Codex app-server selected a different model")
        if thread.get("approvalPolicy") != "never":
            raise RuntimeError("Codex app-server did not enforce approvalPolicy=never")
        if settings.get("reasoning_effort") is not None and thread.get(
            "reasoningEffort"
        ) != settings.get("reasoning_effort"):
            raise RuntimeError("Codex app-server selected a different reasoning effort")
        if settings.get("service_tier") is not None and thread.get(
            "serviceTier"
        ) != settings.get("service_tier"):
            raise RuntimeError("Codex app-server selected a different service tier")
        thread_id = thread["thread"]["id"]
        skill_response = app.request("skills/list", {"cwds": [repository], "forceReload": True})
        skill_entries = {
            skill["name"]: skill
            for entry in skill_response.get("data", [])
            for skill in entry.get("skills", [])
            if skill.get("enabled") and isinstance(skill.get("name"), str)
        }
        native_skills = sorted(skill_entries)
        missing = sorted(set(configuration.get("skills", [])) - set(native_skills))
        if missing:
            raise RuntimeError(f"Codex did not load required Skills: {', '.join(missing)}")
        skill_inputs = []
        skill_outcomes = []
        for name in configuration.get("skills", []):
            path = skill_entries[name].get("path")
            if not isinstance(path, str) or not os.path.isabs(path):
                raise RuntimeError(f"Codex did not report an absolute path for Skill {name}")
            skill_inputs.append({"type": "skill", "name": name, "path": path})
            skill_outcomes.append(
                {"name": name, "outcome": "activated", "source": path}
            )
        emit(
            {
                "type": "tyrion.adapter.ready",
                "native_session_id": thread_id,
                "native_skills": native_skills,
                "native_skill_outcomes": skill_outcomes,
                "configuration_fingerprint": os.environ["TYRION_CONFIGURATION_FINGERPRINT"],
            }
        )
        app.take_buffered()
        turn = app.request(
            "turn/start",
            {
                "threadId": thread_id,
                "input": text_input(prompt(launch)) + skill_inputs,
                "model": configuration["model"],
                "effort": settings.get("reasoning_effort"),
                "summary": settings.get("reasoning_summary"),
                "outputSchema": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": ["summary", "known_effects"],
                    "properties": {
                        "summary": {"type": "string", "minLength": 1},
                        "known_effects": {"type": "array", "maxItems": 0},
                    },
                },
            },
        )
        turn_id = turn["turn"]["id"]
        last_text = ""
        terminal = None
        pending = app.take_buffered()
        while terminal is None:
            try:
                control = controls.get_nowait()
                if control.get("type") == "tyrion.worker.steer":
                    app.request(
                        "turn/steer",
                        {
                            "threadId": thread_id,
                            "expectedTurnId": turn_id,
                            "input": text_input(clarification_text(control)),
                        },
                    )
                    pending.extend(app.take_buffered())
                elif control.get("type") == "tyrion.worker.interrupt":
                    app.request(
                        "turn/interrupt", {"threadId": thread_id, "turnId": turn_id}
                    )
                    pending.extend(app.take_buffered())
            except queue.Empty:
                pass
            if pending:
                message = pending.pop(0)
            else:
                try:
                    message = app.messages.get(timeout=0.05)
                except queue.Empty:
                    continue
            if message is None:
                raise RuntimeError("Codex app-server exited before a terminal state")
            if "method" in message and "id" in message:
                app.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "error": {"code": -32000, "message": "Tyrion denies interactive requests"},
                    }
                )
                continue
            method = message.get("method")
            if method in {
                "turn/started",
                "item/completed",
                "thread/tokenUsage/updated",
                "turn/completed",
                "error",
            }:
                emit(message)
            if method == "item/completed":
                item = message.get("params", {}).get("item", {})
                if isinstance(item.get("text"), str):
                    last_text = item["text"]
            if method == "turn/completed":
                terminal = message["params"]["turn"]["status"]
            elif method == "error":
                terminal = "failed"
        if terminal == "completed":
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
                }
            )
    finally:
        app.close()
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        emit({"method": "error", "params": {"message": str(error)}})
        raise
