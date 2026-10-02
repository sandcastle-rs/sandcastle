use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::image::{self, ConfigState};
use sandcastle::install::{self, Install};
use sandcastle::store::Store;
use sandcastle::{registry, vm};

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
    /// Pull an image and write it as an OCI image layout directory.
    ///
    /// HEALTHCHECK and other non-OCI Docker config fields are not preserved.
    Pull {
        /// Image reference, e.g. `mirror.gcr.io/library/busybox:1.36`.
        reference: String,
        /// Output OCI layout directory (created or updated in place).
        #[arg(short, long)]
        output: PathBuf,
        /// Name recorded in the layout index; defaults to the reference's tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Internal: configure and enter a microVM for one job.
    #[command(name = "__vm", hide = true)]
    Vm { job_dir: PathBuf },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Doctor { json } => doctor(json),
        Command::Pull {
            reference,
            output,
            tag,
        } => pull(&reference, &output, tag),
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

fn pull(reference: &str, output: &Path, tag: Option<String>) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let install = Install::locate(&exe)?;
    let store = Store::open(&install::store_root()?, &install)?;
    let blobs = store.blobs();
    let image = registry::pull(&blobs, reference)?;
    let state = ConfigState::from_base(&image.config, image.layers)?;
    let tag = match tag {
        Some(tag) => tag,
        None => registry::default_tag(reference)?,
    };
    let manifest = image::write_layout(&blobs, &state, output, &tag)?;
    println!("{} {}:{tag}", manifest.digest(), output.display());
    Ok(())
}
