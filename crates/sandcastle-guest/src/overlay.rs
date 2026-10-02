//! The overlay a step runs in: layer dirs as lowers (added with
//! `lowerdir+`, so long stacks fit), a fresh upper dir that becomes the
//! step's layer, and options that keep the upper dir a self-contained diff.

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
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        // Overlay needs at least one lower; an image may have no layers.
        let lowers: Vec<&Path> = if lowers.is_empty() {
            vec![&empty]
        } else {
            lowers.iter().map(PathBuf::as_path).collect()
        };
        for lower in lowers {
            fsconfig_set_string(&fs, "lowerdir+", lower)
                .with_context(|| format!("adding overlay lower {}", lower.display()))?;
        }
        fsconfig_set_string(&fs, "upperdir", &upper)?;
        fsconfig_set_string(&fs, "workdir", &ovl_work)?;
        for (key, value) in OPTIONS {
            fsconfig_set_string(&fs, key, value)?;
        }
        fsconfig_create(&fs).context("creating the overlay")?;
        // NODEV: device nodes from image layers must never be openable.
        let mnt = fsmount(
            &fs,
            FsMountFlags::FSMOUNT_CLOEXEC,
            MountAttrFlags::MOUNT_ATTR_NODEV,
        )?;
        move_mount(
            &mnt,
            "",
            CWD,
            &merged,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .context("mounting the overlay")?;
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
