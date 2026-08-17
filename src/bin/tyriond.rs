use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Run the Tyrion local Control Plane")]
struct Arguments {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long, hide = true)]
    fault_defer_ready_dispatch: bool,
}

fn main() {
    let arguments = Arguments::parse();
    let options = tyrion::DaemonOptions {
        defer_ready_dispatch: arguments.fault_defer_ready_dispatch,
    };
    if let Err(error) =
        tyrion::run_daemon_with_options(&arguments.data_dir, &arguments.socket, options)
    {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
