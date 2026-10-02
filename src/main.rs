use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::vm;

#[derive(Parser)]
#[command(
    name = "sandcastle",
    version,
    about = "Build OCI images from Dockerfiles in microVMs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Internal: configure and enter a microVM for one job.
    #[command(name = "__vm", hide = true)]
    Vm { job_dir: PathBuf },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Vm { job_dir } => {
            let err = vm::child::enter(&job_dir);
            eprintln!("sandcastle: {err:#}");
            ExitCode::FAILURE
        }
    }
}
