//! The VM's disks: a private ext4 scratch disk at `/dev/vda` holding the
//! overlay upper and work dirs, then one read-only erofs image per layer
//! the job reads, in [`Job::layer_disks`] order. Nothing on them outlives
//! the VM: the host deletes the scratch disk after the job, and the images
//! are read-only, so nothing is synced or unmounted.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{Job, LowerLayer};

use crate::layer;

/// The scratch disk's mount point.
pub const SCRATCH: &str = "/scratch";

pub struct Disks {
    work: PathBuf,
    /// Mount point of each layer's image, by diff_id.
    layers: HashMap<String, PathBuf>,
}

impl Disks {
    /// Mounts the scratch disk and the image of every layer `job` reads.
    pub fn mount(job: &Job) -> Result<Self> {
        mount_scratch()?;
        let root = Path::new(SCRATCH);
        let work = root.join("work");
        fs::create_dir(&work)?;
        let mut layers = HashMap::new();
        for (i, diff_id) in job.layer_disks().into_iter().enumerate() {
            let dev = format!("/dev/{}", layer::disk_name(i + 1));
            let dir = root.join("layers").join(i.to_string());
            fs::create_dir_all(&dir)?;
            mount(
                dev.as_str(),
                &dir,
                "erofs",
                MountFlags::RDONLY,
                None::<&CStr>,
            )
            .with_context(|| format!("mounting the image of layer {diff_id} ({dev})"))?;
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
                    .with_context(|| format!("layer {id} has no disk"))
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
