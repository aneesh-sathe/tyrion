#!/usr/bin/env python3
"""Production Tyrion adapter for the Claude Agent SDK."""

import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ClaudeSDKClient,
    ResultMessage,
    SystemMessage,
    TextBlock,
)


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
        prefix="tyrion-claude-",
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
        raise RuntimeError("could not inspect staged Claude workspace changes")
    git("branch", "-f", "tyrion-result", "HEAD", cwd=repository)
    git("bundle", "create", destination, "refs/heads/tyrion-result", cwd=repository)


def prompt(launch):
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


def result_summary(message):
    value = message.structured_output
    if isinstance(value, dict) and isinstance(value.get("summary"), str):
        return value["summary"]
    text = message.result or ""
    try:
        value = json.loads(text)
        if isinstance(value, dict) and isinstance(value.get("summary"), str):
            return value["summary"]
    except json.JSONDecodeError:
        pass
    return text.strip()


def native_tools(names):
    mapping = {
        "git": ["Bash", "Read", "Write", "Edit", "Glob", "Grep"],
        "shell": ["Bash"],
        "filesystem": ["Read", "Write", "Edit", "Glob", "Grep"],
    }
    tools = []
    for name in names:
        if name not in mapping:
            raise RuntimeError(f"unsupported Claude tool capability: {name}")
        tools.extend(mapping[name])
    return sorted(set(tools))


def native_init_list(data, field):
    value = data.get(field)
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise RuntimeError(f"Claude SDK init did not report native {field}")
    return sorted(set(value))


