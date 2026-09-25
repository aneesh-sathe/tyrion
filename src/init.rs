//! `tyrion init`: make a machine that has Docker able to run Commissions,
//! without the Principal writing any JSON.
//!
//! Every value tyriond verifies at startup is discovered here rather than
//! typed: the Docker CLI identity and endpoint, the Worker image identity, each
//! harness binary's published checksum, and each harness version, read by
//! running the binary inside the hardened container. A wrong digest makes the
//! daemon refuse to start, which is correct but opaque, so nothing is hand-set.
//!
//! Rerunning is safe. A verified download and a built image are reused, and
//! the configuration is rewritten from what is discovered each time.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::entry_mcp::connect_entry;
use crate::native_entry_launcher::{
    daemon_is_ready, default_data_dir, RUNTIME_CATALOG, RUNTIME_CONFIG,
};
use crate::worker::CODEX_VERSION;
use crate::{NativeHarness, TyrionError};

const DOCKERFILE: &str = include_str!("../runtime/docker/Dockerfile");
const NATIVE_SKILL: &str = include_str!("../adapters/native_skill.py");
const CLAUDE_ADAPTER: &str = include_str!("../adapters/claude_sdk_adapter.py");
const CODEX_ADAPTER: &str = include_str!("../adapters/codex_app_server.py");

const CLAUDE_RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";
const CODEX_RELEASES: &str = "https://github.com/openai/codex/releases/download";
/// Names a Claude Worker may authenticate with, forwarded by name only.
const CLAUDE_CREDENTIALS: [&str; 2] = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"];

pub struct InitOptions {
    /// A Claude Code release channel (`stable`, `latest`) or exact version.
    pub claude_version: String,
    pub claude_model: String,
    pub codex_model: String,
}

