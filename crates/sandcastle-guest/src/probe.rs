//! `Job::Probe`: checks the store disk and overlayfs inside the guest.

use std::ffi::CStr;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::mount::{
    FsOpenFlags, MountFlags, UnmountFlags, fsconfig_create, fsconfig_set_string, fsopen, mount,
    unmount,
};
use sandcastle_proto::ProbeReport;

/// First virtio-blk device: the store disk added by the host.
const STORE_DEV: &str = "/dev/vda";
const STORE: &str = "/store";

pub fn run() -> Result<ProbeReport> {
    mount(STORE_DEV, STORE, "ext4", MountFlags::NOATIME, None::<&CStr>)
        .context("mounting the store disk")?;
    let dir = Path::new(STORE).join("probe");
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    for sub in ["l1", "l2", "upper", "work"] {
        fs::create_dir_all(dir.join(sub)).context("writing to the store disk")?;
    }
    let overlay_lowerdir_plus = overlay_lowerdir_plus(&dir).is_ok();
    fs::remove_dir_all(&dir)?;
    unmount(STORE, UnmountFlags::empty()).context("unmounting the store disk")?;
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
