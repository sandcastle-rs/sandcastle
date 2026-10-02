//! `RUN`: the command runs chrooted in the overlay, in its own PID and mount
//! namespaces, as Docker's runtime would run it.

use std::ffi::{CStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Stdio};

use anyhow::{Context, Result};
use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, mount, mount_bind, mount_change};
use rustix::process::{Gid, Uid, chroot};
use rustix::thread::{
    CapabilitySet, CapabilitySets, UnshareFlags, remove_capability_from_bounding_set,
    set_capabilities, set_thread_groups, set_thread_res_gid, set_thread_res_uid,
};
use sandcastle_proto::{GUEST_HELPER_PATH, RunJob, Status};
use serde::{Deserialize, Serialize};

use crate::copy::resolve_in_root;
use crate::linux::commit_upper;
use crate::overlay::Overlay;
use crate::store::Store;
use crate::user;

/// Argument that makes the helper act as the step's exec child.
pub const EXEC_ARG: &str = "--exec";
const ETC_FILES: [&str; 3] = ["resolv.conf", "hosts", "hostname"];
/// tmpfs on the VM's `/tmp` holding the step's etc files.
const RUN_ETC: &str = "/tmp/sandcastle-etc";
const HOSTNAME: &str = "sandcastle";
const HOSTS: &str =
    "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\tsandcastle\n";
/// Exit codes of a shell for a command that could not start.
const EXIT_NOT_FOUND: i32 = 127;
const EXIT_CANNOT_EXEC: i32 = 126;
const EXIT_SETUP: i32 = 125;
/// Docker's default container capabilities minus MKNOD.
const ALLOWED_CAPS: CapabilitySet = CapabilitySet::CHOWN
    .union(CapabilitySet::DAC_OVERRIDE)
    .union(CapabilitySet::FSETID)
    .union(CapabilitySet::FOWNER)
    .union(CapabilitySet::SETGID)
    .union(CapabilitySet::SETUID)
    .union(CapabilitySet::SETPCAP)
    .union(CapabilitySet::SETFCAP)
    .union(CapabilitySet::NET_RAW)
    .union(CapabilitySet::NET_BIND_SERVICE)
    .union(CapabilitySet::SYS_CHROOT)
    .union(CapabilitySet::KILL)
    .union(CapabilitySet::AUDIT_WRITE);

/// What the exec child needs; passed as JSON in its argv.
#[derive(Serialize, Deserialize)]
struct ExecSpec {
    root: PathBuf,
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

pub fn run_job(store: &Store, job: &RunJob, out: &Path) -> Result<Status> {
    store.ensure_layers(&job.lower)?;
    let work = store.work("run")?;
    let mut lowers = store.lower_dirs(&job.lower)?;
    lowers.push(stub_layer(&work)?);
    write_etc(&job.resolv_conf)?;
    let overlay = Overlay::mount(&lowers, &work)?;
    let result = execute(overlay.merged(), job);
    let upper = overlay.unmount()?;
    let exit_code = result?;
    if exit_code != 0 {
        fs::remove_dir_all(&upper)?;
        return Ok(Status {
            exit_code,
            ..Default::default()
        });
    }
    commit_upper(store, &upper, out)
}

/// Bottom-most lower layer providing mount points, so binds never create
/// files in the upper dir.
fn stub_layer(work: &Path) -> Result<PathBuf> {
    let stubs = work.join("stubs");
    for dir in ["dev", "proc", "sys", "etc"] {
        fs::create_dir_all(stubs.join(dir))?;
    }
    for file in ETC_FILES {
        File::create(stubs.join("etc").join(file))?;
    }
    Ok(stubs)
}

fn write_etc(resolv_conf: &str) -> Result<()> {
    mount(
        "tmpfs",
        "/tmp",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        None::<&CStr>,
    )
    .context("mounting /tmp")?;
    let etc = Path::new(RUN_ETC);
    fs::create_dir(etc)?;
    fs::write(etc.join("resolv.conf"), resolv_conf)?;
    fs::write(etc.join("hosts"), HOSTS)?;
    fs::write(etc.join("hostname"), format!("{HOSTNAME}\n"))?;
    rustix::system::sethostname(HOSTNAME.as_bytes())?;
    Ok(())
}

fn execute(root: &Path, job: &RunJob) -> Result<i32> {
    let passwd = read_in_root(root, "/etc/passwd")?;
    let group = read_in_root(root, "/etc/group")?;
    let ids = user::resolve(&job.user, &passwd, &group)?;
    let mut env = job.env.clone();
    if !env.iter().any(|e| e.starts_with("HOME=")) {
        env.push(format!("HOME={}", ids.home));
    }
    let spec = ExecSpec {
        root: root.to_path_buf(),
        argv: job.argv.clone(),
        env,
        workdir: job.workdir.clone(),
        uid: ids.uid,
        gid: ids.gid,
        groups: ids.groups,
    };
    // SAFETY: CLONE_NEWPID only changes which namespace later children are
    // created in; it does not unshare the file table, the hazard
    // `unshare_unsafe` guards against.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWPID) }
        .context("creating the step's PID namespace")?;
    // The child is init of that namespace: when the command exits the kernel
    // kills whatever it left running, so the overlay can be unmounted.
    let status = Command::new(GUEST_HELPER_PATH)
        .arg(EXEC_ARG)
        .arg(serde_json::to_string(&spec)?)
        .stdin(Stdio::null())
        .status()
        .context("starting the step")?;
    Ok(exit_code(status))
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Reads a file of the image, resolving links inside the image root.
fn read_in_root(root: &Path, path: &str) -> Result<String> {
    match fs::read_to_string(resolve_in_root(root, Path::new(path))?) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {path}")),
    }
}

/// Entry point of `sandcastle-guest --exec <spec>`.
pub fn exec_main(spec: Option<OsString>) -> ! {
    let spec: ExecSpec = match spec.context("missing exec spec").and_then(|s| {
        Ok(serde_json::from_str(
            s.to_str().context("exec spec is not UTF-8")?,
        )?)
    }) {
        Ok(spec) => spec,
        Err(e) => fail_setup(e),
    };
    if let Err(e) = setup(&spec) {
        fail_setup(e);
    }
    let err = Command::new(&spec.argv[0])
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().filter_map(|e| e.split_once('=')))
        .exec();
    eprintln!("sandcastle: exec {}: {err}", spec.argv[0]);
    process::exit(if err.kind() == io::ErrorKind::NotFound {
        EXIT_NOT_FOUND
    } else {
        EXIT_CANNOT_EXEC
    })
}