pub fn run_init(options: &InitOptions) -> Result<(), TyrionError> {
    let data_dir = default_data_dir()?;
    let runtime_dir = data_dir.join("runtime");
    private_dir(&data_dir)?;
    private_dir(&runtime_dir)?;
    println!("Setting up Tyrion in {}", display(&data_dir));

    let docker = Docker::discover()?;
    report(
        1,
        "docker",
        &format!("{} ({})", docker.version, docker.platform.docker_arch),
    );

    let (image_id, built) = docker.worker_image()?;
    report(
        2,
        "worker image",
        &format!(
            "{} ({})",
            short(&image_id),
            if built { "built" } else { "reused" }
        ),
    );

    let harness_dir = runtime_dir.join("harnesses");
    private_dir(&harness_dir)?;
    let claude = fetch_claude(&harness_dir, docker.platform, &options.claude_version)?;
    let codex = fetch_codex(&harness_dir, docker.platform)?;

    let probe = Probe::start(&docker, &image_id)?;
    probe.check_adapter_runtime()?;
    let claude_version = probe.version(&claude.binary, "claude")?;
    report(
        3,
        "claude code",
        &format!("{claude_version} {}", claude.note),
    );
    let codex_version = probe.version(&codex.binary, "codex")?;
    if codex_version != CODEX_VERSION {
        return Err(next_action(
            &format!("the downloaded Codex reports {codex_version}, but this Tyrion pins {CODEX_VERSION}"),
            "reinstall Tyrion, or delete the Codex download and rerun `tyrion init`",
        ));
    }
    report(4, "codex", &format!("{codex_version} {}", codex.note));
    probe.finish()?;

    let adapters = runtime_dir.join("adapters");
    private_dir(&adapters)?;
    let claude_adapter = write_private(&adapters.join("claude_sdk_adapter.py"), CLAUDE_ADAPTER)?;
    let codex_adapter = write_private(&adapters.join("codex_app_server.py"), CODEX_ADAPTER)?;
    // The daemon launches adapters directly, so they must be executable.
    for adapter in [&claude_adapter, &codex_adapter] {
        fs::set_permissions(adapter, fs::Permissions::from_mode(0o700))?;
    }

    let claude_credentials: Vec<&str> = CLAUDE_CREDENTIALS
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        .collect();
    let codex_auth = home()?.join(".codex/auth.json");
    let codex_auth = codex_auth.is_file().then_some(codex_auth);

    let mut destinations = Vec::new();
    let mut configurations = Vec::new();
    if !claude_credentials.is_empty() {
        destinations.push(json!({"host": "api.anthropic.com", "port": 443}));
        configurations.push(configuration(
            "claude-default",
            "claude",
            "claude_agent_sdk",
            &claude_adapter,
            &options.claude_model,
            json!({}),
            2,
        )?);
    }
    if codex_auth.is_some() {
        destinations.push(json!({"host": "chatgpt.com", "port": 443}));
        destinations.push(json!({"host": "auth.openai.com", "port": 443}));
        configurations.push(configuration(
            "codex-default",
            "codex",
            "codex_app_server",
            &codex_adapter,
            &options.codex_model,
            json!({"reasoning_effort": "low"}),
            1,
        )?);
    }

    let mut runtime = json!({
        "docker_binary": docker.binary,
        "docker_sha256": sha256_file(&docker.binary)?,
        "docker_version": docker.version,
        "docker_host": docker.host,
        // A locally built image has no registry digest, only an id. Both are
        // content addressed; a tag is not, and Tyrion rejects one.
        "worker_image": image_id,
        "worker_image_id": image_id,
        "codex_binary": codex.binary,
        "codex_sha256": sha256_file(&codex.binary)?,
        "codex_version": codex_version,
        "codex_code_mode_host": {
            "path": codex.code_mode_host,
            "sha256": sha256_file(&codex.code_mode_host)?,
        },
        "claude": {
            "binary": claude.binary,
            "version": claude_version,
            "sha256": sha256_file(&claude.binary)?,
        },
        "model": options.codex_model,
        "egress": {"destinations": destinations},
        "worker_credentials": claude_credentials,
        "lease_ttl_seconds": 900,
        "vcpus": 2,
        "memory_mib": 6144,
        "writable_storage_mib": 4096,
        "max_processes": 256,
    });
    if let Some(auth) = &codex_auth {
        runtime["codex_auth_file"] = json!(auth);
    }
    let config_path = runtime_dir.join(RUNTIME_CONFIG);
    let catalog_path = runtime_dir.join(RUNTIME_CATALOG);
    write_private(&config_path, &serde_json::to_string_pretty(&runtime)?)?;
    if configurations.is_empty() {
        remove_if_present(&catalog_path)?;
    } else {
        write_private(
            &catalog_path,
            &serde_json::to_string_pretty(&json!({"configurations": configurations}))?,
        )?;
    }
    report(5, "configuration", &display(&config_path));

    let elapsed = check_daemon(&data_dir, &config_path, &catalog_path)?;
    report(
        6,
        "daemon",
        &format!(
            "started on this runtime, Entry Session attached ({:.1}s)",
            elapsed.as_secs_f64()
        ),
    );

    println!();
    println!(
        "  claude workers  {}",
        if claude_credentials.is_empty() {
            "off: run `claude setup-token`, export CLAUDE_CODE_OAUTH_TOKEN, then rerun `tyrion init`"
                .to_owned()
        } else {
            format!("on, authenticated by {}", claude_credentials.join(", "))
        }
    );
    println!(
        "  codex workers   {}",
        if codex_auth.is_some() {
            "on, authenticated by ~/.codex/auth.json"
        } else {
            "off: run `codex login`, then rerun `tyrion init`"
        }
    );
    println!("\nTyrion is ready. From any Git repository, run `tyrion claude` or `tyrion codex`.");
    Ok(())
}

fn report(step: u8, name: &str, detail: &str) {
    if step == 1 {
        println!();
    }
    println!("  {step}/6  {name:<17} {detail}");
}

