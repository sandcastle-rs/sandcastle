use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::install::{self, Install};
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
    /// Check that this host can run build steps in a microVM.
    Doctor {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Internal: configure and enter a microVM for one job.
    #[command(name = "__vm", hide = true)]
    Vm { job_dir: PathBuf },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Doctor { json } => doctor(json),
        Command::Vm { job_dir } => Err(vm::child::enter(&job_dir)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sandcastle: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn doctor(json: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let install = Install::locate(&exe)?;
    let report = sandcastle::doctor::run(&exe, &install, &install::store_root()?)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{report}");
    }
    Ok(())
}
