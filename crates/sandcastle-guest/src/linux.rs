use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process;
use std::time::Instant;

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{
    CopyJob, EVENTS_FILE, JOB_FILE, Job, LAYER_FILE, SHARE_CTX, SHARE_OUT, STATUS_FILE, Status,
};

use crate::commit;
use crate::copy::Copy;
use crate::events::Recorder;
use crate::overlay::Overlay;
use crate::store::Store;

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
    write_status(out, &status)?;
    Ok(status.exit_code)
}

fn read_job(out: &Path) -> Result<Job> {
    serde_json::from_slice(&fs::read(out.join(JOB_FILE)).context("reading job")?)
        .context("parsing job")
}

fn run_job(job: Job, out: &Path, rec: Option<&Recorder>) -> Result<Status> {
    match job {
        Job::Probe { exit_code } => Ok(Status {
            exit_code,
            probe: Some(crate::probe::run()?),
            ..Default::default()
        }),
        Job::Copy(job) => with_store(rec, |store| copy_job(store, &job, out, rec)),
        Job::Run(job) => with_store(rec, |store| crate::run::run_job(store, &job, out, rec)),
    }
}

/// Mounts the store for `f` and always unmounts it, so written layers reach
/// the disk before the VM stops.
pub fn with_store(
    rec: Option<&Recorder>,
    f: impl FnOnce(&Store) -> Result<Status>,
) -> Result<Status> {
    let store = phase(rec, "store mount", None, Store::mount)?;
    let result = f(&store);
    let unmounted = phase(rec, "unmount", None, || store.unmount());
    let status = result?;
    unmounted?;
    Ok(status)
}

fn copy_job(store: &Store, job: &CopyJob, out: &Path, rec: Option<&Recorder>) -> Result<Status> {
    mount(
        SHARE_CTX,
        CTX,
        "virtiofs",
        MountFlags::RDONLY,
        None::<&CStr>,
    )
    .context("mounting the build context")?;
    store.ensure_layers(&job.lower, rec)?;
    let work = store.work("copy")?;
    let lowers = store.lower_dirs(&job.lower)?;
    let overlay = phase(rec, "overlay", None, || Overlay::mount(&lowers, &work))?;
    let copy = Copy {
        ctx: Path::new(CTX),
        root: overlay.merged(),
        workdir: &job.workdir,
        owner: Some((0, 0)),
    };
    let result = phase(rec, "copy", None, || {
        copy.ensure_workdir()
            .and_then(|()| copy.run(&job.sources, &job.dest))
    });
    let upper = overlay.unmount()?;
    result?;
    commit_upper(store, &upper, out, rec)
}

/// Writes the step's layer and keeps its upper dir as the layer's store copy.
pub fn commit_upper(
    store: &Store,
    upper: &Path,
    out: &Path,
    rec: Option<&Recorder>,
) -> Result<Status> {
    let layer = phase(rec, "commit", None, || {
        commit::commit(upper, &out.join(LAYER_FILE))
    })?;
    match &layer {
        Some(diff_id) => {
            let dest = store.layer_dir(diff_id)?;
            if dest.exists() {
                fs::remove_dir_all(upper)?;
            } else {
                // A layer directory must be complete on disk once it exists.
                phase(rec, "sync", None, || store.sync())?;
                fs::rename(upper, &dest)?;
            }
        }
        None => fs::remove_dir(upper)?,
    }
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
