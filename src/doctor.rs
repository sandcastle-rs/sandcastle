//! `sandcastle doctor`: boots one probe VM and reports what the guest supports.

use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sandcastle_proto::Job;
use serde::Serialize;

use crate::install::Install;
use crate::store::Store;
use crate::vm::{Resources, Vm};

#[derive(Debug, Serialize)]
pub struct Report {
    pub lib_dir: PathBuf,
    pub store_root: PathBuf,
    pub kernel_release: String,
    pub overlay_lowerdir_plus: bool,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "libraries:       {}", self.lib_dir.display())?;
        writeln!(f, "store:           {}", self.store_root.display())?;
        writeln!(f, "guest kernel:    {}", self.kernel_release)?;
        writeln!(
            f,
            "overlay lowerdir+: {}",
            if self.overlay_lowerdir_plus {
                "yes"
            } else {
                "no"
            }
        )
    }
}

pub fn run(exe: &Path, install: &Install, store_root: &Path) -> Result<Report> {
    #[cfg(target_os = "linux")]
    check_kvm(Path::new("/dev/kvm"))?;
    let store = Store::open(store_root, install)?;
    let vm = Vm {
        exe,
        install,
        store: &store,
        resources: Resources::default(),
    };
    let status = vm.run(&Job::Probe { exit_code: 0 }, None)?.status.clone();
    if status.exit_code != 0 {
        bail!("guest probe exited with {}", status.exit_code);
    }
    let probe = status.probe.context("guest returned no probe report")?;
    Ok(Report {
        lib_dir: install.lib_dir.clone(),
        store_root: store.root().to_path_buf(),
        kernel_release: probe.kernel_release,
        overlay_lowerdir_plus: probe.overlay_lowerdir_plus,
    })
}

/// KVM needs read-write access to the device; that is the only privilege sandcastle uses.
pub fn check_kvm(path: &Path) -> Result<()> {
    OpenOptions::new().read(true).write(true).open(path).map(drop).with_context(|| {
        format!(
            "cannot open {} read-write: enable virtualization and add your user to the `kvm` group",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_error_explains_the_fix() {
        let err = format!(
            "{:#}",
            check_kvm(Path::new("/nonexistent/kvm")).unwrap_err()
        );
        assert!(err.contains("kvm"), "{err}");
        assert!(err.contains("group"), "{err}");
    }
}
