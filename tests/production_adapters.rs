#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tyrion::adapter_contract::{validate_trace, AdapterContractExpectation, StructuredAdapterKind};
use tyrion::protocol::SkillVersion;

#[test]
fn production_codex_adapter_uses_app_server_protocol() {
    let temp = TempDir::new().unwrap();
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        r#"#!/usr/bin/env python3
import json, os, sys
if sys.argv[1:] != ["app-server", "--strict-config"]:
    raise RuntimeError(f"unexpected Codex arguments: {sys.argv[1:]}")
selected_model = None
workspace = None
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialized":
        continue
    if method == "initialize":
        result = {"serverInfo": {"name": "fake", "version": "1"}}
    elif method == "thread/start":
        selected_model = request["params"]["model"]
        workspace = request["params"]["cwd"]
        reasoning_effort = (request["params"].get("config") or {}).get("model_reasoning_effort", "medium")
        result = {"thread": {"id": "codex-production-thread"}, "model": request["params"]["model"], "reasoningEffort": reasoning_effort, "approvalPolicy": "never"}
    elif method == "skills/list":
        skills = [{"name": "code-review", "path": os.environ["TYRION_SKILL_PATH"], "enabled": True}] if selected_model == "gpt-skill-fixture" else []
        result = {"data": [{"cwd": "/sandbox", "skills": skills, "errors": []}]}
    elif method == "turn/start":
        if selected_model == "gpt-effort-fixture":
            assert request["params"]["effort"] == "xhigh"
        if selected_model == "gpt-git-fixture":
            with open(workspace + "/codex-uncommitted.txt", "w") as output:
                output.write("saved by Codex\n")
        if selected_model == "gpt-skill-fixture":
            assert {"type": "skill", "name": "code-review", "path": os.environ["TYRION_SKILL_PATH"]} in request["params"]["input"]
        print(json.dumps({"method": "turn/started", "params": {"turn": {"id": "turn-1", "status": "inProgress"}}}), flush=True)
        result = {"turn": {"id": "turn-1", "status": "inProgress"}}
        print(json.dumps({"id": request["id"], "result": result}), flush=True)
        print(json.dumps({"method": "item/completed", "params": {"item": {"text": "{\"summary\":\"implemented by Codex\",\"known_effects\":[]}"}}}), flush=True)
        if request["params"]["model"] == "gpt-control-fixture":
            continue
        print(json.dumps({"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 8, "outputTokens": 3}}}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}}), flush=True)
        continue
    elif method == "turn/steer":
        print(json.dumps({"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 9, "outputTokens": 3}}}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}}), flush=True)
        result = {}
    else:
        continue
    print(json.dumps({"id": request["id"], "result": result}), flush=True)
"#,
    );
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        launch("codex_app_server", "gpt-fixture"),
        &[("TYRION_CODEX_BINARY", fake_codex.as_os_str())],
        &[],
        false,
    );
    let report = validate_production_trace(StructuredAdapterKind::CodexAppServer, trace, &[]);
    assert_eq!(report.result_summary, "implemented by Codex");
    assert_eq!(report.input_tokens, 8);
    assert_eq!(report.output_tokens, 3);

    let mut effort_launch = launch("codex_app_server", "gpt-effort-fixture");
    effort_launch["worker_configuration"]["settings"] = json!({"reasoning_effort": "xhigh"});
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        effort_launch,
        &[("TYRION_CODEX_BINARY", fake_codex.as_os_str())],
        &[],
        false,
    );
    validate_production_trace(StructuredAdapterKind::CodexAppServer, trace, &[]);

    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        launch("codex_app_server", "gpt-control-fixture"),
        &[("TYRION_CODEX_BINARY", fake_codex.as_os_str())],
        &[json!({
            "type": "tyrion.worker.steer",
            "command_id": "steer-race",
            "mandate_revision": 1,
            "clarification": "Finish the accepted assignment."
        })],
        false,
    );
    let report = validate_production_trace(StructuredAdapterKind::CodexAppServer, trace, &[]);
    assert_eq!(report.terminal_state, "completed");
    assert_eq!(report.input_tokens, 9);

    let skill_root = temp.path().join("codex-native-skill");
    fs::create_dir(&skill_root).unwrap();
    let skill_path = skill_root.join("SKILL.md");
    fs::write(&skill_path, "# Code review\n").unwrap();
    let skill_digest = skill_content_digest(&skill_root);
    let mut skill_launch = launch("codex_app_server", "gpt-skill-fixture");
    skill_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest
    }]);
    skill_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        skill_launch.clone(),
        &[
            ("TYRION_CODEX_BINARY", fake_codex.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
        ],
        &[],
        false,
    );
    let required_skills = vec![SkillVersion {
        name: "code-review".to_owned(),
        content_digest: skill_digest.clone(),
    }];
    let report = validate_production_trace(
        StructuredAdapterKind::CodexAppServer,
        trace,
        &required_skills,
    );
    assert_eq!(report.native_skills, ["code-review"]);
    fs::write(&skill_path, "# Changed code review\n").unwrap();
    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        skill_launch,
        &[
            ("TYRION_CODEX_BINARY", fake_codex.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("required_skill_failure"));

    let git = git_bundle_fixture(temp.path(), "codex");
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        launch("codex_app_server", "gpt-git-fixture"),
        &[
            ("TYRION_CODEX_BINARY", fake_codex.as_os_str()),
            ("TYRION_BASE_BUNDLE", git.base_bundle.as_os_str()),
            ("TYRION_CANDIDATE_BUNDLE", git.candidate_bundle.as_os_str()),
            ("TYRION_WORKSPACE_ROOT", git.workspace_root.as_os_str()),
        ],
        &[],
        false,
    );
    validate_production_trace(StructuredAdapterKind::CodexAppServer, trace, &[]);
    assert_candidate_contains(
        &git.candidate_bundle,
        "codex-uncommitted.txt",
        "saved by Codex\n",
    );

    let mut unsupported_context = launch("codex_app_server", "gpt-fixture");
    unsupported_context["worker_configuration"]["context"]["strategy"] = json!("resume");
    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        unsupported_context,
        &[("TYRION_CODEX_BINARY", fake_codex.as_os_str())],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported context strategy"));
}

