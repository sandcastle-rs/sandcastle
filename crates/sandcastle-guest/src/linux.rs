use std::ffi::CStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::{fs, process};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{JOB_FILE, Job, STATUS_FILE, Status};

const OUT: &str = "/out";

pub fn main() -> ! {
    match run() {
        Ok(code) => process::exit(code),
        Err(e) => {
            // No status file: the host reports a setup failure.
            eprintln!("sandcastle-guest: {e:#}");
            process::exit(125)
        }
    }
}

fn run() -> Result<i32> {
    mount("out", OUT, "virtiofs", MountFlags::empty(), None::<&CStr>)
        .context("mounting the out share")?;
    let out = Path::new(OUT);
    let job: Job = serde_json::from_slice(&fs::read(out.join(JOB_FILE)).context("reading job")?)
        .context("parsing job")?;
    let status = match job {
        Job::Probe { exit_code } => Status {
            exit_code,
            probe: Some(crate::probe::run()?),
            ..Default::default()
        },
        Job::Run(_) | Job::Copy(_) => anyhow::bail!("job not supported by this helper"),
    };
    // Write-then-rename so the host never sees a partial status.
    let tmp = out.join(format!("{STATUS_FILE}.tmp"));
    let mut file = File::create(&tmp).context("creating status file")?;
    file.write_all(&serde_json::to_vec(&status)?)
        .context("writing status")?;
    // The VM is torn down right after exit; make sure the host sees the bytes.
    file.sync_all().context("writing status")?;
    fs::rename(&tmp, out.join(STATUS_FILE)).context("writing status")?;
    Ok(status.exit_code)
}