fn display(path: &Path) -> String {
    match home()
        .ok()
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

fn short(image_id: &str) -> &str {
    &image_id[..image_id.len().min(19)]
}

/// Every failure names what the Principal should do next.
fn next_action(problem: &str, action: &str) -> TyrionError {
    TyrionError::InvalidRequest(format!("{problem}\n  next: {action}"))
}

#[derive(Clone, Copy)]
struct Platform {
    docker_arch: &'static str,
    claude: &'static str,
    codex: &'static str,
}

impl Platform {
    fn from_docker(arch: &str) -> Option<Self> {
        match arch {
            "arm64" | "aarch64" => Some(Self {
                docker_arch: "linux/arm64",
                claude: "linux-arm64",
                codex: "aarch64-unknown-linux-musl",
            }),
            "amd64" | "x86_64" => Some(Self {
                docker_arch: "linux/amd64",
                claude: "linux-x64",
                codex: "x86_64-unknown-linux-musl",
            }),
            _ => None,
        }
    }
}

struct Docker {
    binary: PathBuf,
    version: String,
    /// The endpoint is resolved once here and pinned, because the daemon
    /// never resolves an ambient Docker context.
    host: String,
    platform: Platform,
}

impl Docker {
    fn discover() -> Result<Self, TyrionError> {
        let binary = find_docker().ok_or_else(|| {
            next_action(
                "Docker is not installed",
                "install Docker Desktop (https://docker.com/products/docker-desktop) or `brew install colima docker`, then rerun `tyrion init`",
            )
        })?;
        let version = text(run(Command::new(&binary).arg("--version"))?);
        let host = text(run(Command::new(&binary).args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]))?);
        let docker = |args: &[&str]| {
            let mut command = Command::new(&binary);
            command.arg("--host").arg(&host).args(args);
            command
        };
        let arch = run(&mut docker(&["version", "--format", "{{.Server.Arch}}"]))
            .map(text)
            .map_err(|_| {
                next_action(
                    &format!("Docker is installed but not reachable at {host}"),
                    "start Docker Desktop, or run `colima start`, then rerun `tyrion init`",
                )
            })?;
        let platform = Platform::from_docker(&arch).ok_or_else(|| {
            next_action(
                &format!("Docker reports an unsupported architecture: {arch}"),
                "use a Docker engine running linux/arm64 or linux/amd64",
            )
        })?;
        Ok(Self {
            binary,
            version,
            host,
            platform,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("--host").arg(&self.host);
        command
    }

    /// The image tag is derived from its build inputs, so a Tyrion upgrade that
    /// changes them builds a new image rather than silently reusing a stale one.
    fn worker_image(&self) -> Result<(String, bool), TyrionError> {
        let digest = Sha256::digest([DOCKERFILE, NATIVE_SKILL].concat());
        let tag = format!("tyrion-worker:{}", &format!("{digest:x}")[..12]);
        if let Some(id) = self.image_id(&tag) {
            return Ok((id, false));
        }
        println!("       building the Worker image, a few minutes the first time");
        let context = std::env::temp_dir().join(format!("tyrion-image-{}", Uuid::new_v4()));
        fs::create_dir_all(context.join("adapters"))?;
        fs::write(context.join("Dockerfile"), DOCKERFILE)?;
        fs::write(context.join("adapters/native_skill.py"), NATIVE_SKILL)?;
        let built = self
            .command()
            .args(["build", "--quiet", "--tag", &tag])
            .arg(&context)
            .output();
        let _ = fs::remove_dir_all(&context);
        let built = built?;
        if !built.status.success() {
            return Err(next_action(
                &format!("the Worker image failed to build:\n{}", tail(&built.stderr)),
                "check that Docker can reach the internet (Debian and PyPI), then rerun `tyrion init`",
            ));
        }
        let id = self.image_id(&tag).ok_or_else(|| {
            TyrionError::InvalidRequest(format!("built {tag} but Docker cannot inspect it"))
        })?;
        Ok((id, true))
    }

    fn image_id(&self, tag: &str) -> Option<String> {
        run(self
            .command()
            .args(["image", "inspect", tag, "--format", "{{.Id}}"]))
        .ok()
        .map(text)
    }
}

fn find_docker() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("docker"));
    let known = [
        "/opt/homebrew/bin/docker",
        "/usr/local/bin/docker",
        "/Applications/Docker.app/Contents/Resources/bin/docker",
    ]
    .into_iter()
    .map(PathBuf::from);
    on_path
        .chain(known)
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
}

