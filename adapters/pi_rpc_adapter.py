#!/usr/bin/env python3
"""Production Tyrion adapter for Pi's structured RPC protocol."""

import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading

from native_skill import RequiredSkillFailure, skill_content_digest


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
        prefix="tyrion-pi-",
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
        ["git", "diff", "--cached", "--quiet"], cwd=repository, check=False
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
        raise RuntimeError("could not inspect staged Pi workspace changes")
    git("branch", "-f", "tyrion-result", "HEAD", cwd=repository)
    git("bundle", "create", destination, "refs/heads/tyrion-result", cwd=repository)


def assignment_prompt(launch):
    context_strategy = launch["worker_configuration"]["context"]["strategy"]
    context_instruction = {
        "fresh": "Start from only this accepted Assignment context; do not resume prior session context.",
        "fresh_with_retrieval": "Start fresh, use the accepted context below, and retrieve relevant workspace context with the configured tools and Skills before acting.",
    }[context_strategy]
    return "\n".join(
        [
            context_instruction,
            "Complete this Tyrion Assignment inside the current workspace.",
            f"Goal: {launch['goal']}",
            f"Acceptance criteria: {json.dumps(launch['criteria'], separators=(',', ':'))}",
            f"Authority Envelope: {json.dumps(launch['authority'], separators=(',', ':'))}",
            f"Declared write scopes: {json.dumps(launch['declared_write_scopes'])}",
            f"Worker Context Packet: {json.dumps(launch.get('worker_context_packet'), separators=(',', ':'))}",
            "Do not cause external effects. Respect every scope and resource ceiling.",
            "Return JSON with a non-empty summary and known_effects as an empty array.",
        ]
    )


def clarification_text(control):
    return (
        "Assignment clarification only. The accepted goal, Authority Envelope, "
        "Acceptance Criteria, and resource ceilings remain immutable.\n"
        f"Clarification: {control['clarification']}"
    )


def parse_result(text):
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        raise RuntimeError("Pi Result is not structured JSON") from error
    if not isinstance(value, dict) or set(value) != {"summary", "known_effects"}:
        raise RuntimeError("Pi Result must contain only summary and known_effects")
    summary = value.get("summary")
    if not isinstance(summary, str) or not summary.strip():
        raise RuntimeError("Pi Result summary must be non-empty")
    if value.get("known_effects") != []:
        raise RuntimeError("Pi Result reported effects outside the structured contract")
    return summary.strip()


def native_tools(configured):
    mapping = {
        "git": ["read", "bash", "edit", "write", "grep", "find", "ls"],
        "shell": ["bash"],
        "filesystem": ["read", "edit", "write", "grep", "find", "ls"],
    }
    supported = {"read", "bash", "edit", "write", "grep", "find", "ls"}
    selected = []
    for tool in configured:
        candidates = mapping.get(tool, [tool])
        if any(candidate not in supported for candidate in candidates):
            raise RuntimeError(f"unsupported Pi tool capability: {tool}")
        for candidate in candidates:
            if candidate not in selected:
                selected.append(candidate)
    return selected


def model_identity(configured):
    levels = {"off", "minimal", "low", "medium", "high", "xhigh"}
    base, separator, level = configured.rpartition(":")
    if separator and level in levels:
        return base, level
    return configured, None


