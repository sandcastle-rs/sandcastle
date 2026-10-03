use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::image::{self, ConfigState};
use sandcastle::install::{self, Install};
use sandcastle::store::Store;
use sandcastle::vm::Resources;
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
    /// Build a Dockerfile into an OCI image layout directory.
    Build {
        /// Build context directory.
        context: PathBuf,
        /// Dockerfile path [default: <context>/Dockerfile].
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Name recorded in the layout index (use as `oci:<output>:<tag>`).
        #[arg(short, long)]
        tag: String,
        /// Output OCI layout directory (created or updated in place).
        #[arg(short, long)]
        output: PathBuf,
        /// vCPUs per build-step VM [default: all host CPUs].
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
        cpus: Option<u8>,
        /// Memory per build-step VM, in MiB.
        #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u32).range(1..))]
        memory: u32,
        /// Print each step's phases and a summary sorted by duration.
        #[arg(long)]
        timings: bool,
        /// Write a Chrome trace (open in ui.perfetto.dev) to FILE, also on failure.
        #[arg(long, value_name = "FILE")]
        trace: Option<PathBuf>,
        /// Run shell-form RUN without `sh -x` command tracing.
        #[arg(long)]
        no_trace_run: bool,
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
        Command::Build {
            context,
            file,
            tag,
            output,
            cpus,
            memory,
            timings,
            trace,
            no_trace_run,
        } => {
            let mut resources = Resources::default();
            if let Some(cpus) = cpus {
                resources.vcpus = cpus;
            }
            resources.ram_mib = memory;
            build(sandcastle::build::Options {
                dockerfile: file.unwrap_or_else(|| context.join("Dockerfile")),
                context,
                output,
                tag,
                resources,
                timings,
                trace,
                trace_run: !no_trace_run,
            })
        }
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

fn build(opts: sandcastle::build::Options) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let manifest = sandcastle::build::build(&exe, &opts)?;
    println!(
        "{} {}:{}",
        manifest.digest(),
        opts.output.display(),
        opts.tag
    );
    Ok(())
}
