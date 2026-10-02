//! Running guest-helper jobs in libkrun microVMs.
//!
//! `krun_start_enter` takes over the calling process and exits with the
//! guest's exit code, so every job runs in a re-executed `sandcastle __vm`
//! child. The guest's `status.json` is authoritative; the child's exit code
//! is only used to describe failures before the helper ran.

pub mod child;
pub mod krun;
#[cfg(target_os = "linux")]
pub mod landlock;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sandcastle_proto::{JOB_FILE, Job, STATUS_FILE, Status};
use serde::{Deserialize, Serialize};

use crate::install::Install;
use crate::store::Store;

/// VM description the parent writes into the job dir for the `__vm` child.
pub const SPEC_FILE: &str = "vm.json";
const RAM_MIB: u32 = 2048;

#[cfg(target_os = "linux")]
const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";
#[cfg(target_os = "macos")]
const LIB_PATH_ENV: &str = "DYLD_LIBRARY_PATH";

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSpec {
    pub lib_dir: PathBuf,
    pub guest_root: PathBuf,
    pub disk: PathBuf,
    pub out_dir: PathBuf,
    pub vcpus: u8,
    pub ram_mib: u32,
}

/// Runs `job` in a fresh microVM and returns the guest's status.
/// `exe` is the signed `sandcastle` binary used for the `__vm` child.
pub fn run_job(exe: &Path, store: &Store, install: &Install, job: &Job) -> Result<Status> {
    let job_dir = store.new_job_dir()?;
    let out_dir = job_dir.join("out");
    fs::write(out_dir.join(JOB_FILE), serde_json::to_vec(job)?)?;
    let vcpus = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(usize::from(u8::MAX));
    let spec = VmSpec {
        lib_dir: install.lib_dir.clone(),
        guest_root: store.guest_root(),
        disk: store.disk(),
        out_dir: out_dir.clone(),
        vcpus: vcpus as u8,
        ram_mib: RAM_MIB,
    };
    fs::write(job_dir.join(SPEC_FILE), serde_json::to_vec(&spec)?)?;

    // libkrun dlopens libkrunfw by bare file name; point the loader at the bundle.
    let exit = Command::new(exe)
        .arg("__vm")
        .arg(&job_dir)
        .env(LIB_PATH_ENV, &install.lib_dir)
        .status()
        .with_context(|| format!("starting {}", exe.display()))?;
    let status = read_status(&out_dir.join(STATUS_FILE))?;
    fs::remove_dir_all(&job_dir)?;
    outcome(exit.code(), status)
}

fn read_status(path: &Path) -> Result<Option<Status>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("parsing guest status")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Decides a job's result. A missing status file means the helper never
/// finished, whatever the child exit code looks like (libkrun's init itself
/// uses 125/126/127).
pub fn outcome(child_exit: Option<i32>, status: Option<Status>) -> Result<Status> {
    match (status, child_exit) {
        (Some(status), _) => Ok(status),
        (None, Some(code)) => bail!(
            "VM or guest helper setup failed before the step ran (VM process exited with {code})"
        ),
        (None, None) => bail!(
            "VM or guest helper setup failed before the step ran (VM process was killed by a signal)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_file_wins_over_child_exit_code() {
        let status = outcome(
            Some(127),
            Some(Status {
                exit_code: 127,
                probe: None,
            }),
        )
        .unwrap();
        assert_eq!(status.exit_code, 127);
    }

    #[test]
    fn missing_status_is_setup_failure_even_with_exit_127() {
        let err = format!("{:#}", outcome(Some(127), None).unwrap_err());
        assert!(err.contains("setup failed"), "{err}");
        assert!(err.contains("127"), "{err}");
    }

    #[test]
    fn missing_status_after_signal_is_setup_failure() {
        let err = format!("{:#}", outcome(None, None).unwrap_err());
        assert!(err.contains("killed by a signal"), "{err}");
    }
}
