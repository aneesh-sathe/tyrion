//! `tyrion uninstall`: remove everything `tyrion init` and Tyrion's jobs made,
//! after naming any finished work that exists only in Tyrion's data folder.
//!
//! The program itself belongs to whoever installed it (Homebrew or Cargo), so
//! this removes the data folder and Docker leftovers, then says how to remove
//! the program.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::init::find_docker;
use crate::native_entry_launcher::{default_data_dir, RUNTIME_CONFIG};
use crate::TyrionError;

pub struct UninstallOptions {
    /// Skip the typed confirmation.
    pub yes: bool,
}

/// Finished work whose result is not on any branch of its own repository.
struct Unmerged {
    goal: String,
    repository: PathBuf,
    integration: PathBuf,
}

pub fn run_uninstall(options: &UninstallOptions) -> Result<(), TyrionError> {
    let data_dir = default_data_dir()?;
    let present = data_dir.is_dir();
    // Holding the daemon's own lock proves nothing is running, and keeps a new
    // session from starting while its folder is being deleted.
    let _lock = if present {
        Some(hold_lock(&data_dir)?)
    } else {
        None
    };
    let docker = Docker::find(&data_dir);
    let leftovers = docker.as_ref().map(Docker::leftovers);
    let unmerged = if present {
        unmerged_results(&data_dir)?
    } else {
        Vec::new()
    };

    println!("Tyrion will remove:\n");
    if present {
        println!(
            "  data folder   {} ({}: harness downloads, job history and results)",
            data_dir.display(),
            human_size(folder_size(&data_dir))
        );
    }
    match &leftovers {
        Some(found) => println!(
            "  Docker        {} Worker image{}, {} container{}, {} network{}",
            found.images.len(),
            plural(found.images.len()),
            found.containers.len(),
            plural(found.containers.len()),
            found.networks.len(),
            plural(found.networks.len()),
        ),
        None => println!("  Docker        not reachable, so Docker leftovers stay (see below)"),
    }
    let nothing = !present && leftovers.as_ref().is_none_or(Leftovers::is_empty);
    if nothing {
        println!("\nNothing of Tyrion's is left on this machine.");
        print_program_removal();
        return Ok(());
    }
    if !present {
        // No data folder, so there are no results to speak of.
    } else if unmerged.is_empty() {
        println!("\nEvery finished result is already merged into its project.");
    } else {
        println!("\nThese finished results are not merged anywhere, and will be lost:\n");
        for result in &unmerged {
            println!("  {}", result.goal);
            println!(
                "    keep it first:  git -C {} fetch {} tyrion-integration:tyrion-result",
                result.repository.display(),
                result.integration.display()
            );
        }
    }

    if !options.yes && !confirmed()? {
        return Err(TyrionError::InvalidRequest(
            "nothing was removed, because `delete` was not typed".into(),
        ));
    }

    if let (Some(docker), Some(found)) = (&docker, &leftovers) {
        docker.remove(found);
    }
    if present {
        fs::remove_dir_all(&data_dir)?;
    }
    println!("\nRemoved Tyrion's data and Docker leftovers.");
    if docker.is_none() {
        println!("Docker was not reachable. Once it is, remove what is left with:");
        println!("  docker ps -aq --filter label=tyrion.attempt | xargs docker rm -f");
        println!("  docker network ls -q --filter label=tyrion.attempt | xargs docker network rm");
        println!("  docker images -q tyrion-worker | xargs docker rmi");
    }
    print_program_removal();
    Ok(())
}

fn print_program_removal() {
    println!("\nFinally, remove the program itself:");
    println!("  brew uninstall tyrion && brew untap aneesh-sathe/tyrion");
    println!("  (or, if you installed it with Cargo: cargo uninstall tyrion)");
}

fn hold_lock(data_dir: &Path) -> Result<File, TyrionError> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("control-plane.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(TyrionError::InvalidRequest(
            "Tyrion is still running. Close every `tyrion claude` and `tyrion codex` session (or stop `tyriond`), then run `tyrion uninstall` again".into(),
        )),
        Err(error) => Err(TyrionError::Io(error)),
    }
}