struct Claude {
    binary: PathBuf,
    note: &'static str,
}

/// Claude Code publishes a manifest of per-platform SHA-256 checksums for each
/// release; that manifest is the trust root for the download.
fn fetch_claude(dir: &Path, platform: Platform, requested: &str) -> Result<Claude, TyrionError> {
    let version = if requested.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        requested.to_owned()
    } else {
        curl_text(&format!("{CLAUDE_RELEASES}/{requested}"))?
    };
    let binary = dir.join(format!("claude-{version}-{}", platform.claude));
    let manifest: Value = serde_json::from_str(&curl_text(&format!(
        "{CLAUDE_RELEASES}/{version}/manifest.json"
    ))?)?;
    let expected = manifest["platforms"][platform.claude]["checksum"]
        .as_str()
        .ok_or_else(|| {
            next_action(
                &format!(
                    "Claude Code {version} publishes no {} build",
                    platform.claude
                ),
                "rerun with `tyrion init --claude-version stable`",
            )
        })?;
    if binary.is_file() && sha256_file(&binary)? == expected {
        return Ok(Claude {
            binary,
            note: "(cached, checksum verified)",
        });
    }
    download_verified(
        &format!("{CLAUDE_RELEASES}/{version}/{}/claude", platform.claude),
        &binary,
        expected,
    )?;
    Ok(Claude {
        binary,
        note: "(downloaded, checksum verified)",
    })
}

struct Codex {
    binary: PathBuf,
    code_mode_host: PathBuf,
    note: &'static str,
}

/// Codex publishes SHA-256 sums for its package archive, which carries both the
/// harness and the companion code-mode host it needs to edit files. The
/// version is Tyrion's pin, not the latest release.
fn fetch_codex(dir: &Path, platform: Platform) -> Result<Codex, TyrionError> {
    let version = CODEX_VERSION.trim_start_matches("codex-cli ");
    let home = dir.join(format!("codex-{version}-{}", platform.codex));
    let codex = Codex {
        binary: home.join("codex"),
        code_mode_host: home.join("codex-code-mode-host"),
        note: "(cached, checksum verified)",
    };
    // The archive is discarded after extraction, so a rerun re-verifies the
    // binaries against the sums recorded when the archive itself was verified.
    let recorded = home.join("SHA256SUMS");
    if let Ok(sums) = fs::read_to_string(&recorded) {
        let current = format!(
            "{}\n{}\n",
            sha256_file(&codex.binary).unwrap_or_default(),
            sha256_file(&codex.code_mode_host).unwrap_or_default()
        );
        if sums == current {
            return Ok(codex);
        }
    }
    let release = format!("{CODEX_RELEASES}/rust-v{version}");
    let archive_name = format!("codex-package-{}.tar.gz", platform.codex);
    let sums = curl_text(&format!("{release}/codex-package_SHA256SUMS"))?;
    let expected = published_checksum(&sums, &archive_name).ok_or_else(|| {
        TyrionError::InvalidRequest(format!(
            "Codex {version} publishes no checksum for {archive_name}"
        ))
    })?;
    let _ = fs::remove_dir_all(&home);
    private_dir(&home)?;
    let archive = home.join(&archive_name);
    download_verified(&format!("{release}/{archive_name}"), &archive, expected)?;
    let extracted = run(Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&home)
        .args(["bin/codex", "bin/codex-code-mode-host"]));
    fs::remove_file(&archive)?;
    extracted?;
    fs::rename(home.join("bin/codex"), &codex.binary)?;
    fs::rename(home.join("bin/codex-code-mode-host"), &codex.code_mode_host)?;
    fs::remove_dir(home.join("bin"))?;
    for binary in [&codex.binary, &codex.code_mode_host] {
        fs::set_permissions(binary, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(
        &recorded,
        format!(
            "{}\n{}\n",
            sha256_file(&codex.binary)?,
            sha256_file(&codex.code_mode_host)?
        ),
    )?;
    Ok(Codex {
        note: "(downloaded, checksum verified)",
        ..codex
    })
}

/// Find one file's digest in a `sha256sum`-format listing. Only a well-formed
/// 64-digit hex digest counts, so a truncated or tampered listing fails.
fn published_checksum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| *name == file)
        .map(|(digest, _)| digest)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn download_verified(url: &str, destination: &Path, expected: &str) -> Result<(), TyrionError> {
    let partial = destination.with_extension("partial");
    println!("       downloading {url}");
    let fetched = run(Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "2",
        ])
        .arg("--output")
        .arg(&partial)
        .arg(url));
    if let Err(error) = fetched {
        let _ = fs::remove_file(&partial);
        return Err(next_action(
            &format!("download failed: {error}"),
            "check your network connection, then rerun `tyrion init`",
        ));
    }
    let actual = sha256_file(&partial)?;
    if actual != expected {
        let _ = fs::remove_file(&partial);
        return Err(next_action(
            &format!(
                "{url} does not match its published checksum (expected {expected}, got {actual})"
            ),
            "rerun `tyrion init`; if it persists, do not use this download and report it",
        ));
    }
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o700))?;
    fs::rename(&partial, destination)?;
    Ok(())
}

