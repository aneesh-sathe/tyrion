#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;
use tyrion::adapter_contract::{validate_trace, AdapterContractExpectation, StructuredAdapterKind};

#[test]
fn production_codex_adapter_uses_app_server_protocol() {
    let temp = TempDir::new().unwrap();
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        r#"#!/usr/bin/env python3
import json, sys
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
        skills = [{"name": "code-review", "path": "/fixture/code-review/SKILL.md", "enabled": True}] if selected_model == "gpt-skill-fixture" else []
        result = {"data": [{"cwd": "/sandbox", "skills": skills, "errors": []}]}
    elif method == "turn/start":
        if selected_model == "gpt-effort-fixture":
            assert request["params"]["effort"] == "xhigh"
        if selected_model == "gpt-git-fixture":
            with open(workspace + "/codex-uncommitted.txt", "w") as output:
                output.write("saved by Codex\n")
        if selected_model == "gpt-skill-fixture":
            assert {"type": "skill", "name": "code-review", "path": "/fixture/code-review/SKILL.md"} in request["params"]["input"]
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

    let mut skill_launch = launch("codex_app_server", "gpt-skill-fixture");
    skill_launch["worker_configuration"]["skills"] = json!(["code-review"]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/codex_app_server.py"),
        skill_launch,
        &[("TYRION_CODEX_BINARY", fake_codex.as_os_str())],
        &[],
        false,
    );
    let required_skills = vec!["code-review".to_owned()];
    let report = validate_production_trace(
        StructuredAdapterKind::CodexAppServer,
        trace,
        &required_skills,
    );
    assert_eq!(report.native_skills, ["code-review"]);

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
class AssistantMessage:
    def __init__(self, model):
        self.model = model
        self.content = [TextBlock("implemented by Claude")]
        self.usage = {"input_tokens": 7, "output_tokens": 4}
        self.session_id = "claude-production-session"
class SystemMessage:
    def __init__(self, options):
        self.subtype = "init"
        self.data = {
            "session_id": "claude-production-session",
            "model": options.model,
            "permissionMode": options.permission_mode,
            "tools": sorted(set(options.tools) | ({"Skill"} if options.skills else set())),
            "skills": options.skills,
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
    async def query(self, prompt): self.prompt = prompt
    async def receive_response(self):
        assert self.options.tools == []
        assert self.options.allowed_tools == []
        assert self.options.max_budget_usd == 1.0
        assert self.options.cli_path == "/fixture/claude"
        if self.options.model == "claude-git-fixture":
            with open(self.options.cwd + "/claude-uncommitted.txt", "w") as output:
                output.write("saved by Claude\n")
        yield SystemMessage(self.options)
        yield AssistantMessage(self.options.model)
        yield ResultMessage(self.options)
    async def interrupt(self): pass
    async def disconnect(self): pass
"#,
    )
    .unwrap();
    let mut claude_launch = launch("claude_agent_sdk", "claude-fixture");
    claude_launch["worker_configuration"]["skills"] = json!(["code-review"]);
    let trace = run_adapter(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/claude_sdk_adapter.py"),
        claude_launch,
        &[
            ("PYTHONPATH", temp.path().as_os_str()),
            (
                "TYRION_CLAUDE_BINARY",
                std::ffi::OsStr::new("/fixture/claude"),
            ),
        ],
        &[],
        true,
    );
    let required_skills = vec!["code-review".to_owned()];
    let report = validate_production_trace(
        StructuredAdapterKind::ClaudeAgentSdk,
        trace,
        &required_skills,
    );
    assert_eq!(report.result_summary, "implemented by Claude");
    assert_eq!(report.input_tokens, 7);
    assert_eq!(report.output_tokens, 4);

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
            "max_model_spend_cents": if kind == "codex_app_server" { 0 } else { 100 },
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
        serde_json::to_writer(&mut input, control).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
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
    mut trace: Vec<Value>,
    required_skills: &[String],
) -> tyrion::adapter_contract::AdapterContractReport {
    trace[0]["containment_enforced"] = json!(true);
    trace[0]["containment_profile"] = json!("production-test-containment");
    validate_trace(
        kind,
        &trace,
        AdapterContractExpectation {
            containment_profile: "production-test-containment",
            required_skills,
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
