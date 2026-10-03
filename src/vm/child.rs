//! The `sandcastle __vm <job_dir>` process: configures libkrun and enters the VM.

use std::convert::Infallible;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use sandcastle_proto::GUEST_HELPER_PATH;

use super::krun::Krun;
use super::{MARKS_FILE, SPEC_FILE, VmMarks, VmSpec, unix_ns};

/// Never returns on success (libkrun exits the process with the guest's code).
pub fn enter(job_dir: &Path) -> anyhow::Error {
    match try_enter(job_dir) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

fn try_enter(job_dir: &Path) -> Result<Infallible> {
    let main_ns = unix_ns();
    #[cfg(target_os = "linux")]
    die_with_parent()?;
    raise_fd_limit();
    // Opened before Landlock, which forbids creating files afterwards.
    let mut marks_file = File::create(job_dir.join(MARKS_FILE)).ok();
    let spec: VmSpec = serde_json::from_slice(&fs::read(job_dir.join(SPEC_FILE))?)?;
    let krun = Krun::load(&spec.lib_dir)?;
    let loaded_ns = unix_ns();
    krun.set_log_level_error()?;
    let mut ctx = krun.create_ctx()?;
    ctx.set_vm_config(spec.vcpus, spec.ram_mib)?;
    ctx.set_root(&spec.guest_root)?;
    ctx.add_disk("store", &spec.disk, false)?;
    for share in &spec.shares {
        ctx.add_virtiofs(&share.tag, &share.path, share.read_only)?;
    }
    ctx.set_workdir("/")?;
    ctx.set_exec(GUEST_HELPER_PATH, &[], &["HOME=/"])?;
    #[cfg(target_os = "linux")]
    super::landlock::restrict(&spec)?;
    let configured_ns = unix_ns();
    if let Some(file) = marks_file.as_mut() {
        let marks = VmMarks {
            main_ns,
            loaded_ns,
            configured_ns,
            enter_ns: unix_ns(),
        };
        // Timing only: a failed write just means no breakdown for this step.
        let _ = file.write_all(&serde_json::to_vec(&marks)?);
    }
    Err(ctx.start_enter())
}

/// libkrun's virtio-fs server holds a host fd for every guest inode it has
/// looked up, so a COPY of a large context exceeds the usual soft limit of
/// 1024. Raise the soft limit to the hard one, as container runtimes do.
fn raise_fd_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    if limit.current == limit.maximum {
        return;
    }
    let raised = Rlimit {
        current: limit.maximum,
        maximum: limit.maximum,
    };
    if let Err(e) = setrlimit(Resource::Nofile, raised) {
        // macOS refuses an unlimited soft limit; its virtio-fs does not keep
        // an fd per inode, so the inherited limit is enough there.
        if cfg!(target_os = "linux") {
            eprintln!("sandcastle: warning: could not raise the open file limit: {e}");
        }
    }
}

/// The VM keeps the store disk open, so it must not outlive the build that
/// started it. (The store lock is inherited too, so an orphan never shares
/// the disk with a new build; this just ends it promptly.) The signal fires
/// when the spawning *thread* exits, which is the build's main thread.
#[cfg(target_os = "linux")]
fn die_with_parent() -> Result<()> {
    use anyhow::ensure;
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    let parent: u32 = std::env::var(super::PARENT_PID_ENV)?.parse()?;
    ensure!(
        std::os::unix::process::parent_id() == parent,
        "sandcastle exited before the VM started"
    );
    Ok(())
}
