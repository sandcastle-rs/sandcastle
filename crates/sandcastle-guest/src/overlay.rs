//! Overlays of layer dirs, added with `lowerdir+` so long stacks fit. A
//! step's overlay has a fresh upper dir that becomes the step's layer, with
//! options that keep it a self-contained diff; a `COPY --from` source is a
//! read-only overlay with no upper dir.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::fs::CWD;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, UnmountFlags, fsconfig_create,
    fsconfig_set_string, fsmount, fsopen, move_mount, unmount,
};

const OPTIONS: [(&str, &str); 4] = [
    ("index", "off"),
    ("metacopy", "off"),
    ("redirect_dir", "off"),
    ("xino", "off"),
];

pub struct Overlay {
    merged: PathBuf,
    upper: PathBuf,
}

impl Overlay {
    /// Mounts `lowers` (top-most first) at `work/merged` with an empty
    /// upper dir `work/upper`.
    pub fn mount(lowers: &[PathBuf], work: &Path) -> Result<Self> {
        let upper = work.join("upper");
        let ovl_work = work.join("ovl-work");
        let merged = work.join("merged");
        let empty = work.join("empty");
        for dir in [&upper, &ovl_work, &merged, &empty] {
            fs::create_dir_all(dir)?;
        }
        // Overlay needs at least one lower; an image may have no layers.
        let lowers: Vec<&Path> = if lowers.is_empty() {
            vec![&empty]
        } else {
            lowers.iter().map(PathBuf::as_path).collect()
        };
        mount_overlay(
            &lowers,
            Some((&upper, &ovl_work)),
            &merged,
            MountAttrFlags::empty(),
        )?;
        Ok(Self { merged, upper })
    }

    pub fn merged(&self) -> &Path {
        &self.merged
    }

    /// Unmounts and returns the upper dir, now an ordinary directory.
    pub fn unmount(self) -> Result<PathBuf> {
        unmount(&self.merged, UnmountFlags::empty()).context("unmounting the overlay")?;
        Ok(self.upper)
    }
}

/// The merged view of a layer stack, mounted read-only.
pub struct ReadOnlyOverlay {
    merged: PathBuf,
}

impl ReadOnlyOverlay {
    /// Mounts `lowers` (top-most first) read-only at `work/merged`.
    pub fn mount(lowers: &[PathBuf], work: &Path) -> Result<Self> {
        let merged = work.join("merged");
        let empty = work.join("empty");
        for dir in [&merged, &empty] {
            fs::create_dir_all(dir)?;
        }
        // Without an upper dir overlay needs two lowers; an empty bottom
        // lower leaves the merged view unchanged.
        let lowers: Vec<&Path> = lowers
            .iter()
            .map(PathBuf::as_path)
            .chain([empty.as_path()])
            .collect();
        mount_overlay(&lowers, None, &merged, MountAttrFlags::MOUNT_ATTR_RDONLY)?;
        Ok(Self { merged })
    }

    pub fn merged(&self) -> &Path {
        &self.merged
    }
}

/// Mounts an overlay of `lowers` (top-most first) at `merged`; `upper` is
/// the upper dir and overlay's work dir, if writable.
fn mount_overlay(
    lowers: &[&Path],
    upper: Option<(&Path, &Path)>,
    merged: &Path,
    attrs: MountAttrFlags,
) -> Result<()> {
    let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
    for &lower in lowers {
        fsconfig_set_string(&fs, "lowerdir+", lower)
            .with_context(|| format!("adding overlay lower {}", lower.display()))?;
    }
    if let Some((upper, work)) = upper {
        fsconfig_set_string(&fs, "upperdir", upper)?;
        fsconfig_set_string(&fs, "workdir", work)?;
    }
    for (key, value) in OPTIONS {
        fsconfig_set_string(&fs, key, value)?;
    }
    fsconfig_create(&fs).context("creating the overlay")?;
    // NODEV: device nodes from image layers must never be openable.
    let mnt = fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        attrs | MountAttrFlags::MOUNT_ATTR_NODEV,
    )?;
    move_mount(
        &mnt,
        "",
        CWD,
        merged,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )
    .context("mounting the overlay")?;
    Ok(())
}