#[test]
fn production_claude_adapter_uses_agent_sdk_messages() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join("claude_agent_sdk.py"),
        r#"class ClaudeAgentOptions:
    def __init__(self, **kwargs): self.__dict__.update(kwargs)
class TextBlock:
    def __init__(self, text): self.text = text
class ToolUseBlock:
    def __init__(self, name, input):
        self.name = name
        self.input = input
class AssistantMessage:
    def __init__(self, options):
        self.model = options.model
        self.content = []
        if options.model == "claude-fixture":
            self.content.append(ToolUseBlock("Skill", {"skill": "code-review"}))
        elif options.model == "claude-worker-skill-fixture":
            self.content.append(ToolUseBlock("Skill", {"skill": "frontend"}))
        self.content.append(TextBlock("implemented by Claude"))
        self.usage = {"input_tokens": 7, "output_tokens": 4}
        self.session_id = "claude-production-session"
class SystemMessage:
    def __init__(self, options):
        self.subtype = "init"
        self.data = {
            "session_id": "claude-production-session",
            "model": options.model,
            "permissionMode": options.permission_mode,
            "tools": options.tools,
            "skills": ["code-review", "frontend"],
            "slash_commands": ["code-review", "frontend"],
        }
class ResultMessage:
    def __init__(self, options):
        self.session_id = "claude-production-session"
        self.structured_output = {"summary": "implemented by Claude", "known_effects": []}
        self.result = None
        self.usage = {"input_tokens": 7, "output_tokens": 4}
        self.is_error = False
        self.terminal_reason = "completed"
        self.subtype = "success"
        self.total_cost_usd = 2.0 if options.model == "claude-over-budget" else 0.25
class ClaudeSDKClient:
    def __init__(self, options): self.options = options
    async def connect(self): pass
    async def query(self, prompt): self.prompt = await prompt.__anext__()
    async def receive_response(self):
        assert self.options.tools == ["Skill"]
        assert self.options.allowed_tools == ["Skill"]
        assert self.options.skills == "all"
        assert not hasattr(self.options, "setting_sources")
        assert self.options.max_budget_usd == 1.0
        assert self.options.cli_path == "/fixture/claude"
        if self.options.model in {"claude-fixture", "claude-skill-refusal"}:
            assert "native Skill tool: code-review" in self.prompt["message"]["content"]
        if self.options.model == "claude-git-fixture":
            with open(self.options.cwd + "/claude-uncommitted.txt", "w") as output:
                output.write("saved by Claude\n")
        yield SystemMessage(self.options)
        yield AssistantMessage(self.options)
        yield ResultMessage(self.options)
    async def interrupt(self): pass
    async def disconnect(self): pass
