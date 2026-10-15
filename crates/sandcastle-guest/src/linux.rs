use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process;
use std::time::Instant;

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{
    CopyJob, CopySource, EVENTS_FILE, JOB_FILE, Job, KMSG_ENV, KMSG_FILE, LAYER_FILE,
    MAX_KMSG_BYTES, SHARE_CTX, SHARE_OUT, STATUS_FILE, Status,
};

use crate::commit;
use crate::copy::{Copy, Owner};
use crate::events::Recorder;
use crate::mounts::Mounts;
use crate::overlay::{Overlay, ReadOnlyOverlay};

pub const OUT: &str = "/out";
const CTX: &str = "/ctx";

pub fn main() -> ! {
    let mut args = std::env::args_os().skip(1);
    if let Some(arg) = args.next()
        && arg == crate::run::EXEC_ARG
    {
        crate::run::exec_main(args.next())
    }
    match job_main() {
        Ok(code) => process::exit(code),
        Err(e) => {
            // No status file: the host reports a setup failure.
            eprintln!("sandcastle-guest: {e:#}");
            process::exit(125)
        }
    }
}

fn job_main() -> Result<i32> {
    let start = Instant::now();
    let boot = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let boot_us = boot.tv_sec as u64 * 1_000_000 + boot.tv_nsec as u64 / 1_000;
    mount(
        SHARE_OUT,
        OUT,
        "virtiofs",
        MountFlags::empty(),
        None::<&CStr>,
    )
    .context("mounting the out share")?;
    let out = Path::new(OUT);
    // Failing to create the file only means there are no events.
    let rec = Recorder::create(&out.join(EVENTS_FILE), start).ok();
    if let Some(r) = &rec {
        r.boot(boot_us);
    }
    let status = match read_job(out).and_then(|job| run_job(job, out, rec.as_ref())) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("sandcastle-guest: {e:#}");
            Status {
                exit_code: 1,
                error: Some(format!("{e:#}")),
                ..Default::default()
            }
        }
    };
    if let Some(r) = rec {
        // Events never decide a job's success; `status.json` alone does.
        let _ = r.finish();
    }
    if std::env::var_os(KMSG_ENV).is_some()
        && let Err(e) = copy_kmsg(&out.join(KMSG_FILE))
    {
        eprintln!("sandcastle-guest: not saving the kernel log: {e}");
    }
    write_status(out, &status)?;
    Ok(status.exit_code)
}

fn read_job(out: &Path) -> Result<Job> {
    serde_json::from_slice(&fs::read(out.join(JOB_FILE)).context("reading job")?)
        .context("parsing job")
}

fn run_job(job: Job, out: &Path, rec: Option<&Recorder>) -> Result<Status> {
    if let Job::Probe { exit_code } = job {
        return Ok(Status {
            exit_code,
            probe: Some(crate::probe::run()?),
            ..Default::default()
        });
    }
    let mounts = Mounts::mount(&job, rec)?;
    match job {
        Job::Probe { .. } => unreachable!("handled above"),
        Job::Copy(job) => copy_job(&mounts, &job, out, rec),
        Job::Run(job) => crate::run::run_job(&mounts, &job, out, rec),
    }
}

fn copy_job(mounts: &Mounts, job: &CopyJob, out: &Path, rec: Option<&Recorder>) -> Result<Status> {
    let source = match &job.source {
        CopySource::Context => {
            mount(
                SHARE_CTX,
                CTX,
                "virtiofs",
                MountFlags::RDONLY,
                None::<&CStr>,
            )
            .context("mounting the build context")?;
            None
        }
        CopySource::Layers { lower } => {
            let (lowers, work) = (mounts.lower_dirs(lower)?, mounts.work("copy-src")?);
            // The host only trusts known phase names; the detail tells
            // this overlay from the destination's.
            Some(phase(rec, "overlay", Some("source"), || {
                ReadOnlyOverlay::mount(&lowers, &work)
            })?)
        }
    };
    let (lowers, work) = (mounts.lower_dirs(&job.lower)?, mounts.work("copy")?);
    let overlay = phase(rec, "overlay", None, || Overlay::mount(&lowers, &work))?;
    let copy = match &source {
        None => Copy {
            src_root: Path::new(CTX),
            src_name: "the build context",
            root: overlay.merged(),
            workdir: &job.workdir,
            owner: Owner::Fixed(0, 0),
        },
        Some(source) => Copy {
            src_root: source.merged(),
            src_name: "the source image",
            root: overlay.merged(),
            workdir: &job.workdir,
            owner: Owner::Source,
        },
    };
    let result = phase(rec, "copy", None, || {
        copy.ensure_workdir()
            .and_then(|()| copy.run(&job.sources, &job.dest))
    });
    let upper = overlay.unmount();
    result?;
    commit_upper(&upper?, out, rec)
}

/// Writes the step's layer from its upper dir to `/out`.
pub fn commit_upper(upper: &Path, out: &Path, rec: Option<&Recorder>) -> Result<Status> {
    let layer = phase(rec, "commit", None, || {
        commit::commit(upper, &out.join(LAYER_FILE))
    })?;
    Ok(Status {
        exit_code: 0,
        layer,
        ..Default::default()
    })
}

/// Times `f` as phase `name` when recording; plain call otherwise.
pub fn phase<T>(
    rec: Option<&Recorder>,
    name: &str,
    detail: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    match rec {
        Some(r) => r.phase(name, detail, f),
        None => f(),
    }
}

/// Copies the kernel's log ring buffer (one record per read, timestamped
/// from the kernel's first instruction) for boot-time diagnosis.
fn copy_kmsg(dest: &Path) -> std::io::Result<()> {
    use std::io::{ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let mut kmsg = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open("/dev/kmsg")?;
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; 8192];
    let mut written = 0u64;
    loop {
        match kmsg.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                written += n as u64;
                if written > MAX_KMSG_BYTES {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
            // Records overwritten while reading: skip ahead.
            Err(e) if e.kind() == ErrorKind::BrokenPipe => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    out.sync_all()
}

fn write_status(out: &Path, status: &Status) -> Result<()> {
    // Write-then-rename so the host never sees a partial status.
    let tmp = out.join(format!("{STATUS_FILE}.tmp"));
    let mut file = File::create(&tmp).context("creating status file")?;
    file.write_all(&serde_json::to_vec(status)?)
        .context("writing status")?;
    // The VM is torn down right after exit; make sure the host sees the bytes.
    file.sync_all().context("writing status")?;
    fs::rename(&tmp, out.join(STATUS_FILE)).context("writing status")?;
    Ok(())
}