async def run():
    launch = json.loads(await asyncio.to_thread(sys.stdin.readline))
    if launch.get("type") != "tyrion.assignment.launch":
        raise RuntimeError("first input must be tyrion.assignment.launch")
    configuration = launch["worker_configuration"]
    resource_limits = launch["resource_limits"]
    context_strategy = configuration.get("context", {}).get("strategy")
    if context_strategy not in {"fresh", "fresh_with_retrieval"}:
        raise RuntimeError(f"unsupported context strategy: {context_strategy}")
    if resource_limits["max_paid_service_spend_cents"] != 0:
        raise RuntimeError("Claude adapter does not permit paid service spend")
    model_budget_usd = resource_limits["max_model_spend_cents"] / 100
    settings = configuration.get("settings", {})
    supported = {"effort", "max_turns"}
    unknown = sorted(set(settings) - supported)
    if unknown:
        raise RuntimeError(f"unsupported Claude settings: {', '.join(unknown)}")
    repository, temporary_root = prepare_workspace()
    input_queue = asyncio.Queue()
    control_lines = asyncio.Queue()
    interrupted = False
    configured_tools = native_tools(configuration.get("tools", []))
    configured_skills = sorted(set(configuration.get("skills", [])))

    async def inputs():
        yield {
            "type": "user",
            "message": {"role": "user", "content": prompt(launch)},
        }
        while True:
            item = await input_queue.get()
            if item is None:
                return
            yield item

    options = ClaudeAgentOptions(
        model=configuration["model"],
        cwd=repository,
        permission_mode="bypassPermissions",
        tools=configured_tools,
        allowed_tools=configured_tools,
        skills=configured_skills,
        setting_sources=["project"],
        effort=settings.get("effort"),
        max_turns=settings.get("max_turns"),
        max_budget_usd=model_budget_usd,
        cli_path=os.environ.get("TYRION_CLAUDE_BINARY"),
        output_format={
            "type": "json_schema",
            "schema": {
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
    client = ClaudeSDKClient(options=options)

    loop = asyncio.get_running_loop()

    def read_controls():
        for line in sys.stdin:
            loop.call_soon_threadsafe(control_lines.put_nowait, line)

    threading.Thread(target=read_controls, daemon=True).start()

    async def controls():
        nonlocal interrupted
        while True:
            line = await control_lines.get()
            control = json.loads(line)
            if control.get("type") == "tyrion.worker.steer":
                await input_queue.put(
                    {
                        "type": "user",
                        "message": {
                            "role": "user",
                            "content": clarification_text(control),
                        },
                    }
                )
            elif control.get("type") == "tyrion.worker.interrupt":
                interrupted = True
                emit({"type": "user.interrupt"})
                await client.interrupt()

    control_task = None
    try:
        await client.connect()
        await client.query(inputs())
        control_task = asyncio.create_task(controls())
        ready = False
        usage_reported = False
        terminal = None
        async for message in client.receive_response():
            session_id = getattr(message, "session_id", None)
            if isinstance(message, SystemMessage):
                session_id = message.data.get("session_id", session_id)
            if not ready and isinstance(message, SystemMessage) and message.subtype == "init":
                if message.data.get("model") != configuration["model"]:
                    raise RuntimeError("Claude Agent SDK selected a different model")
                if message.data.get("permissionMode") != "bypassPermissions":
                    raise RuntimeError("Claude Agent SDK selected a different permission mode")
                effective_skills = native_init_list(message.data, "skills")
                missing_skills = sorted(set(configured_skills) - set(effective_skills))
                if missing_skills:
                    raise RuntimeError(
                        "Claude did not load required Skills: " + ", ".join(missing_skills)
                    )
                effective_tools = native_init_list(message.data, "tools")
                expected_tools = sorted(
                    set(configured_tools)
                    | ({"Skill"} if configured_skills else set())
                )
                if effective_tools != expected_tools:
                    raise RuntimeError(
                        "Claude native tool inventory does not match the selected configuration"
                    )
                if not session_id:
                    raise RuntimeError("Claude SDK init reported no native session identity")
                emit(
                    {
                        "type": "tyrion.adapter.ready",
                        "native_session_id": session_id,
                        "native_skills": effective_skills,
                        "native_skill_outcomes": [
                            {
                                "name": name,
                                "outcome": "activated",
                                "source": "claude-sdk-init",
                            }
                            for name in configured_skills
                        ],
                        "configuration_fingerprint": os.environ[
                            "TYRION_CONFIGURATION_FINGERPRINT"
                        ],
                    }
                )
                emit({"type": "session.status_running"})
                ready = True
            if isinstance(message, AssistantMessage):
                if message.model != configuration["model"]:
                    raise RuntimeError("Claude Agent SDK selected a different model")
                content = [
                    {"type": "text", "text": block.text}
                    for block in message.content
                    if isinstance(block, TextBlock)
                ]
                if content:
                    emit({"type": "agent.message", "content": content})
                if message.usage:
                    emit(
                        {
                            "type": "span.model_request_end",
                            "usage": {
                                "input_tokens": message.usage.get("input_tokens", 0),
                                "output_tokens": message.usage.get("output_tokens", 0),
                            },
                        }
                    )
                    usage_reported = True
            if isinstance(message, ResultMessage):
                if not ready:
                    raise RuntimeError("Claude Agent SDK returned no native session identity")
                if not usage_reported:
                    usage = message.usage or {}
                    emit(
                        {
                            "type": "span.model_request_end",
                            "usage": {
                                "input_tokens": usage.get("input_tokens", 0),
                                "output_tokens": usage.get("output_tokens", 0),
                            },
                        }
                    )
                total_cost_usd = getattr(message, "total_cost_usd", None)
                if not isinstance(total_cost_usd, (int, float)):
                    raise RuntimeError("Claude Agent SDK reported no model spend")
                if total_cost_usd > model_budget_usd + 1e-9:
                    raise RuntimeError("Claude Agent SDK exceeded the reserved model spend")
                aborted = message.terminal_reason in {"aborted_streaming", "aborted_tools"}
                if message.is_error and not aborted:
                    emit({"type": "session.error", "message": message.result or message.subtype})
                    terminal = "failed"
                else:
                    if aborted and not interrupted:
                        interrupted = True
                        emit({"type": "user.interrupt"})
                    emit({"type": "session.status_idle"})
                    terminal = "interrupted" if interrupted else "completed"
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
                            "summary": result_summary(message),
                            "known_effects": [],
                        }
                    )
        if terminal is None:
            raise RuntimeError("Claude Agent SDK emitted no terminal ResultMessage")
    finally:
        await input_queue.put(None)
        if control_task:
            control_task.cancel()
        await client.disconnect()
        if temporary_root:
            shutil.rmtree(temporary_root, ignore_errors=True)


if __name__ == "__main__":
    try:
        asyncio.run(run())
    except Exception as error:
        emit({"type": "session.error", "message": str(error)})
        raise