"#,
    )
    .unwrap();
    let claude_workspace = temp.path().join("claude-skill-workspace");
    let isolated_claude_config = temp.path().join("isolated-claude-config");
    fs::create_dir(&isolated_claude_config).unwrap();
    let skill_root = claude_workspace.join(".claude/skills/code-review");
    fs::create_dir_all(&skill_root).unwrap();
    fs::write(skill_root.join("SKILL.md"), "# Code review\n").unwrap();
    let skill_digest = skill_content_digest(&skill_root);
    let mut claude_launch = launch("claude_agent_sdk", "claude-fixture");
    claude_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest
    }]);
    claude_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        claude_launch.clone(),
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_WORKSPACE_ROOT", claude_workspace.as_os_str()),
            ("CLAUDE_CONFIG_DIR", isolated_claude_config.as_os_str()),
        ],
        &[],
        true,
    );
    let required_skills = vec![SkillVersion {
        name: "code-review".to_owned(),
        content_digest: skill_digest.clone(),
    }];
    let report = validate_production_trace(
        StructuredAdapterKind::ClaudeAgentSdk,
        trace,
        &required_skills,
    );
    assert_eq!(report.result_summary, "implemented by Claude");
    assert_eq!(report.input_tokens, 7);
    assert_eq!(report.output_tokens, 4);
    assert_eq!(report.cost_cents, 25);

    let mut refusal_launch = claude_launch.clone();
    refusal_launch["worker_configuration"]["model"] = json!("claude-skill-refusal");
    let refusal_trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        refusal_launch,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_WORKSPACE_ROOT", claude_workspace.as_os_str()),
            ("CLAUDE_CONFIG_DIR", isolated_claude_config.as_os_str()),
        ],
        &[],
        true,
    );
    assert!(refusal_trace
        .iter()
        .all(|event| event["type"] != "tyrion.skill.invoked"));

    let frontend_skill_root = claude_workspace.join(".claude/skills/frontend");
    fs::create_dir_all(&frontend_skill_root).unwrap();
    fs::write(frontend_skill_root.join("SKILL.md"), "# Frontend\n").unwrap();
    let frontend_digest = skill_content_digest(&frontend_skill_root);
    let mut worker_selected_launch = launch("claude_agent_sdk", "claude-worker-skill-fixture");
    worker_selected_launch["worker_configuration"]["skills"] = json!([{
        "name": "frontend",
        "content_digest": frontend_digest
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        worker_selected_launch,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_WORKSPACE_ROOT", claude_workspace.as_os_str()),
            ("CLAUDE_CONFIG_DIR", isolated_claude_config.as_os_str()),
        ],
        &[],
        true,
    );
    let allowed_skills = [SkillVersion {
        name: "frontend".into(),
        content_digest: frontend_digest.clone(),
    }];
    let report = validate_production_trace_with_allowed(
        StructuredAdapterKind::ClaudeAgentSdk,
        trace,
        &[],
        &allowed_skills,
    );
    assert_eq!(
        report.skill_versions,
        [SkillVersion {
            name: "frontend".into(),
            content_digest: frontend_digest,
        }]
    );
    let claude_config = temp.path().join("claude-config");
    let personal_skill_root = claude_config.join("skills/code-review");
    fs::create_dir_all(&personal_skill_root).unwrap();
    fs::write(
        personal_skill_root.join("SKILL.md"),
        "# Personal code review\n",
    )
    .unwrap();
    let personal_digest = skill_content_digest(&personal_skill_root);
    let mut personal_launch = launch("claude_agent_sdk", "claude-fixture");
    personal_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": personal_digest
    }]);
    personal_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": personal_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        personal_launch,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_WORKSPACE_ROOT", claude_workspace.as_os_str()),
            ("CLAUDE_CONFIG_DIR", claude_config.as_os_str()),
        ],
        &[],
        true,
    );
    validate_production_trace(
        StructuredAdapterKind::ClaudeAgentSdk,
        trace,
        &[SkillVersion {
            name: "code-review".into(),
            content_digest: personal_digest,
        }],
    );
    fs::write(skill_root.join("SKILL.md"), "# Changed code review\n").unwrap();
    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        claude_launch,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_WORKSPACE_ROOT", claude_workspace.as_os_str()),
            ("CLAUDE_CONFIG_DIR", isolated_claude_config.as_os_str()),
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("required_skill_failure"));

    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        launch("claude_agent_sdk", "claude-over-budget"),
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeded the reserved model spend"));

    let git = git_bundle_fixture(temp.path(), "claude");
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        launch("claude_agent_sdk", "claude-git-fixture"),
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
            ("TYRION_BASE_BUNDLE", git.base_bundle.as_os_str()),
            ("TYRION_CANDIDATE_BUNDLE", git.candidate_bundle.as_os_str()),
            ("TYRION_WORKSPACE_ROOT", git.workspace_root.as_os_str()),
        ],
        &[],
        true,
    );
    validate_production_trace(StructuredAdapterKind::ClaudeAgentSdk, trace, &[]);
    assert_candidate_contains(
        &git.candidate_bundle,
        "claude-uncommitted.txt",
        "saved by Claude\n",
    );

    let mut unsupported_context = launch("claude_agent_sdk", "claude-fixture");
    unsupported_context["worker_configuration"]["context"]["strategy"] = json!("resume");
    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        unsupported_context,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported context strategy"));
}