/// One disposable container under the exact Worker profile. Each harness is
/// streamed in and asked its version there, which is also the first proof it
/// runs under the boundary Tyrion will hold it to. A guest-only Linux binary
/// cannot report its version on the host.
struct Probe<'a> {
    docker: &'a Docker,
    name: String,
    finished: bool,
}

impl<'a> Probe<'a> {
    fn start(docker: &'a Docker, image_id: &str) -> Result<Self, TyrionError> {
        let name = format!("tyrion-init-{}", Uuid::new_v4());
        run(docker.command().args([
            "run",
            "--detach",
            "--name",
            &name,
            "--label",
            "tyrion.init=probe",
            "--network",
            "none",
            "--read-only",
            "--pids-limit",
            "256",
            "--memory",
            "6144m",
            "--memory-swap",
            "6144m",
            "--cpus",
            "2",
            "--cpuset-cpus",
            "0-1",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--security-opt",
            "seccomp=builtin",
            "--user",
            "65534:65534",
            "--tmpfs",
            "/sandbox:rw,exec,nosuid,nodev,size=1024m,mode=1777",
            "--env",
            "HOME=/sandbox",
            "--env",
            "PYTHONPATH=/opt/tyrion",
            image_id,
            "sleep",
            "600",
        ]))
        .map_err(|error| {
            next_action(
                &format!("the hardened Worker profile cannot start on this Docker engine: {error}"),
                "give Docker at least 2 CPUs and 8 GB of memory (Docker Desktop: Settings > Resources), then rerun `tyrion init`",
            )
        })?;
        Ok(Self {
            docker,
            name,
            finished: false,
        })
    }

    /// The adapters import these from the image rather than receiving them
    /// per Attempt, so a broken image fails here instead of mid-Commission.
    fn check_adapter_runtime(&self) -> Result<(), TyrionError> {
        run(self.docker.command().args([
            "exec",
            &self.name,
            "python3",
            "-c",
            "import native_skill, claude_agent_sdk",
        ]))
        .map(drop)
        .map_err(|error| {
            TyrionError::InvalidRequest(format!(
                "the Worker image cannot import its adapter runtime: {error}"
            ))
        })
    }

