use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Run the Tyrion local Control Plane")]
struct Arguments {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    codex_worker_config: Option<PathBuf>,
    #[arg(long)]
    worker_catalog: Option<PathBuf>,
    #[arg(long)]
    credential_runtime: Option<PathBuf>,
    #[arg(long)]
    principal_control_bootstrap_fd: Option<i32>,
    #[arg(long, hide = true)]
    fault_defer_ready_dispatch: bool,
    #[arg(long, hide = true)]
    fault_corrupt_worker_artifact_revision: bool,
    #[arg(long, hide = true)]
    fault_incorrect_first_worker_result: bool,
    #[arg(long, hide = true)]
    fault_hold_worker_for_control: bool,
    #[arg(long, hide = true)]
    fault_hold_worker_before_integration: bool,
    #[arg(long, hide = true)]
    fault_hold_worker_after_integration: bool,
    #[arg(long, hide = true)]
    fault_hold_worker_after_external_integration: bool,
    #[arg(long, hide = true)]
    fault_skip_sandbox_cleanup: bool,
    #[arg(long, hide = true)]
    fault_leave_effect_started: bool,
    #[arg(long, hide = true)]
    fault_leave_effect_started_after_rename: bool,
    #[arg(long, hide = true)]
    fault_leave_one_shot_effect_started_before_cleanup: bool,
    #[arg(long, default_value_t = 0, hide = true)]
    fault_hold_effect_before_commit_milliseconds: u64,
    #[arg(long, default_value_t = 30_000, hide = true)]
    watchdog_stall_milliseconds: u64,
    #[arg(long, hide = true)]
    fault_memory_now_epoch: Option<i64>,
}

fn main() {
    let arguments = Arguments::parse();
    let options = tyrion::DaemonOptions {
        defer_ready_dispatch: arguments.fault_defer_ready_dispatch,
        corrupt_worker_artifact_revision: arguments.fault_corrupt_worker_artifact_revision,
        incorrect_first_worker_result: arguments.fault_incorrect_first_worker_result,
        codex_worker_config: arguments.codex_worker_config,
        worker_catalog: arguments.worker_catalog,
        credential_runtime: arguments.credential_runtime,
        principal_control_bootstrap_fd: arguments.principal_control_bootstrap_fd,
        hold_worker_for_control: arguments.fault_hold_worker_for_control,
        hold_worker_before_integration: arguments.fault_hold_worker_before_integration,
        hold_worker_after_integration: arguments.fault_hold_worker_after_integration,
        hold_worker_after_external_integration: arguments
            .fault_hold_worker_after_external_integration,
        skip_sandbox_cleanup: arguments.fault_skip_sandbox_cleanup,
        leave_effect_started: arguments.fault_leave_effect_started,
        leave_effect_started_after_rename: arguments.fault_leave_effect_started_after_rename,
        leave_one_shot_effect_started_before_cleanup: arguments
            .fault_leave_one_shot_effect_started_before_cleanup,
        hold_effect_before_commit_milliseconds: arguments
            .fault_hold_effect_before_commit_milliseconds,
        watchdog_stall_milliseconds: arguments.watchdog_stall_milliseconds,
        memory_now_epoch_seconds: arguments.fault_memory_now_epoch,
    };
    if let Err(error) =
        tyrion::run_daemon_with_options(&arguments.data_dir, &arguments.socket, options)
    {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