#[test]
fn production_pi_adapter_uses_rpc_lifecycle_and_native_skills() {
    let temp = TempDir::new().unwrap();
    let skill_root = temp.path().join("pi-native-skill");
    fs::create_dir(&skill_root).unwrap();
    let skill_path = skill_root.join("SKILL.md");
    fs::write(&skill_path, "# Code review\n").unwrap();
    let skill_digest = skill_content_digest(&skill_root);
    let wrong_skill_root = temp.path().join("pi-wrong-native-skill");
    fs::create_dir(&wrong_skill_root).unwrap();
    let wrong_skill_path = wrong_skill_root.join("SKILL.md");
    fs::write(&wrong_skill_path, "# Code review\n").unwrap();
    let fake_pi = write_executable(
        &temp.path().join("pi"),
        r#"#!/usr/bin/env python3
import json, os, sys
model = sys.argv[sys.argv.index("--model") + 1]
configured_skill = os.path.realpath(os.environ["TYRION_SKILL_PATH"])
git_skill = os.path.realpath(os.getcwd() + "/.pi/skills/code-review/SKILL.md")
if model in {"pi-fixture", "pi-wrong-skill-path"}:
    tail = ["--tools", "read,bash", "--skill", configured_skill]
elif model == "pi-git":
    tail = ["--no-tools", "--skill", git_skill]
else:
    tail = ["--no-tools"]
expected = ["--mode", "rpc", "--no-session", "--no-extensions", "--no-prompt-templates", "--no-context-files", "--no-skills", "--model", model, *tail]
if sys.argv[1:] != expected:
    raise RuntimeError(f"unexpected Pi arguments: {sys.argv[1:]}")
queued_steer = False
for line in sys.stdin:
    request = json.loads(line)
    kind = request["type"]
    if kind == "get_state":
        data = {"model": {"id": model, "provider": "fixture"}, "sessionId": "pi-production-session"}
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True, "data": data}), flush=True)
    elif kind == "get_commands":
        if model == "pi-wrong-skill-path":
            path = os.environ["TYRION_WRONG_SKILL_PATH"]
        elif model == "pi-git":
            path = git_skill
        else:
            path = configured_skill
        commands = [{"name": "skill:code-review", "source": "skill", "path": path}] if model in {"pi-fixture", "pi-wrong-skill-path", "pi-git"} else []
        data = {"commands": commands}
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True, "data": data}), flush=True)
    elif kind == "prompt":
        if model in {"pi-fixture", "pi-git"}:
            assert request["message"].startswith("/skill:code-review ")
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True}), flush=True)
        print(json.dumps({"type": "agent_start"}), flush=True)
        if model == "pi-git":
            with open(os.getcwd() + "/pi-uncommitted.txt", "w") as output:
                output.write("saved by Pi\n")
            result = json.dumps({"summary": "implemented Pi Git change", "known_effects": []}, separators=(",", ":"))
            print(json.dumps({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": result}], "usage": {"input": 17, "output": 7, "cost": {"total": 0}}}}), flush=True)
            print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind == "steer":
        assert "Finish the accepted assignment" in request["message"]
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True}), flush=True)
        if model == "pi-fixture":
            result = json.dumps({"summary": "implemented by Pi", "known_effects": []}, separators=(",", ":"))
            print(json.dumps({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": result}], "usage": {"input": 13, "output": 5, "cost": {"total": 0}}}}), flush=True)
            print(json.dumps({"type": "agent_settled"}), flush=True)
        else:
            queued_steer = True
    elif kind == "clear_queue":
        cleared = ["queued steer"] if queued_steer else []
        queued_steer = False
        data = {"steering": cleared, "followUp": []}
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True, "data": data}), flush=True)
    elif kind == "abort":
        assert not queued_steer, "abort would continue queued steering"
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True}), flush=True)
        print(json.dumps({"type": "message_end", "message": {"role": "assistant", "content": [], "usage": {"input": 2, "output": 0, "cost": {"total": 0}}}}), flush=True)
        print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind == "get_session_stats":
        usage = {
            "pi-fixture": (31, 11),
            "pi-git": (17, 7),
            "pi-interrupt": (2, 0),
        }.get(model, (0, 0))
        data = {"sessionId": "pi-production-session", "tokens": {"input": usage[0], "output": usage[1], "cacheRead": 0, "cacheWrite": 0, "total": sum(usage)}, "cost": 0}
        print(json.dumps({"id": request["id"], "type": "response", "command": kind, "success": True, "data": data}), flush=True)
