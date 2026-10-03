//! `sandcastle build`: pull the base, apply each instruction, run RUN and
//! COPY in microVMs, and write the image as an OCI layout.

pub mod config;
pub mod dns;

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use oci_spec::image::Descriptor;
use sandcastle_proto::{CopyJob, Job, LAYER_FILE, RunJob};

use crate::blobs::BlobStore;
use crate::dockerfile::{self, Step};
use crate::image::{self, ConfigState, Layer, LayerWriter};
use crate::install::{self, Install};
use crate::registry;
use crate::store::Store;
use crate::vm::{self, Resources, Vm};
use config::{Action, Stage};

/// Longest instruction text shown in progress and error messages.
const SHOWN_CHARS: usize = 60;

pub struct Options {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    pub output: PathBuf,
    pub tag: String,
    pub resources: Resources,
}

/// Builds the image and returns its manifest descriptor in the layout.
pub fn build(exe: &Path, opts: &Options) -> Result<Descriptor> {
    // Unsupported instructions fail here, before any download or VM.
    let recipe = dockerfile::load(&opts.dockerfile)?;
    let context = std::fs::canonicalize(&opts.context)
        .with_context(|| format!("build context {}", opts.context.display()))?;
    ensure!(
        context.is_dir(),
        "build context {} is not a directory",
        context.display()
    );
    let install = Install::locate(exe)?;
    #[cfg(target_os = "linux")]
    crate::doctor::check_kvm(Path::new("/dev/kvm"))?;
    let store = Store::open(&install::store_root()?, &install)?;
    let blobs = store.blobs();

    let total = recipe.steps.len() + 1;
    eprintln!("[1/{total}] FROM {}", recipe.base);
    let image = registry::pull(&blobs, &recipe.base)?;
    let mut stage = Stage::new(
        ConfigState::from_base(&image.config, image.layers)?,
        recipe.escape,
    );
    let vm = Vm {
        exe,
        install: &install,
        store: &store,
        resources: opts.resources,
    };
    let resolv_conf = dns::resolv_conf();
    for (i, step) in recipe.steps.iter().enumerate() {
        let label = format!("step {}/{total} {}", i + 2, shown(&step.text));
        eprintln!("[{}/{total}] {}", i + 2, shown(&step.text));
        run_step(&vm, &blobs, &mut stage, step, &context, &resolv_conf).context(label)?;
    }
    image::write_layout(&blobs, &stage.state, &opts.output, &opts.tag)
}

fn run_step(
    vm: &Vm,
    blobs: &BlobStore<'_>,
    stage: &mut Stage,
    step: &Step,
    context: &Path,
    resolv_conf: &str,
) -> Result<()> {
    let (job, ctx) = match stage.apply(step)? {
        Action::Metadata => return stage.state.add_empty(&step.text),
        Action::Run { argv } => (
            Job::Run(RunJob {
                lower: stage.state.lower_layers()?,
                argv,
                env: stage.run_env(),
                user: stage.user(),
                workdir: stage.workdir(),
                resolv_conf: resolv_conf.to_string(),
            }),
            None,
        ),
        Action::Copy { sources, dest } => (
            Job::Copy(CopyJob {
                lower: stage.state.lower_layers()?,
                sources,
                dest,
                workdir: stage.workdir(),
            }),
            Some(context),
        ),
    };
    let finished = vm.run(&job, ctx)?;
    let status = &finished.status;
    ensure!(status.exit_code == 0, "exited with {}", status.exit_code);
    match &status.layer {
        None => stage.state.add_empty(&step.text),
        Some(diff_id) => {
            let layer = ingest(blobs, &finished.out_dir().join(LAYER_FILE), diff_id)?;
            stage.state.add_layer(layer, &step.text)
        }
    }
}

/// Gzips the guest's layer tar into the blob store and checks that the
/// host and the guest agree on its diff_id.
fn ingest(blobs: &BlobStore<'_>, path: &Path, diff_id: &str) -> Result<Layer> {
    let mut file =
        vm::open_guest_file(path)?.context("the guest reported a layer but wrote no layer.tar")?;
    let mut writer = LayerWriter::new(blobs)?;
    io::copy(&mut file, &mut writer).context("compressing the layer")?;
    let layer = writer.finish()?;
    ensure!(
        layer.diff_id.to_string() == diff_id,
        "layer digest mismatch: the guest reported {diff_id}, the host computed {}",
        layer.diff_id
    );
    Ok(layer)
}

fn shown(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > SHOWN_CHARS {
        format!("{}…", line.chars().take(SHOWN_CHARS).collect::<String>())
    } else {
        line.to_string()
    }
}
