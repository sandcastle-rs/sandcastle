//! The ext4 store disk inside the guest: `/store/layers/<diff_id hex>/`
//! holds one overlay-format directory per layer, `/store/work/` is per-job
//! scratch wiped at every boot.

use std::ffi::CStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use sandcastle_proto::{LowerLayer, SHARE_BLOBS};

use crate::{layer, unpack};

/// First virtio-blk device: the store disk added by the host.
const STORE_DEV: &str = "/dev/vda";
const STORE: &str = "/store";
const BLOBS: &str = "/blobs";

pub struct Store {
    layers: PathBuf,
    work: PathBuf,
}

impl Store {
    pub fn mount() -> Result<Self> {
        mount(STORE_DEV, STORE, "ext4", MountFlags::NOATIME, None::<&CStr>)
            .context("mounting the store disk")?;
        let root = Path::new(STORE);
        let store = Self {
            layers: root.join("layers"),
            work: root.join("work"),
        };
        match fs::remove_dir_all(&store.work) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("removing work left by an interrupted job"),
        }
        fs::create_dir_all(&store.layers)?;
        fs::create_dir_all(&store.work)?;
        Ok(store)
    }

    /// Unmounting flushes the guest page cache to the disk; without it the
    /// VM teardown would drop written layers.
    pub fn unmount(self) -> Result<()> {
        unmount(STORE, UnmountFlags::empty()).context("unmounting the store disk")
    }

    pub fn layer_dir(&self, diff_id: &str) -> Result<PathBuf> {
        Ok(self.layers.join(layer::digest_hex(diff_id)?))
    }

    /// A fresh directory under `/store/work`.
    pub fn work(&self, name: &str) -> Result<PathBuf> {
        let dir = self.work.join(name);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    /// Unpacks every layer of `lower` that is not in the store yet.
    pub fn ensure_layers(&self, lower: &[LowerLayer]) -> Result<()> {
        let mut blobs_mounted = false;
        for l in lower {
            let dest = self.layer_dir(&l.diff_id)?;
            if dest.is_dir() {
                continue;
            }
            if !blobs_mounted {
                mount(
                    SHARE_BLOBS,
                    BLOBS,
                    "virtiofs",
                    MountFlags::RDONLY,
                    None::<&CStr>,
                )
                .context("mounting the blob share")?;
                blobs_mounted = true;
            }
            let blob = Path::new(BLOBS)
                .join("sha256")
                .join(layer::digest_hex(&l.blob)?);
            let tmp = self
                .work
                .join(format!("unpack-{}", layer::digest_hex(&l.diff_id)?));
            unpack::unpack(&blob, &l.media_type, &l.diff_id, &tmp)
                .with_context(|| format!("unpacking layer {}", l.diff_id))?;
            fs::rename(&tmp, &dest)?;
        }
        Ok(())
    }

    /// Overlay lower dirs for a bottom-first stack: top-most first.
    pub fn lower_dirs(&self, lower: &[LowerLayer]) -> Result<Vec<PathBuf>> {
        let ids: Vec<String> = lower.iter().map(|l| l.diff_id.clone()).collect();
        layer::overlay_stack(&ids)
            .into_iter()
            .map(|id| self.layer_dir(id))
            .collect()
    }
}
