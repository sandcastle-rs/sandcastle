//! The `sandcastle __vm <job_dir>` process: configures libkrun and enters the VM.

use std::convert::Infallible;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use sandcastle_proto::{GUEST_HELPER_PATH, KMSG_ENV};

use super::krun::Krun;
use super::{MARKS_FILE, SPEC_FILE, VmMarks, VmSpec, unix_ns};

/// Never returns on success (libkrun exits the process with the guest's code).
pub fn enter(job_dir: &Path) -> anyhow::Error {
    match try_enter(job_dir) {
        Ok(never) => match never {},
        Err(e) => e,
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn x86_guests_skip_the_tsc_sync_check() {
        assert!(
            guest_env(false, None)
                .unwrap()
                .contains(&"tsc=reliable".to_string())
        );
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
    let env = guest_env(
        std::env::var_os(super::DEBUG_KMSG_ENV).is_some(),
        std::env::var(super::DEBUG_KERNEL_ARGS_ENV).ok().as_deref(),
    )?;
    let env: Vec<&str> = env.iter().map(String::as_str).collect();
    ctx.set_exec(GUEST_HELPER_PATH, &[], &env)?;
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

/// Guest kernel boot parameters sandcastle always passes.
///
/// `tsc=reliable` skips x86's TSC synchronization test, which otherwise runs
/// for ~20 ms per secondary CPU while the guest boots (the bench box spent
/// ~120 ms of every 8-vCPU step in it). KVM gives all vCPUs the host's
/// synchronized TSC, and a build-step VM lives for well under a second.
#[cfg(target_arch = "x86_64")]
const KERNEL_ARGS: &[&str] = &["tsc=reliable"];
#[cfg(not(target_arch = "x86_64"))]
const KERNEL_ARGS: &[&str] = &[];

/// The guest helper's environment. libkrun writes each entry, in double
/// quotes, onto the guest kernel's command line; the kernel takes the ones
/// it knows as boot parameters, which is how `kernel_args` reach it.
fn guest_env(kmsg: bool, kernel_args: Option<&str>) -> Result<Vec<String>> {
    let mut env = vec!["HOME=/".to_string()];
    env.extend(KERNEL_ARGS.iter().map(|a| a.to_string()));
    if kmsg {
        env.push(format!("{KMSG_ENV}=1"));
    }
    for arg in kernel_args.unwrap_or_default().split_whitespace() {
        anyhow::ensure!(
            !arg.contains('"'),
            "kernel argument {arg:?} contains a double quote"
        );
        env.push(arg.to_string());
    }
    Ok(env)
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

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn x86_guests_skip_the_tsc_sync_check() {
        assert!(
            guest_env(false, None)
                .unwrap()
                .contains(&"tsc=reliable".to_string())
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_env_carries_debug_flags_and_kernel_args() {
        let base: Vec<&str> = ["HOME=/"]
            .into_iter()
            .chain(KERNEL_ARGS.iter().copied())
            .collect();
        assert_eq!(guest_env(false, None).unwrap(), base);
        let mut debug = base.clone();
        debug.extend(["SANDCASTLE_KMSG=1", "nosmt", "loglevel=7"]);
        assert_eq!(guest_env(true, Some("nosmt  loglevel=7")).unwrap(), debug);
        // libkrun wraps each entry in quotes on the kernel command line.
        let err = guest_env(false, Some("bad\"arg")).unwrap_err();
        assert!(format!("{err:#}").contains("quote"), "{err:#}");
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn x86_guests_skip_the_tsc_sync_check() {
        assert!(
            guest_env(false, None)
                .unwrap()
                .contains(&"tsc=reliable".to_string())
        );
    }
}
