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

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use sandcastle_proto::{JOB_FILE, Job, SHARE_BLOBS, SHARE_CTX, SHARE_OUT, STATUS_FILE, Status};
use serde::{Deserialize, Serialize};

use crate::install::Install;
use crate::store::Store;

/// VM description the parent writes into the job dir for the `__vm` child.
pub const SPEC_FILE: &str = "vm.json";
/// Directory to save each build step's guest kernel log into
/// (`step-<n>.kmsg`), for diagnosing VM boot time.
pub const DEBUG_KMSG_ENV: &str = "SANDCASTLE_DEBUG_KMSG";
/// Extra guest kernel boot parameters, space separated, for experiments.
pub const DEBUG_KERNEL_ARGS_ENV: &str = "SANDCASTLE_DEBUG_KERNEL_ARGS";
/// Timestamps the `__vm` child records while it starts the VM.
pub const MARKS_FILE: &str = "vm-marks.json";
/// Largest marks file read back from the job directory.
const MAX_MARKS_BYTES: u64 = 4096;
/// The parent's pid, so the child can tell whether it outlived it.
pub const PARENT_PID_ENV: &str = "SANDCASTLE_PARENT_PID";
/// Largest `status.json` read from the guest.
const MAX_STATUS_BYTES: u64 = 1 << 20;

#[cfg(target_os = "linux")]
const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";
#[cfg(target_os = "macos")]
const LIB_PATH_ENV: &str = "DYLD_LIBRARY_PATH";

/// A host directory shared into the guest at `/<tag>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSpec {
    pub lib_dir: PathBuf,
    pub guest_root: PathBuf,
    pub disk: PathBuf,
    pub shares: Vec<Share>,
    pub vcpus: u8,
    pub ram_mib: u32,
}

/// vCPUs and memory of each build-step VM.
#[derive(Debug, Clone, Copy)]
pub struct Resources {
    pub vcpus: u8,
    pub ram_mib: u32,
}

impl Default for Resources {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            vcpus: cpus.min(usize::from(u8::MAX)) as u8,
            ram_mib: 2048,
        }
    }
}

/// Runs guest jobs. `exe` is the signed `sandcastle` binary used for the
/// `__vm` child.
pub struct Vm<'a> {
    pub exe: &'a Path,
    pub install: &'a Install,
    pub store: &'a Store,
    pub resources: Resources,
}

impl Vm<'_> {
    /// Runs `job` in a fresh microVM. `ctx` is shared read-only at `/ctx`.
    pub fn run(&self, job: &Job, ctx: Option<&Path>) -> Result<Finished> {
        let mut finished = Finished {
            status: Status::default(),
            dir: self.store.new_job_dir()?,
            started: Instant::now(),
            ended: Instant::now(),
            started_ns: 0,
            ended_ns: 0,
            marks: None,
            helper_end_ns: None,
        };
        let out_dir = finished.out_dir();
        fs::write(out_dir.join(JOB_FILE), serde_json::to_vec(job)?)?;
        let mut shares = vec![Share {
            tag: SHARE_OUT.into(),
            path: out_dir.clone(),
            read_only: false,
        }];
        if !matches!(job, Job::Probe { .. }) {
            shares.push(Share {
                tag: SHARE_BLOBS.into(),
                path: self.store.blobs_dir(),
                read_only: true,
            });
        }
        if let Some(ctx) = ctx {
            shares.push(Share {
                tag: SHARE_CTX.into(),
                path: ctx.to_path_buf(),
                read_only: true,
            });
        }
        let spec = VmSpec {
            lib_dir: self.install.lib_dir.clone(),
            guest_root: self.store.guest_root(),
            disk: self.store.disk(),
            shares,
            vcpus: self.resources.vcpus,
            ram_mib: self.resources.ram_mib,
        };
        fs::write(finished.dir.join(SPEC_FILE), serde_json::to_vec(&spec)?)?;

        // libkrun dlopens libkrunfw by bare file name; point the loader at the bundle.
        finished.started = Instant::now();
        finished.started_ns = unix_ns();
        let exit = Command::new(self.exe)
            .arg("__vm")
            .arg(&finished.dir)
            .env(LIB_PATH_ENV, &self.install.lib_dir)
            .env(PARENT_PID_ENV, std::process::id().to_string())
            .status()
            .with_context(|| format!("starting {}", self.exe.display()))?;
        finished.ended = Instant::now();
        finished.ended_ns = unix_ns();
        finished.marks = read_marks(&finished.dir.join(MARKS_FILE));
        finished.helper_end_ns = modified_ns(&out_dir.join(STATUS_FILE));
        let status = read_status(&out_dir.join(STATUS_FILE))?;
        finished.status = outcome(exit.code(), status)?;
        Ok(finished)
    }
}