"#,
    );
    let mut pi_launch = launch("pi_rpc", "pi-fixture");
    pi_launch["worker_configuration"]["settings"] = json!({
        "production_qualified": true,
        "native_skill_paths": {"code-review": skill_path}
    });
    pi_launch["worker_configuration"]["tools"] = json!(["read", "bash"]);
    pi_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest
    }]);
    pi_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/pi_rpc_adapter.py"),
        pi_launch,
        &[
            ("TYRION_PI_BINARY", fake_pi.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
            ("TYRION_WORKSPACE_ROOT", temp.path().as_os_str()),
        ],
        &[json!({
            "type": "tyrion.worker.steer",
            "command_id": "pi-steer",
            "mandate_revision": 1,
            "clarification": "Finish the accepted assignment."
        })],
        false,
    );
    let required_skills = vec![SkillVersion {
        name: "code-review".to_owned(),
        content_digest: skill_digest.clone(),
    }];
    let report = validate_production_trace(StructuredAdapterKind::PiRpc, trace, &required_skills);
    assert_eq!(report.native_session_id, "pi-production-session");
    assert_eq!(report.result_summary, "implemented by Pi");
    assert_eq!(report.input_tokens, 31);
    assert_eq!(report.output_tokens, 11);

    let mut wrong_path_launch = launch("pi_rpc", "pi-wrong-skill-path");
    wrong_path_launch["worker_configuration"]["settings"] = json!({
        "production_qualified": true,
        "native_skill_paths": {"code-review": skill_path}
    });
    wrong_path_launch["worker_configuration"]["tools"] = json!(["read", "bash"]);
    wrong_path_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest
    }]);
    wrong_path_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": skill_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let output = run_adapter_output(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/pi_rpc_adapter.py"),
        wrong_path_launch,
        &[
            ("TYRION_PI_BINARY", fake_pi.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
            ("TYRION_WRONG_SKILL_PATH", wrong_skill_path.as_os_str()),
            ("TYRION_WORKSPACE_ROOT", temp.path().as_os_str()),
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("required_skill_failure"));

    let mut interrupt_launch = launch("pi_rpc", "pi-interrupt");
    interrupt_launch["worker_configuration"]["settings"] = json!({"production_qualified": true});
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/pi_rpc_adapter.py"),
        interrupt_launch,
        &[
            ("TYRION_PI_BINARY", fake_pi.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
            ("TYRION_WORKSPACE_ROOT", temp.path().as_os_str()),
        ],
        &[
            json!({
                "type": "tyrion.worker.steer",
                "command_id": "pi-steer-before-interrupt",
                "mandate_revision": 1,
                "clarification": "Finish the accepted assignment."
            }),
            json!({
                "type": "tyrion.worker.interrupt",
                "command_id": "pi-interrupt",
                "mandate_revision": 1,
                "reason": "Stop the accepted Assignment."
            }),
        ],
        false,
    );
    let report = validate_production_trace(StructuredAdapterKind::PiRpc, trace, &[]);
    assert_eq!(report.terminal_state, "interrupted");

    let git_fixture = git_bundle_fixture(temp.path(), "pi");
    let git_repository = temp.path().join("pi-base-repository");
    let git_skill_root = git_repository.join(".pi/skills/code-review");
    fs::create_dir_all(&git_skill_root).unwrap();
    fs::write(git_skill_root.join("SKILL.md"), "# Contained code review\n").unwrap();
    let git_skill_digest = skill_content_digest(&git_skill_root);
    git(&git_repository, &["add", ".pi/skills/code-review/SKILL.md"]);
    git(
        &git_repository,
        &["commit", "--amend", "-qm", "test: add skill"],
    );
    git(&git_repository, &["branch", "-f", "tyrion-base", "HEAD"]);
    fs::remove_file(&git_fixture.base_bundle).unwrap();
    git(
        &git_repository,
        &[
            "bundle",
            "create",
            git_fixture.base_bundle.to_str().unwrap(),
            "refs/heads/tyrion-base",
        ],
    );
    let mut git_launch = launch("pi_rpc", "pi-git");
    git_launch["worker_configuration"]["settings"] = json!({
        "production_qualified": true,
        "native_skill_paths": {"code-review": ".pi/skills/code-review/SKILL.md"}
    });
    git_launch["worker_configuration"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": git_skill_digest
    }]);
    git_launch["skill_defaults"] = json!([{
        "name": "code-review",
        "content_digest": git_skill_digest,
        "requirement": "required",
        "provenance": "principal",
        "delegation": "native_unchanged"
    }]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/pi_rpc_adapter.py"),
        git_launch,
        &[
            ("TYRION_PI_BINARY", fake_pi.as_os_str()),
            ("TYRION_SKILL_PATH", skill_path.as_os_str()),
            ("TYRION_BASE_BUNDLE", git_fixture.base_bundle.as_os_str()),
            (
                "TYRION_CANDIDATE_BUNDLE",
                git_fixture.candidate_bundle.as_os_str(),
            ),
            (
                "TYRION_WORKSPACE_ROOT",
                git_fixture.workspace_root.as_os_str(),
            ),
        ],
        &[],
        false,
    );
    let required_git_skills = [SkillVersion {
        name: "code-review".to_owned(),
        content_digest: git_skill_digest,
    }];
    let report =
        validate_production_trace(StructuredAdapterKind::PiRpc, trace, &required_git_skills);
    assert_eq!(report.result_summary, "implemented Pi Git change");
    assert_candidate_contains(
        &git_fixture.candidate_bundle,
        "pi-uncommitted.txt",
        "saved by Pi\n",
    );
}