class PiRpc:
    def __init__(self, arguments, cwd):
        self.process = subprocess.Popen(
            arguments,
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
            bufsize=1,
        )
        self.messages = queue.Queue()
        self.buffered = []
        self.next_id = 1
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError as error:
                self.messages.put({"type": "extension_error", "error": str(error)})
        self.messages.put(None)

    def send(self, value):
        self.process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def request(self, command, **parameters):
        request_id = str(self.next_id)
        self.next_id += 1
        self.send({"id": request_id, "type": command, **parameters})
        while True:
            message = self.messages.get()
            if message is None:
                raise RuntimeError(f"Pi exited during {command}")
            if message.get("type") == "response" and message.get("id") == request_id:
                if not message.get("success"):
                    raise RuntimeError(f"Pi rejected {command}: {message.get('error')}")
                return message.get("data", {})
            self.buffered.append(message)

    def next_message(self, timeout=0.05):
        if self.buffered:
            return self.buffered.pop(0)
        try:
            return self.messages.get(timeout=timeout)
        except queue.Empty:
            return ...

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def pi_skill_path(repository, name, settings):
    paths = settings.get("native_skill_paths", {})
    if not isinstance(paths, dict):
        raise RuntimeError("Pi native_skill_paths setting must be an object")
    path = paths.get(name)
    if not isinstance(path, str) or not path:
        raise RuntimeError(f"Pi native Skill {name} requires a configured SKILL.md path")
    resolved = os.path.realpath(path if os.path.isabs(path) else os.path.join(repository, path))
    if not os.path.isabs(path):
        repository_root = os.path.realpath(repository)
        if os.path.commonpath([repository_root, resolved]) != repository_root:
            raise RuntimeError(f"Pi native Skill {name} escapes the accepted workspace")
    if not os.path.isfile(resolved) or os.path.basename(resolved) != "SKILL.md":
        raise RuntimeError(f"Pi native Skill {name} requires an available SKILL.md path")
    return resolved


def prepare_skills(repository, selected_skills, configured_versions, settings):
    if len(selected_skills) > 1:
        raise RuntimeError(
            "Pi RPC currently qualifies exactly one pinned native Skill per Assignment"
        )
    preparations = []
    arguments = []
    for skill in selected_skills:
        name = skill["name"]
        if (
            skill.get("delegation") != "native_unchanged"
            or configured_versions.get(name) != skill["content_digest"]
        ):
            raise RequiredSkillFailure(skill, f"Pi selected a different Skill Version for {name}")
        try:
            path = pi_skill_path(repository, name, settings)
        except RuntimeError as error:
            raise RequiredSkillFailure(skill, str(error)) from error
        actual_digest = skill_content_digest(path)
        if actual_digest != skill["content_digest"]:
            raise RequiredSkillFailure(
                skill,
                f"Pi native Skill {name} content changed: expected "
                f"{skill['content_digest']}, actual {actual_digest}",
            )
        arguments.extend(["--skill", path])
        preparations.append(
            {"name": name, "content_digest": skill["content_digest"], "source": path}
        )
    return preparations, arguments


def validate_native_skills(commands, selected_skills, preparations):
    expected_names = [skill["name"] for skill in selected_skills]
    native_commands = [
        command
        for command in commands
        if isinstance(command, dict)
        and isinstance(command.get("name"), str)
        and command["name"].startswith("skill:")
    ]
    native_skills = sorted(
        command["name"].removeprefix("skill:") for command in native_commands
    )
    if native_skills != sorted(expected_names):
        missing = next(
            (skill for skill in selected_skills if skill["name"] not in native_skills),
            None,
        )
        if missing:
            raise RequiredSkillFailure(
                missing, f"Pi did not load required Skill {missing['name']}"
            )
        raise RuntimeError("Pi loaded an unselected native Skill")
    for skill, preparation in zip(selected_skills, preparations):
        command = next(
            item
            for item in native_commands
            if item["name"] == f"skill:{skill['name']}"
        )
        native_path = command.get("path")
        if command.get("source") != "skill" or not isinstance(native_path, str):
            raise RequiredSkillFailure(
                skill, f"Pi reported an invalid native command for Skill {skill['name']}"
            )
        native_path = os.path.realpath(native_path)
        if native_path != preparation["source"]:
            raise RequiredSkillFailure(
                skill, f"Pi resolved required Skill {skill['name']} from a different path"
            )
        try:
            native_digest = skill_content_digest(native_path)
        except RuntimeError as error:
            raise RequiredSkillFailure(skill, str(error)) from error
        if native_digest != skill["content_digest"]:
            raise RequiredSkillFailure(
                skill,
                f"Pi native Skill {skill['name']} content changed after startup: "
                f"expected {skill['content_digest']}, actual {native_digest}",
            )
    return native_skills