/// A completed job. Its directory, with the files the guest left in
/// `out`, is removed when this is dropped.
pub struct Finished {
    pub status: Status,
    /// When the VM process was spawned and when it exited.
    pub started: Instant,
    pub ended: Instant,
    /// The same two moments on the wall clock, which the `__vm` child and
    /// the host's file timestamps share.
    pub started_ns: u64,
    pub ended_ns: u64,
    /// When the child loaded libkrun, configured it and entered the VM.
    pub marks: Option<VmMarks>,
    /// When the guest last wrote `status.json`, stamped by libkrun's
    /// virtio-fs server on the host: the end of the guest helper.
    pub helper_end_ns: Option<u64>,
    dir: PathBuf,
}

/// Wall-clock nanoseconds since the Unix epoch at four points of the `__vm`
/// child's start-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmMarks {
    /// `main` entered.
    pub main_ns: u64,
    /// libkrun and libkrunfw loaded.
    pub loaded_ns: u64,
    /// libkrun configured and the Landlock sandbox applied.
    pub configured_ns: u64,
    /// Just before `krun_start_enter`.
    pub enter_ns: u64,
}

pub fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The child's marks, or `None` if it wrote none (it failed before the VM
/// started) or wrote something unreadable. Timing only; never an error.
fn read_marks(path: &Path) -> Option<VmMarks> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_MARKS_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Modification time of a regular file, without following links.
fn modified_ns(path: &Path) -> Option<u64> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(d.as_nanos()).ok()
}

impl Finished {
    pub fn out_dir(&self) -> PathBuf {
        self.dir.join("out")
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            eprintln!("sandcastle: could not remove {}: {e}", self.dir.display());
        }
    }
}

/// Opens a file the guest wrote to its `out` share. The guest controls that
/// directory, so links and special files are refused and the open never
/// blocks. A missing file is `None`.
pub fn open_guest_file(path: &Path) -> Result<Option<File>> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => {
            return Err(std::io::Error::from(e))
                .with_context(|| format!("opening guest file {}", path.display()));
        }
    };
    let file = File::from(fd);
    ensure!(
        file.metadata()?.is_file(),
        "guest file {} is not a regular file",
        path.display()
    );
    Ok(Some(file))
}

fn read_status(path: &Path) -> Result<Option<Status>> {
    let Some(file) = open_guest_file(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_STATUS_BYTES).read_to_end(&mut bytes)?;
    match serde_json::from_slice(&bytes) {
        Ok(status) => Ok(Some(status)),
        Err(e) => {
            // The guest commits status atomically, so garbage means no commit.
            let e = anyhow::Error::new(e).context("parsing guest status");
            eprintln!("sandcastle: ignoring unreadable guest status: {e:#}");
            Ok(None)
        }
    }
}

/// Decides a job's result. A missing status file means the helper never
/// finished, whatever the child exit code looks like (libkrun's init itself
/// uses 125/126/127).
pub fn outcome(child_exit: Option<i32>, status: Option<Status>) -> Result<Status> {
    match (status, child_exit) {
        (
            Some(Status {
                error: Some(error), ..
            }),
            _,
        ) => bail!("guest helper failed: {error}"),
        (Some(status), _) => Ok(status),
        (None, Some(code)) => bail!(
            "the VM or guest helper failed without reporting a status (see its output above; VM process exited with {code})"
        ),
        (None, None) => bail!(
            "the VM or guest helper failed without reporting a status (see its output above; VM process was killed by a signal)"
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
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(status.exit_code, 127);
    }

    #[test]
    fn missing_status_is_a_failure_even_with_exit_127() {
        let err = format!("{:#}", outcome(Some(127), None).unwrap_err());
        assert!(err.contains("without reporting a status"), "{err}");
        assert!(err.contains("127"), "{err}");
    }

    #[test]
    fn unparseable_status_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        fs::write(&path, b"{\"exit_co").unwrap();
        let status = read_status(&path).unwrap();
        assert!(status.is_none());
        let err = format!("{:#}", outcome(Some(0), status).unwrap_err());
        assert!(err.contains("without reporting a status"), "{err}");
    }

    #[test]
    fn missing_status_after_signal_is_a_failure() {
        let err = format!("{:#}", outcome(None, None).unwrap_err());
        assert!(err.contains("killed by a signal"), "{err}");
    }

    #[test]
    fn guest_error_in_status_is_reported() {
        let err = outcome(
            Some(1),
            Some(Status {
                exit_code: 1,
                error: Some("mounting the overlay: EINVAL".into()),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("guest helper failed: mounting the overlay"),
            "{err:#}"
        );
    }

    #[test]
    fn guest_files_must_be_regular_and_not_links() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        fs::write(&secret, b"{}").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("status.json")).unwrap();
        assert!(open_guest_file(&dir.path().join("status.json")).is_err());
        assert!(
            open_guest_file(&dir.path().join("missing"))
                .unwrap()
                .is_none()
        );
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let err = open_guest_file(&fifo).unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
    }
}