fn fail_setup(e: anyhow::Error) -> ! {
    eprintln!("sandcastle: preparing the step: {e:#}");
    process::exit(EXIT_SETUP)
}

fn setup(spec: &ExecSpec) -> Result<()> {
    anyhow::ensure!(!spec.argv.is_empty(), "empty command");
    // SAFETY: this process is single-threaded; CLONE_NEWNS does not touch
    // the file table.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }?;
    mount_change(
        "/",
        MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )?;
    let root = &spec.root;
    let hardened = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    mount("proc", root.join("proc"), "proc", hardened, None::<&CStr>).context("mounting /proc")?;
    mount(
        "sysfs",
        root.join("sys"),
        "sysfs",
        hardened | MountFlags::RDONLY,
        None::<&CStr>,
    )
    .context("mounting /sys")?;
    populate_dev(&root.join("dev")).context("populating /dev")?;
    for file in ETC_FILES {
        let target = root.join("etc").join(file);
        // A link here would be followed outside the image root; skip it.
        if fs::symlink_metadata(&target).is_ok_and(|m| m.is_file()) {
            mount_bind(Path::new(RUN_ETC).join(file), &target)
                .with_context(|| format!("binding /etc/{file}"))?;
        }
    }
    chroot(root)?;
    std::env::set_current_dir("/")?;
    rustix::process::umask(Mode::from_raw_mode(0o022));
    fs::create_dir_all(&spec.workdir)
        .with_context(|| format!("creating workdir {}", spec.workdir))?;
    std::env::set_current_dir(&spec.workdir)?;
    drop_capabilities()?;
    let groups: Vec<Gid> = spec.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    set_thread_groups(&groups)?;
    let gid = Gid::from_raw(spec.gid);
    set_thread_res_gid(gid, gid, gid)?;
    let uid = Uid::from_raw(spec.uid);
    set_thread_res_uid(uid, uid, uid)?;
    Ok(())
}

/// Removes every capability outside `ALLOWED_CAPS`. Without this a root step
/// could mknod the store disk, or mount things, and poison cached layers of
/// other images. This deliberately deviates from Docker: a step cannot mknod.
fn drop_capabilities() -> Result<()> {
    for cap in 0..u64::BITS {
        let bit = CapabilitySet::from_bits_retain(1 << cap);
        if ALLOWED_CAPS.contains(bit) {
            continue;
        }
        match remove_capability_from_bounding_set(bit) {
            Ok(()) => {}
            // Past the last capability this kernel knows.
            Err(Errno::INVAL) => break,
            Err(e) => return Err(e).context("dropping from the bounding set"),
        }
    }
    set_capabilities(
        None,
        CapabilitySets {
            effective: ALLOWED_CAPS,
            permitted: ALLOWED_CAPS,
            inheritable: ALLOWED_CAPS,
        },
    )
    .context("restricting capabilities")
}

/// Docker's container `/dev`: a tmpfs with the standard character devices,
/// a private devpts and `/dev/shm`; never the VM's own devices.
fn populate_dev(dev: &Path) -> Result<()> {
    mount(
        "tmpfs",
        dev,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(c"mode=755,size=65536k"),
    )?;
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let path = dev.join(name);
        mknodat(
            CWD,
            &path,
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o666),
            makedev(major, minor),
        )?;
        rustix::fs::chmod(&path, Mode::from_raw_mode(0o666))?;
    }
    fs::create_dir(dev.join("pts"))?;
    mount(
        "devpts",
        dev.join("pts"),
        "devpts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(c"newinstance,ptmxmode=0666,mode=0620"),
    )?;
    std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir(dev.join("shm"))?;
    mount(
        "shm",
        dev.join("shm"),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        Some(c"mode=1777,size=65536k"),
    )?;
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        std::os::unix::fs::symlink(target, dev.join(name))?;
    }
    Ok(())
}
