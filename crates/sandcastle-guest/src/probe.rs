//! `Job::Probe`: checks the scratch disk and overlayfs inside the guest.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::mount::{FsOpenFlags, fsconfig_create, fsconfig_set_string, fsopen};
use sandcastle_proto::ProbeReport;

use crate::mounts::{SCRATCH, mount_scratch};

pub fn run() -> Result<ProbeReport> {
    mount_scratch()?;
    let dir = Path::new(SCRATCH).join("probe");
    for sub in ["l1", "l2", "upper", "work"] {
        fs::create_dir_all(dir.join(sub)).context("writing to the scratch disk")?;
    }
    let overlay_lowerdir_plus = overlay_lowerdir_plus(&dir).is_ok();
    Ok(ProbeReport {
        kernel_release: rustix::system::uname()
            .release()
            .to_string_lossy()
            .into_owned(),
        overlay_lowerdir_plus,
    })
}

/// Creates (without mounting) an overlay superblock using `lowerdir+`.
fn overlay_lowerdir_plus(dir: &Path) -> rustix::io::Result<()> {
    let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
    fsconfig_set_string(&fs, "lowerdir+", dir.join("l1"))?;
    fsconfig_set_string(&fs, "lowerdir+", dir.join("l2"))?;
    fsconfig_set_string(&fs, "upperdir", dir.join("upper"))?;
    fsconfig_set_string(&fs, "workdir", dir.join("work"))?;
    fsconfig_create(&fs)
}
