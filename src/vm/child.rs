//! The `sandcastle __vm <job_dir>` process: configures libkrun and enters the VM.

use std::convert::Infallible;
use std::fs;
use std::path::Path;

use anyhow::Result;
use sandcastle_proto::GUEST_HELPER_PATH;

use super::krun::Krun;
use super::{SPEC_FILE, VmSpec};

/// Never returns on success (libkrun exits the process with the guest's code).
pub fn enter(job_dir: &Path) -> anyhow::Error {
    match try_enter(job_dir) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

fn try_enter(job_dir: &Path) -> Result<Infallible> {
    #[cfg(target_os = "linux")]
    die_with_parent()?;
    let spec: VmSpec = serde_json::from_slice(&fs::read(job_dir.join(SPEC_FILE))?)?;
    let krun = Krun::load(&spec.lib_dir)?;
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
    Err(ctx.start_enter())
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
