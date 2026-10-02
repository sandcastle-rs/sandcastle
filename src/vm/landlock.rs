//! Host-side Landlock sandbox for the `__vm` child (Linux only). Applied
//! after libkrun is configured and right before `krun_start_enter`, so
//! only the VM's own files stay reachable while the guest runs.

use std::path::Path;

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};

use super::VmSpec;

const ABI_LEVEL: ABI = ABI::V5;

pub struct Rules<'a> {
    pub rw_dirs: &'a [&'a Path],
    pub ro_dirs: &'a [&'a Path],
    pub rw_files: &'a [&'a Path],
    pub devices: &'a [&'a Path],
}

pub fn restrict(spec: &VmSpec) -> Result<()> {
    // guest-root is the VM's `/` and is written by init (mount points).
    let mut rw_dirs = vec![spec.guest_root.as_path()];
    // libkrun dlopens libkrunfw from here inside krun_start_enter.
    let mut ro_dirs = vec![spec.lib_dir.as_path()];
    for share in &spec.shares {
        if share.read_only {
            ro_dirs.push(share.path.as_path());
        } else {
            rw_dirs.push(share.path.as_path());
        }
    }
    let enforced = restrict_paths(&Rules {
        rw_dirs: &rw_dirs,
        ro_dirs: &ro_dirs,
        rw_files: &[spec.disk.as_path()],
        devices: &[Path::new("/dev/kvm")],
    })?;
    if !enforced {
        eprintln!(
            "sandcastle: warning: kernel lacks Landlock; VM process runs without a host sandbox"
        );
    }
    Ok(())
}

/// Returns `false` if the kernel does not support Landlock at all.
pub fn restrict_paths(rules: &Rules) -> Result<bool> {
    let abi = ABI_LEVEL;
    let file_rw: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?;
    for dir in rules.rw_dirs {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, AccessFs::from_all(abi)))?;
    }
    for dir in rules.ro_dirs {
        ruleset = ruleset.add_rule(PathBeneath::new(
            PathFd::new(dir)?,
            AccessFs::from_read(abi),
        ))?;
    }
    for file in rules.rw_files {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(file)?, file_rw))?;
    }
    for dev in rules.devices {
        ruleset = ruleset.add_rule(PathBeneath::new(
            PathFd::new(dev)?,
            file_rw | AccessFs::IoctlDev,
        ))?;
    }
    // restrict_self also sets PR_SET_NO_NEW_PRIVS.
    let status = ruleset.restrict_self()?;
    Ok(status.ruleset != RulesetStatus::NotEnforced)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    // Landlock applies to the calling thread and its future children only,
    // so the restriction stays inside this spawned thread.
    #[test]
    fn only_allowed_paths_stay_accessible() {
        let allowed = tempfile::tempdir().unwrap();
        let denied = tempfile::tempdir().unwrap();
        fs::write(denied.path().join("secret"), b"x").unwrap();
        let (a, d) = (allowed.path().to_path_buf(), denied.path().to_path_buf());
        std::thread::spawn(move || {
            let rules = Rules {
                rw_dirs: &[a.as_path()],
                ro_dirs: &[],
                rw_files: &[],
                devices: &[],
            };
            let enforced = restrict_paths(&rules).unwrap();
            if !enforced {
                eprintln!("kernel without Landlock; skipping");
                return;
            }
            fs::write(a.join("ok"), b"y").expect("allowed dir must stay writable");
            let err = fs::read(d.join("secret")).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        })
        .join()
        .unwrap();
    }
}