fn launch(kind: &str, model: &str) -> Value {
    json!({
        "type": "tyrion.assignment.launch",
        "commission_id": "commission-production",
        "assignment_id": "assignment-production",
        "attempt_id": "attempt-production",
        "mandate_revision": 1,
        "plan_revision": 1,
        "goal": "implement the production adapter fixture",
        "execution": {"kind": "deterministic"},
        "criteria": [],
        "authority": {
            "repositories": [],
            "paths": [],
            "actions": ["deterministic.echo"],
            "destinations": [],
            "effects": []
        },
        "authorized_paths": [],
        "declared_write_scopes": [],
        "worker_configuration": {
            "adapter": {"kind": kind},
            "model": model,
            "settings": {},
            "tools": [],
            "skills": [],
            "context": {"strategy": "fresh", "capacity_tokens": 100000}
        },
        "resource_limits": {
            "max_storage_bytes": 1048576,
            "max_model_spend_cents": if matches!(kind, "codex_app_server" | "pi_rpc") { 0 } else { 100 },
            "max_paid_service_spend_cents": 0
        }
    })
}

fn run_adapter(
    adapter: PathBuf,
    launch: Value,
    environment: &[(&str, &std::ffi::OsStr)],
    controls: &[Value],
    keep_input_open: bool,
) -> Vec<Value> {
    let started = Instant::now();
    let output = run_adapter_output(adapter, launch, environment, controls, keep_input_open);
    if keep_input_open {
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "adapter waited for its still-open control input"
        );
    }
    assert!(
        output.status.success(),
        "production adapter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn run_adapter_output(
    adapter: PathBuf,
    launch: Value,
    environment: &[(&str, &std::ffi::OsStr)],
    controls: &[Value],
    keep_input_open: bool,
) -> std::process::Output {
    let mut command = Command::new("python3");
    command
        .arg(adapter)
        .env("TYRION_CONFIGURATION_FINGERPRINT", "production-fingerprint")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    serde_json::to_writer(&mut input, &launch).unwrap();
    input.write_all(b"\n").unwrap();
    for control in controls {
        thread::sleep(Duration::from_millis(100));
        if serde_json::to_writer(&mut input, control).is_err()
            || input.write_all(b"\n").is_err()
            || input.flush().is_err()
        {
            break;
        }
    }
    if keep_input_open {
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            drop(input);
        });
    } else {
        drop(input);
    }
    child.wait_with_output().unwrap()
}