fn confirmed() -> Result<bool, TyrionError> {
    print!("\nType `delete` to remove all of this: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    let read = io::stdin().lock().read_line(&mut answer)?;
    if read == 0 && !io::stdin().is_terminal() {
        return Err(TyrionError::InvalidRequest(
            "no confirmation was given; pass --yes to uninstall without asking".into(),
        ));
    }
    Ok(answer.trim() == "delete")
}

/// Results Tyrion integrated that no branch of their repository contains. A
/// result a user merged fast-forward keeps its commit, so it is found; one
/// merged some other way looks unmerged, which errs on the side of warning.
fn unmerged_results(data_dir: &Path) -> Result<Vec<Unmerged>, TyrionError> {
    let integrations = data_dir.join("integrations");
    let Ok(entries) = fs::read_dir(&integrations) else {
        return Ok(Vec::new());
    };
    let state = Connection::open_with_flags(
        data_dir.join("state.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok();
    let mut unmerged = Vec::new();
    for entry in entries.flatten() {
        let commission_id = entry.file_name().to_string_lossy().into_owned();
        let integration = entry.path().join("repository");
        let Some(result) = git_output(
            &integration,
            &["rev-parse", "--verify", "-q", "tyrion-integration^{commit}"],
        ) else {
            continue;
        };
        let (goal, execution) = state
            .as_ref()
            .and_then(|state| {
                state
                    .query_row(
                        "SELECT goal, execution_json FROM commissions WHERE id = ?1",
                        [&commission_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| (format!("Commission {commission_id}"), "{}".into()));
        let execution: Value = serde_json::from_str(&execution).unwrap_or(Value::Null);
        if execution["base_revision"].as_str() == Some(result.as_str()) {
            continue; // nothing was ever integrated
        }
        let repository = PathBuf::from(execution["repository"].as_str().unwrap_or_default());
        let merged = git_output(&repository, &["branch", "--all", "--contains", &result])
            .is_some_and(|branches| !branches.is_empty());
        if !merged {
            unmerged.push(Unmerged {
                goal,
                repository,
                integration,
            });
        }
    }
    Ok(unmerged)
}

fn git_output(repository: &Path, arguments: &[&str]) -> Option<String> {
    if !repository.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

struct Docker {
    binary: PathBuf,
    host: Option<String>,
}

struct Leftovers {
    containers: Vec<String>,
    networks: Vec<String>,
    images: Vec<String>,
}

impl Leftovers {
    fn is_empty(&self) -> bool {
        self.containers.is_empty() && self.networks.is_empty() && self.images.is_empty()
    }
}

impl Docker {
    /// The Docker Tyrion was set up with, or else the one on this machine.
    fn find(data_dir: &Path) -> Option<Self> {
        let pinned = fs::read(data_dir.join("runtime").join(RUNTIME_CONFIG))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
        let docker = match &pinned {
            Some(config) => Self {
                binary: PathBuf::from(config["docker_binary"].as_str()?),
                host: config["docker_host"].as_str().map(str::to_owned),
            },
            None => Self {
                binary: find_docker()?,
                host: None,
            },
        };
        docker
            .lines(&["info", "--format", "{{.ID}}"])
            .map(|_| docker)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        if let Some(host) = &self.host {
            command.env("DOCKER_HOST", host);
        }
        command
    }

    fn lines(&self, arguments: &[&str]) -> Option<Vec<String>> {
        let output = self.command().args(arguments).output().ok()?;
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
    }

    fn leftovers(&self) -> Leftovers {
        let mut containers = Vec::new();
        for label in ["label=tyrion.attempt", "label=tyrion.init"] {
            containers.extend(
                self.lines(&["ps", "-aq", "--filter", label])
                    .unwrap_or_default(),
            );
        }
        containers.sort();
        containers.dedup();
        Leftovers {
            containers,
            networks: self
                .lines(&["network", "ls", "-q", "--filter", "label=tyrion.attempt"])
                .unwrap_or_default(),
            images: self
                .lines(&[
                    "images",
                    "--format",
                    "{{.Repository}}:{{.Tag}}",
                    "tyrion-worker",
                ])
                .unwrap_or_default(),
        }
    }

    fn remove(&self, found: &Leftovers) {
        if !found.containers.is_empty() {
            let _ = self
                .command()
                .args(["rm", "-f"])
                .args(&found.containers)
                .output();
        }
        if !found.networks.is_empty() {
            let _ = self
                .command()
                .args(["network", "rm"])
                .args(&found.networks)
                .output();
        }
        if !found.images.is_empty() {
            let _ = self.command().arg("rmi").args(&found.images).output();
        }
    }
}

fn folder_size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| folder_size(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

fn human_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mib = bytes as f64 / MIB;
    if mib >= 1024.0 {
        format!("{:.1} GB", mib / 1024.0)
    } else {
        format!("{mib:.0} MB")
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}
