use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Run the Tyrion local Control Plane")]
struct Arguments {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    socket: PathBuf,
}

fn main() {
    let arguments = Arguments::parse();
    if let Err(error) = tyrion::run_daemon(&arguments.data_dir, &arguments.socket) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