    fn version(&self, binary: &Path, label: &str) -> Result<String, TyrionError> {
        let target = format!("/sandbox/{label}");
        // `docker cp` writes beneath a tmpfs mount on Docker Desktop, so stream.
        let mut upload = self
            .docker
            .command()
            .args(["exec", "--interactive", &self.name, "sh", "-c"])
            .arg(format!("cat > {target} && chmod 700 {target}"))
            .stdin(File::open(binary)?)
            .output()?;
        if upload.status.success() {
            upload = self
                .docker
                .command()
                .args(["exec", &self.name, &target, "--version"])
                .output()?;
        }
        let _ = self
            .docker
            .command()
            .args(["exec", &self.name, "rm", "-f", &target])
            .output();
        if !upload.status.success() {
            return Err(next_action(
                &format!(
                    "{label} did not run inside the Worker container: {}",
                    tail(&upload.stderr)
                ),
                "delete the downloads in the runtime/harnesses directory and rerun `tyrion init`",
            ));
        }
        Ok(String::from_utf8_lossy(&upload.stdout)
            .lines()
            .last()
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    /// Remove the container and confirm its absence independently. `docker
    /// exec` is never a liveness probe because it restarts a stopped container.
    fn finish(mut self) -> Result<(), TyrionError> {
        self.finished = true;
        self.remove();
        if run(self.docker.command().args(["inspect", &self.name])).is_ok() {
            return Err(TyrionError::InvalidRequest(format!(
                "probe container {} survived removal; remove it with `docker rm -f {}`",
                self.name, self.name
            )));
        }
        Ok(())
    }

    fn remove(&self) {
        let _ = self
            .docker
            .command()
            .args(["rm", "--force", &self.name])
            .output();
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.remove();
        }
    }
}

fn configuration(
    id: &str,
    harness: &str,
    kind: &str,
    adapter: &Path,
    model: &str,
    settings: Value,
    cost_cents: u64,
) -> Result<Value, TyrionError> {
    Ok(json!({
        "id": id,
        "harness": harness,
        "adapter": {
            "kind": kind,
            "version": "1.0.0",
            "command": [adapter],
            "sha256": sha256_file(adapter)?,
        },
        "model": model,
        "settings": settings,
        "tools": ["git"],
        "skills": [],
        "context": {"strategy": "fresh", "capacity_tokens": 200000},
        "resource_limits": {
            "max_concurrency_slots": 2,
            "max_storage_bytes": 10485760,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0,
        },
        "capabilities": [
            "structured_lifecycle", "semantic_interrupt", "terminal_state",
            "usage", "skills", "result_submission", "contained",
        ],
        "authority_actions": ["codex.git_change"],
        "authority_scope_types": ["repository", "path", "action"],
        "assignment_constraints": ["coding"],
        "containment_profile": "docker-hardened-v1",
        "replacement_class": "coding",
        "available": true,
        "metrics": {
            "expected_verified_correctness": 9000,
            "preference_adherence": 9000,
            "first_pass_acceptance": 9000,
            "commission_elapsed_time_contribution_ms": 1000,
            "cost_cents": cost_cents,
            "continuity": 0,
        },
    }))
}

/// Start a throwaway daemon on the generated configuration and attach an
/// Entry Session exactly as `tyrion claude` does. Startup validates every pin,
/// so this proves the configuration a real Commission will run on. It spends
/// no model tokens: the deterministic Worker is deliberately absent from a
/// configured daemon, so the first real Commission is the Principal's own.
fn check_daemon(data_dir: &Path, config: &Path, catalog: &Path) -> Result<Duration, TyrionError> {
    let check_dir = data_dir.join("init-check");
    let _ = fs::remove_dir_all(&check_dir);
    private_dir(&check_dir)?;
    let socket = check_dir.join("tyrion.sock");
    let log = check_dir.join("tyriond.log");
    let mut command = Command::new(std::env::current_exe()?.with_file_name("tyriond"));
    command
        .arg("--data-dir")
        .arg(&check_dir)
        .arg("--socket")
        .arg(&socket)
        .arg("--codex-worker-config")
        .arg(config);
    if catalog.is_file() {
        command.arg("--worker-catalog").arg(catalog);
    }
    let started = Instant::now();
    let mut daemon = DaemonGuard(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log)?)
            .spawn()?,
    );
    let result = wait_and_attach(&socket, &mut daemon).map_err(|error| {
        let log = fs::read_to_string(&log).unwrap_or_default();
        next_action(
            &format!("{error}\n  daemon log:\n{}", tail(log.as_bytes())),
            "rerun `tyrion init`; if it persists, report this message",
        )
    });
    drop(daemon);
    let _ = fs::remove_dir_all(&check_dir);
    result.map(|()| started.elapsed())
}

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_and_attach(socket: &Path, daemon: &mut DaemonGuard) -> Result<(), TyrionError> {
    // Startup hashes every pinned binary before it answers, and the socket
    // binds before it does, so wait for an answer rather than for the file.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !daemon_is_ready(socket) {
        if let Some(status) = daemon.0.try_wait()? {
            return Err(TyrionError::InvalidRequest(format!(
                "tyriond rejected the generated configuration ({status})"
            )));
        }
        if Instant::now() >= deadline {
            return Err(TyrionError::InvalidRequest(
                "tyriond did not become ready within 120 seconds".into(),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
    connect_entry(socket, NativeHarness::Claude).map(drop)
}

fn private_dir(path: &Path) -> Result<(), TyrionError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TyrionError::InvalidRequest(format!(
            "{} must be a directory you own",
            path.display()
        )));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private(path: &Path, content: &str) -> Result<PathBuf, TyrionError> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(path.to_path_buf())
}

fn remove_if_present(path: &Path) -> Result<(), TyrionError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn sha256_file(path: &Path) -> Result<String, TyrionError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn home() -> Result<PathBuf, TyrionError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| TyrionError::InvalidRequest("HOME is not set".into()))
}

