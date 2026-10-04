//! Waiting for a `__vm` child.
//!
//! On Linux the guest's `status.json` marks the end of a job: the helper
//! commits it last, after unmounting the store and writing everything else.
//! The host kills the VM at that point instead of waiting for it to exit,
//! because the kernel's teardown of a KVM process takes ~15 ms whatever the
//! VM's size. The build goes on while it finishes, and [`Children`] reaps
//! every child before the store (whose lock the children inherit) is
//! released.

use std::process::{Child, ExitStatus};
use std::sync::Mutex;

/// How a job's VM ended.
pub enum Ended {
    /// The child exited on its own.
    Exited(ExitStatus),
    /// The guest committed its status; the child was killed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Committed,
}

/// Killed `__vm` children the kernel may still be tearing down.
#[derive(Default)]
pub struct Children(Mutex<Vec<Child>>);

impl Children {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn keep(&self, child: Child) {
        let mut children = self.0.lock().unwrap_or_else(|e| e.into_inner());
        children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        children.push(child);
    }
}

impl Drop for Children {
    fn drop(&mut self) {
        let children = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        for child in children {
            let _ = child.wait();
        }
    }
}

pub fn wait(mut child: Child) -> std::io::Result<Ended> {
    child.wait().map(Ended::Exited)
}

#[cfg(target_os = "linux")]
pub use linux::StatusWatch;

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};
    use std::process::Child;

    use anyhow::{Context, Result};
    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::fs::inotify;
    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    use sandcastle_proto::STATUS_FILE;

    use super::{Children, Ended};

    /// Watches a job's `out` directory for the guest's status commit. Set
    /// it up before spawning the child so the commit cannot be missed.
    pub struct StatusWatch {
        inotify: OwnedFd,
        status: PathBuf,
    }

    impl StatusWatch {
        pub fn new(out_dir: &Path) -> Result<Self> {
            let inotify =
                inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
                    .context("creating an inotify instance")?;
            // The guest writes `status.json.tmp` and renames it into place.
            inotify::add_watch(&inotify, out_dir, inotify::WatchFlags::MOVED_TO)
                .context("watching the job's out directory")?;
            Ok(Self {
                inotify,
                status: out_dir.join(STATUS_FILE),
            })
        }

        fn committed(&self) -> bool {
            fs::symlink_metadata(&self.status).is_ok_and(|m| m.is_file())
        }

        /// Waits until the guest commits its status or the child exits.
        pub fn wait(self, mut child: Child, children: &Children) -> Result<Ended> {
            let pidfd = pidfd_open(Pid::from_child(&child), PidfdFlags::empty())
                .context("opening the VM process")?;
            loop {
                if self.committed() {
                    // Nothing the host reads changes after the commit; a guest
                    // that keeps running past it is stopped here.
                    let _ = pidfd_send_signal(&pidfd, Signal::KILL);
                    children.keep(child);
                    return Ok(Ended::Committed);
                }
                let mut fds = [
                    PollFd::new(&self.inotify, PollFlags::IN),
                    PollFd::new(&pidfd, PollFlags::IN),
                ];
                match poll(&mut fds, None) {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(e) => return Err(e).context("waiting for the VM"),
                }
                if fds[1].revents().contains(PollFlags::IN) {
                    return Ok(Ended::Exited(child.wait()?));
                }
                drain(&self.inotify)?;
            }
        }
    }

    fn drain(inotify: &OwnedFd) -> Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match rustix::io::read(inotify, &mut buf) {
                Ok(0) | Err(Errno::AGAIN) => return Ok(()),
                Ok(_) | Err(Errno::INTR) => {}
                Err(e) => return Err(e).context("reading inotify events"),
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    fn sh(script: &str, dir: &std::path::Path) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_committed_status_ends_the_wait_and_kills_the_vm() {
        let dir = tempfile::tempdir().unwrap();
        let watch = StatusWatch::new(dir.path()).unwrap();
        let child = sh(
            "echo '{}' > status.json.tmp && mv status.json.tmp status.json && sleep 30",
            dir.path(),
        );
        let pid = child.id();
        let children = Children::default();
        let start = Instant::now();
        assert!(matches!(
            watch.wait(child, &children).unwrap(),
            Ended::Committed
        ));
        assert!(start.elapsed() < Duration::from_secs(10));
        drop(children);
        // Reaped: the pid no longer names our child.
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn a_vm_that_exits_without_status_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let watch = StatusWatch::new(dir.path()).unwrap();
        let child = sh("touch other; exit 3", dir.path());
        match watch.wait(child, &Children::default()).unwrap() {
            Ended::Exited(status) => assert_eq!(status.code(), Some(3)),
            Ended::Committed => panic!("no status was written"),
        }
    }

    #[test]
    fn a_status_directory_is_not_a_commit() {
        let dir = tempfile::tempdir().unwrap();
        let watch = StatusWatch::new(dir.path()).unwrap();
        let child = sh("mkdir s && mv s status.json; exit 4", dir.path());
        match watch.wait(child, &Children::default()).unwrap() {
            Ended::Exited(status) => assert_eq!(status.code(), Some(4)),
            Ended::Committed => panic!("a directory is not a status"),
        }
    }
}