fn validate_production_trace(
    kind: StructuredAdapterKind,
    trace: Vec<Value>,
    required_skills: &[SkillVersion],
) -> tyrion::adapter_contract::AdapterContractReport {
    validate_production_trace_with_allowed(kind, trace, required_skills, required_skills)
}

fn validate_production_trace_with_allowed(
    kind: StructuredAdapterKind,
    mut trace: Vec<Value>,
    required_skills: &[SkillVersion],
    allowed_skills: &[SkillVersion],
) -> tyrion::adapter_contract::AdapterContractReport {
    trace[0]["containment_enforced"] = json!(true);
    trace[0]["containment_profile"] = json!("production-test-containment");
    validate_trace(
        kind,
        &trace,
        AdapterContractExpectation {
            configuration_id: "production-configuration",
            containment_profile: "production-test-containment",
            expected_skills: required_skills,
            allowed_skills,
            commission_id: "commission-production",
            assignment_id: "assignment-production",
            attempt_id: "attempt-production",
            configuration_fingerprint: "production-fingerprint",
            mandate_revision: 1,
            plan_revision: 1,
        },
    )
    .unwrap()
}

fn write_executable(path: &Path, contents: &str) -> PathBuf {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    path.to_owned()
}

fn skill_content_digest(root: &Path) -> String {
    fn collect(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                collect(root, &path, files);
            } else {
                assert!(metadata.is_file());
                files.push(path.strip_prefix(root).unwrap().to_owned());
            }
        }
    }

    let mut files = Vec::new();
    collect(root, root, &mut files);
    let mut digest = Sha256::new();
    for relative in files {
        let path = root.join(&relative);
        let metadata = fs::metadata(&path).unwrap();
        let content = fs::read(&path).unwrap();
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(if metadata.permissions().mode() & 0o111 == 0 {
            b"0"
        } else {
            b"1"
        });
        digest.update([0]);
        digest.update((content.len() as u64).to_be_bytes());
        digest.update(content);
    }
    format!("sha256:{:x}", digest.finalize())
}

struct GitBundleFixture {
    base_bundle: PathBuf,
    candidate_bundle: PathBuf,
    workspace_root: PathBuf,
}

fn git_bundle_fixture(root: &Path, name: &str) -> GitBundleFixture {
    let repository = root.join(format!("{name}-base-repository"));
    fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "-q"]);
    git(&repository, &["config", "user.name", "Tyrion Test"]);
    git(
        &repository,
        &["config", "user.email", "tyrion@example.invalid"],
    );
    fs::write(repository.join("README.md"), "base\n").unwrap();
    git(&repository, &["add", "README.md"]);
    git(&repository, &["commit", "-qm", "test: create base"]);
    git(&repository, &["branch", "tyrion-base"]);
    let base_bundle = root.join(format!("{name}-base.bundle"));
    git(
        &repository,
        &[
            "bundle",
            "create",
            base_bundle.to_str().unwrap(),
            "refs/heads/tyrion-base",
        ],
    );
    let workspace_root = root.join(format!("{name}-workspace"));
    fs::create_dir(&workspace_root).unwrap();
    GitBundleFixture {
        base_bundle,
        candidate_bundle: root.join(format!("{name}-candidate.bundle")),
        workspace_root,
    }
}

fn assert_candidate_contains(bundle: &Path, path: &str, expected: &str) {
    let checkout = bundle.with_extension("checkout");
    let output = Command::new("git")
        .args(["clone", "-q", "-b", "tyrion-result"])
        .arg(bundle)
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "candidate clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(checkout.join(path)).unwrap(), expected);
    let commit_count = Command::new("git")
        .args(["-C"])
        .arg(&checkout)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(commit_count.stdout).unwrap().trim(), "2");
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