def validate_cleared_queue(cleared):
    if not isinstance(cleared, dict):
        raise RuntimeError("Pi clear_queue returned an invalid result")
    for field in ("steering", "followUp"):
        values = cleared.get(field)
        if not isinstance(values, list) or any(not isinstance(value, str) for value in values):
            raise RuntimeError("Pi clear_queue returned an invalid result")


def session_usage(pi, session_id, observed_input, observed_output, observed_cost):
    stats = pi.request("get_session_stats")
    if stats.get("sessionId") != session_id:
        raise RuntimeError("Pi session statistics changed native session identity")
    tokens = stats.get("tokens")
    if not isinstance(tokens, dict):
        raise RuntimeError("Pi session statistics omitted token usage")
    input_tokens = tokens.get("input")
    output_tokens = tokens.get("output")
    if (
        isinstance(input_tokens, bool)
        or not isinstance(input_tokens, int)
        or input_tokens < observed_input
        or isinstance(output_tokens, bool)
        or not isinstance(output_tokens, int)
        or output_tokens < observed_output
    ):
        raise RuntimeError("Pi session statistics conflict with observed token usage")
    cost = stats.get("cost")
    if (
        isinstance(cost, bool)
        or not isinstance(cost, (int, float))
        or cost != 0
        or observed_cost != 0
    ):
        raise RuntimeError("Pi incurred model spend under a zero-spend Worker Configuration")
    return input_tokens, output_tokens


