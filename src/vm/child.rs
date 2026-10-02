//! The `sandcastle __vm <job_dir>` process: configures libkrun and enters the VM.

use std::convert::Infallible;
use std::fs;
use std::path::Path;

use anyhow::Result;

use super::krun::Krun;
use super::{SPEC_FILE, VmSpec};
use crate::store::GUEST_HELPER_PATH;

/// Never returns on success (libkrun exits the process with the guest's code).
pub fn enter(job_dir: &Path) -> anyhow::Error {
    match try_enter(job_dir) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

fn try_enter(job_dir: &Path) -> Result<Infallible> {
    let spec: VmSpec = serde_json::from_slice(&fs::read(job_dir.join(SPEC_FILE))?)?;
    let krun = Krun::load(&spec.lib_dir)?;
    krun.set_log_level_error()?;
    let mut ctx = krun.create_ctx()?;
    ctx.set_vm_config(spec.vcpus, spec.ram_mib)?;
    ctx.set_root(&spec.guest_root)?;
    ctx.add_disk("store", &spec.disk, false)?;
    ctx.add_virtiofs("out", &spec.out_dir)?;
    ctx.set_workdir("/")?;
    ctx.set_exec(GUEST_HELPER_PATH, &[], &["HOME=/"])?;
    Err(ctx.start_enter())
}
