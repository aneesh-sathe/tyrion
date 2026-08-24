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
    #[arg(long, hide = true)]
    fault_defer_ready_dispatch: bool,
    #[arg(long, hide = true)]
    fault_corrupt_worker_artifact_revision: bool,
    #[arg(long, hide = true)]
    fault_incorrect_first_worker_result: bool,
    #[arg(long, hide = true)]
    fault_hold_worker_for_control: bool,
    #[arg(long, hide = true)]
    fault_skip_sandbox_cleanup: bool,
}

fn main() {
    let arguments = Arguments::parse();
    let options = tyrion::DaemonOptions {
        defer_ready_dispatch: arguments.fault_defer_ready_dispatch,
        corrupt_worker_artifact_revision: arguments.fault_corrupt_worker_artifact_revision,
        incorrect_first_worker_result: arguments.fault_incorrect_first_worker_result,
        codex_worker_config: arguments.codex_worker_config,
        worker_catalog: arguments.worker_catalog,
        hold_worker_for_control: arguments.fault_hold_worker_for_control,
        skip_sandbox_cleanup: arguments.fault_skip_sandbox_cleanup,
    };
    if let Err(error) =
        tyrion::run_daemon_with_options(&arguments.data_dir, &arguments.socket, options)
    {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