def main():
    launch = json.loads(sys.stdin.readline())
    if launch.get("type") != "tyrion.assignment.launch":
        raise RuntimeError("first input must be tyrion.assignment.launch")
    configuration = launch["worker_configuration"]
    if configuration.get("adapter", {}).get("kind") != "pi_rpc":
        raise RuntimeError("Pi adapter requires adapter kind pi_rpc")
    context_strategy = configuration.get("context", {}).get("strategy")
    if context_strategy not in {"fresh", "fresh_with_retrieval"}:
        raise RuntimeError(f"unsupported context strategy: {context_strategy}")
    resource_limits = launch["resource_limits"]
    if resource_limits["max_model_spend_cents"] != 0:
        raise RuntimeError(
            "Pi RPC has no hard monetary budget control; use an unmetered provider "
            "and reserve zero model-spend cents"
        )
    if resource_limits["max_paid_service_spend_cents"] != 0:
        raise RuntimeError("Pi adapter does not permit paid service spend")
    settings = configuration.get("settings", {})
    supported = {"production_qualified", "native_skill_paths", "thinking"}
    unknown = sorted(set(settings) - supported)
    if unknown:
        raise RuntimeError(f"unsupported Pi settings: {', '.join(unknown)}")
    if settings.get("production_qualified") is not True:
        raise RuntimeError("Pi Worker Configuration is not production qualified")
    configured_model, model_thinking = model_identity(configuration["model"])
    configured_thinking = settings.get("thinking")
    if configured_thinking is not None and configured_thinking not in {
        "off",
        "minimal",
        "low",
        "medium",
        "high",
        "xhigh",
    }:
        raise RuntimeError("Pi thinking setting is invalid")
    if model_thinking is not None and configured_thinking is not None:
        raise RuntimeError("Pi thinking must be configured in the model or settings, not both")

    selected_skills = launch.get("skill_defaults", [])
    configured_versions = {
        skill["name"]: skill["content_digest"]
        for skill in configuration.get("skills", [])
    }
    configured_tools = configuration.get("tools", [])
    if not isinstance(configured_tools, list):
        raise RuntimeError("Pi Worker Configuration tools must be an array")
    tools = native_tools(configured_tools)
    repository, temporary_root = prepare_workspace()
    try:
        preparations, skill_arguments = prepare_skills(
            repository, selected_skills, configured_versions, settings
        )
    except Exception:
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)
        raise
    arguments = [
        os.environ.get("TYRION_PI_BINARY", "pi"),
        "--mode",
        "rpc",
        "--no-session",
        "--no-extensions",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-skills",
        "--model",
        configuration["model"],
    ]
    if settings.get("thinking") is not None:
        arguments.extend(["--thinking", settings["thinking"]])
    if tools:
        arguments.extend(["--tools", ",".join(tools)])
    else:
        arguments.append("--no-tools")
    arguments.extend(skill_arguments)
    try:
        pi = PiRpc(arguments, repository)
    except Exception:
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)
        raise
    controls = queue.Queue()

    def read_controls():
        for line in sys.stdin:
            controls.put(json.loads(line))

    threading.Thread(target=read_controls, daemon=True).start()
    try:
        state = pi.request("get_state")
        model = state.get("model") or {}
        effective_model = model.get("id")
        provider_model = f"{model.get('provider')}/{effective_model}"
        if configured_model not in {effective_model, provider_model}:
            raise RuntimeError("Pi selected a different model")
        expected_thinking = configured_thinking or model_thinking
        if expected_thinking is not None and state.get("thinkingLevel") != expected_thinking:
            raise RuntimeError("Pi selected a different thinking level")
        session_id = state.get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            raise RuntimeError("Pi RPC reported no native session identity")
        commands = pi.request("get_commands").get("commands", [])
        if not isinstance(commands, list):
            raise RuntimeError("Pi returned an invalid native command inventory")
        native_skills = validate_native_skills(commands, selected_skills, preparations)
        emit(
            {
                "type": "tyrion.adapter.ready",
                "native_session_id": session_id,
                "native_skills": native_skills,
                "native_skill_preparations": preparations,
                "configuration_fingerprint": os.environ["TYRION_CONFIGURATION_FINGERPRINT"],
            }
        )
        text = assignment_prompt(launch)
        if selected_skills:
            text = f"/skill:{selected_skills[0]['name']} {text}"
        pi.request("prompt", message=text)
        for preparation in preparations:
            emit({"type": "tyrion.skill.invoked", **preparation})

        terminal = False
        interrupted = False
        last_text = ""
        total_cost = 0.0
        observed_input = 0
        observed_output = 0
        terminal_event = None
        while not terminal:
            try:
                control = controls.get_nowait()
                if control.get("type") == "tyrion.worker.steer":
                    pi.request("steer", message=clarification_text(control))
                elif control.get("type") == "tyrion.worker.interrupt":
                    interrupted = True
                    emit({"type": "tyrion.pi.interrupt"})
                    validate_cleared_queue(pi.request("clear_queue"))
                    pi.request("abort")
            except queue.Empty:
                pass
            message = pi.next_message()
            if message is ...:
                continue
            if message is None:
                raise RuntimeError("Pi exited before a terminal state")
            if message.get("type") == "response":
                continue
            if message.get("type") != "agent_settled":
                emit(message)
            if message.get("type") == "message_end":
                native = message.get("message", {})
                usage = native.get("usage", {})
                total_cost += (usage.get("cost") or {}).get("total", 0)
                if native.get("role") == "assistant":
                    observed_input += usage.get("input", 0)
                    observed_output += usage.get("output", 0)
                    last_text = "\n".join(
                        item["text"]
                        for item in native.get("content", [])
                        if isinstance(item, dict) and isinstance(item.get("text"), str)
                    )
            if message.get("type") == "agent_settled":
                terminal_event = message
                terminal = True
        input_tokens, output_tokens = session_usage(
            pi, session_id, observed_input, observed_output, total_cost
        )
        emit(
            {
                "type": "tyrion.pi.usage",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cost": 0,
            }
        )
        emit(terminal_event)
        if not interrupted:
            summary = parse_result(last_text)
            finish_workspace(repository)
            emit(
                {
                    "type": "tyrion.result",
                    "commission_id": launch["commission_id"],
                    "assignment_id": launch["assignment_id"],
                    "attempt_id": launch["attempt_id"],
                    "mandate_revision": launch["mandate_revision"],
                    "plan_revision": launch["plan_revision"],
                    "summary": summary,
                    "known_effects": [],
                    "cost_cents": 0,
                }
            )
    finally:
        pi.close()
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except RequiredSkillFailure as error:
        emit(error.event())
        raise
    except Exception as error:
        emit({"type": "extension_error", "error": str(error)})
        raise