fn curl_text(url: &str) -> Result<String, TyrionError> {
    run(Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "2",
        ])
        .arg(url))
    .map(text)
    .map_err(|error| {
        next_action(
            &format!("could not fetch {url}: {error}"),
            "check your network connection, then rerun `tyrion init`",
        )
    })
}

fn run(command: &mut Command) -> Result<Output, TyrionError> {
    let output = command.stdin(Stdio::null()).output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(TyrionError::InvalidRequest(tail(&output.stderr)))
    }
}

fn text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_architectures_map_to_published_linux_builds() {
        let arm = Platform::from_docker("arm64").unwrap();
        assert_eq!(
            (arm.claude, arm.codex),
            ("linux-arm64", "aarch64-unknown-linux-musl")
        );
        let x86 = Platform::from_docker("amd64").unwrap();
        assert_eq!(
            (x86.claude, x86.codex),
            ("linux-x64", "x86_64-unknown-linux-musl")
        );
        assert!(Platform::from_docker("riscv64").is_none());
    }

    #[test]
    fn published_checksum_matches_the_exact_file_only() {
        let digest = "a".repeat(64);
        let sums = format!(
            "{digest}  codex-package-aarch64-unknown-linux-musl.tar.gz\n\
             {}  codex-app-server-package-aarch64-unknown-linux-musl.tar.gz\n\
             abc123  codex-package-x86_64-unknown-linux-musl.tar.gz\n",
            "b".repeat(64)
        );
        assert_eq!(
            published_checksum(&sums, "codex-package-aarch64-unknown-linux-musl.tar.gz"),
            Some(digest.as_str())
        );
        assert_eq!(published_checksum(&sums, "codex-package.tar.gz"), None);
        assert_eq!(
            published_checksum(&sums, "codex-package-x86_64-unknown-linux-musl.tar.gz"),
            None,
            "a malformed digest must not be trusted"
        );
    }
}
