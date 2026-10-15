//! What a job's VM mounts: its private ext4 scratch disk (`/dev/vda`, the
//! only disk) for the overlay upper and work dirs, and the erofs image of
//! every layer the job reads, as files on the read-only `layers` share.
//! Nothing here outlives the VM: the host deletes the scratch disk after
//! the job and the images are read-only, so nothing is synced or unmounted.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{Job, LowerLayer, SHARE_LAYERS};

use crate::layer;

/// The scratch disk's mount point.
pub const SCRATCH: &str = "/scratch";
const LAYERS: &str = "/layers";

pub struct Mounts {
    work: PathBuf,
    /// Mount point of each layer's image, by diff_id.
    layers: HashMap<String, PathBuf>,
}

impl Mounts {
    /// Mounts the scratch disk and the image of every layer `job` reads.
    pub fn mount(job: &Job) -> Result<Self> {
        mount_scratch()?;
        let root = Path::new(SCRATCH);
        let work = root.join("work");
        fs::create_dir(&work)?;
        let ids = job.layers();
        if !ids.is_empty() {
            mount(
                SHARE_LAYERS,
                LAYERS,
                "virtiofs",
                MountFlags::RDONLY,
                None::<&CStr>,
            )
            .context("mounting the layer images")?;
        }
        let mut layers = HashMap::new();
        for (i, diff_id) in ids.into_iter().enumerate() {
            let image = Path::new(LAYERS).join(layer::image_name(diff_id)?);
            let dir = root.join("layers").join(i.to_string());
            fs::create_dir_all(&dir)?;
            // erofs reads a regular file as its backing store.
            mount(&image, &dir, "erofs", MountFlags::RDONLY, None::<&CStr>)
                .with_context(|| format!("mounting the image of layer {diff_id}"))?;
            layers.insert(diff_id.to_owned(), dir);
        }
        Ok(Self { work, layers })
    }

    /// A fresh directory on the scratch disk.
    pub fn work(&self, name: &str) -> Result<PathBuf> {
        let dir = self.work.join(name);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    /// Overlay lower dirs for a bottom-first stack: top-most first.
    pub fn lower_dirs(&self, lower: &[LowerLayer]) -> Result<Vec<PathBuf>> {
        let ids: Vec<String> = lower.iter().map(|l| l.diff_id.clone()).collect();
        layer::overlay_stack(&ids)
            .into_iter()
            .map(|id| {
                self.layers
                    .get(id)
                    .cloned()
                    .with_context(|| format!("layer {id} is not mounted"))
            })
            .collect()
    }
}

/// Mounts the scratch disk at [`SCRATCH`].
pub fn mount_scratch() -> Result<()> {
    mount(
        "/dev/vda",
        SCRATCH,
        "ext4",
        MountFlags::NOATIME,
        None::<&CStr>,
    )
    .context("mounting the scratch disk")
}
