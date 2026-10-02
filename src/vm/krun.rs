//! Runtime-loaded bindings to the bundled libkrun (C API of v1.19.x).
//!
//! The only `unsafe` code in the host crate lives here. libkrun is opened
//! with `dlopen` from the bundle instead of being linked, so the host builds
//! and unit-tests on machines without libkrun.

use std::ffi::{CStr, CString, c_char};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use libloading::Library;

use crate::install::LIB_DIR_ENV;

#[cfg(target_os = "linux")]
pub const LIBKRUN_FILE: &str = "libkrun.so.1";
#[cfg(target_os = "macos")]
pub const LIBKRUN_FILE: &str = "libkrun.1.dylib";

/// `krun_set_log_level`: errors only.
const LOG_LEVEL_ERROR: u32 = 1;
/// libkrun reads exactly this many pointers from `argv`/`envp`
/// (`slice::from_raw_parts(ptr, MAX_ARGS)` in v1.19.6), so arrays are padded
/// with NULL to this length to keep its read in bounds.
const KRUN_MAX_ARGS: usize = 4096;

type CreateCtxFn = unsafe extern "C" fn() -> i32;
type SetLogLevelFn = unsafe extern "C" fn(u32) -> i32;
type SetVmConfigFn = unsafe extern "C" fn(u32, u8, u32) -> i32;
type SetPathFn = unsafe extern "C" fn(u32, *const c_char) -> i32;
type AddDiskFn = unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32;
type AddVirtiofsFn = unsafe extern "C" fn(u32, *const c_char, *const c_char) -> i32;
type SetExecFn =
    unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32;
type StartEnterFn = unsafe extern "C" fn(u32) -> i32;

pub struct Krun {
    create_ctx: CreateCtxFn,
    set_log_level: SetLogLevelFn,
    set_vm_config: SetVmConfigFn,
    set_root: SetPathFn,
    set_workdir: SetPathFn,
    add_disk: AddDiskFn,
    add_virtiofs: AddVirtiofsFn,
    set_exec: SetExecFn,
    start_enter: StartEnterFn,
    /// Keeps the function pointers above valid.
    _lib: Library,
}

impl Krun {
    pub fn load(lib_dir: &Path) -> Result<Self> {
        let path = lib_dir.join(LIBKRUN_FILE);
        // SAFETY: libkrun has no load-time initialisers with preconditions.
        let lib = unsafe { Library::new(path.as_os_str()) }.with_context(|| {
            format!(
                "cannot load {} (set {LIB_DIR_ENV} or run `just build-libs`)",
                path.display()
            )
        })?;
        // SAFETY: each requested type matches the declaration in
        // include/libkrun.h at v1.19.6.
        unsafe {
            Ok(Self {
                create_ctx: symbol(&lib, c"krun_create_ctx")?,
                set_log_level: symbol(&lib, c"krun_set_log_level")?,
                set_vm_config: symbol(&lib, c"krun_set_vm_config")?,
                set_root: symbol(&lib, c"krun_set_root")?,
                set_workdir: symbol(&lib, c"krun_set_workdir")?,
                add_disk: symbol(&lib, c"krun_add_disk")?,
                add_virtiofs: symbol(&lib, c"krun_add_virtiofs")?,
                set_exec: symbol(&lib, c"krun_set_exec")?,
                start_enter: symbol(&lib, c"krun_start_enter")?,
                _lib: lib,
            })
        }
    }

    pub fn set_log_level_error(&self) -> Result<()> {
        // SAFETY: integer argument only.
        check(
            unsafe { (self.set_log_level)(LOG_LEVEL_ERROR) },
            "krun_set_log_level",
        )
        .map(drop)
    }

    pub fn create_ctx(&self) -> Result<Ctx<'_>> {
        // SAFETY: no arguments.
        let id = check(unsafe { (self.create_ctx)() }, "krun_create_ctx")?;
        Ok(Ctx {
            krun: self,
            id: id as u32,
        })
    }
}

/// # Safety
/// `T` must be the exact function pointer type of the symbol `name`.
unsafe fn symbol<T: Copy>(lib: &Library, name: &CStr) -> Result<T> {
    // SAFETY: forwarded to the caller.
    let sym = unsafe { lib.get::<T>(name) }
        .with_context(|| format!("libkrun lacks {name:?}; is the bundle libkrun v1.19.x?"))?;
    Ok(*sym)
}

/// A libkrun configuration context. It is only built in the short-lived
/// `__vm` child, so a context dropped before `start_enter` is simply leaked.
pub struct Ctx<'k> {
    krun: &'k Krun,
    id: u32,
}

impl Ctx<'_> {
    pub fn set_vm_config(&mut self, vcpus: u8, ram_mib: u32) -> Result<()> {
        // SAFETY: integer arguments only.
        check(
            unsafe { (self.krun.set_vm_config)(self.id, vcpus, ram_mib) },
            "krun_set_vm_config",
        )
        .map(drop)
    }

    pub fn set_root(&mut self, dir: &Path) -> Result<()> {
        let dir = path_cstring(dir)?;
        // SAFETY: `dir` is NUL-terminated and outlives the call; libkrun copies it.
        check(
            unsafe { (self.krun.set_root)(self.id, dir.as_ptr()) },
            "krun_set_root",
        )
        .map(drop)
    }

    pub fn set_workdir(&mut self, dir: &str) -> Result<()> {
        let dir = CString::new(dir)?;
        // SAFETY: as in `set_root`.
        check(
            unsafe { (self.krun.set_workdir)(self.id, dir.as_ptr()) },
            "krun_set_workdir",
        )
        .map(drop)
    }

    pub fn add_disk(&mut self, block_id: &str, image: &Path, read_only: bool) -> Result<()> {
        let block_id = CString::new(block_id)?;
        let image = path_cstring(image)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; libkrun copies them.
        let rc =
            unsafe { (self.krun.add_disk)(self.id, block_id.as_ptr(), image.as_ptr(), read_only) };
        check(rc, "krun_add_disk").map(drop)
    }

    pub fn add_virtiofs(&mut self, tag: &str, dir: &Path) -> Result<()> {
        let tag = CString::new(tag)?;
        let dir = path_cstring(dir)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; libkrun copies them.
        check(
            unsafe { (self.krun.add_virtiofs)(self.id, tag.as_ptr(), dir.as_ptr()) },
            "krun_add_virtiofs",
        )
        .map(drop)
    }

    /// `env` must be given explicitly: a NULL `envp` makes libkrun copy the
    /// host environment into the guest.
    pub fn set_exec(&mut self, exec_path: &str, args: &[&str], env: &[&str]) -> Result<()> {
        let exec_path = CString::new(exec_path)?;
        let args = cstrings(args)?;
        let env = cstrings(env)?;
        let argv = padded_ptrs(&args)?;
        let envp = padded_ptrs(&env)?;
        // SAFETY: every pointer refers to a NUL-terminated string owned by
        // `args`/`env`, alive for the call; both arrays hold KRUN_MAX_ARGS
        // entries ending in NULL, matching libkrun's fixed-length read.
        let rc = unsafe {
            (self.krun.set_exec)(self.id, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr())
        };
        check(rc, "krun_set_exec").map(drop)
    }

    /// Boots the VM. On success libkrun never returns: it calls `exit()` with
    /// the guest's exit code. The returned error describes why it did not start.
    pub fn start_enter(self) -> anyhow::Error {
        // SAFETY: integer argument only; on success the call does not return.
        let rc = unsafe { (self.krun.start_enter)(self.id) };
        match check(rc, "krun_start_enter") {
            Err(e) => e,
            Ok(_) => anyhow!("krun_start_enter returned {rc} without starting the VM"),
        }
    }
}

fn check(rc: i32, call: &str) -> Result<i32> {
    if rc < 0 {
        Err(io::Error::from_raw_os_error(-rc)).with_context(|| format!("{call} failed"))
    } else {
        Ok(rc)
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains a NUL byte: {}", path.display()))
}

fn cstrings(items: &[&str]) -> Result<Vec<CString>> {
    items
        .iter()
        .map(|s| CString::new(*s).map_err(Into::into))
        .collect()
}

fn padded_ptrs(items: &[CString]) -> Result<Vec<*const c_char>> {
    anyhow::ensure!(
        items.len() < KRUN_MAX_ARGS,
        "too many arguments for libkrun"
    );
    let mut ptrs: Vec<*const c_char> = items.iter().map(|s| s.as_ptr()).collect();
    ptrs.resize(KRUN_MAX_ARGS, std::ptr::null());
    Ok(ptrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_error_names_the_file_and_the_override() {
        let dir = tempfile::tempdir().unwrap();
        let err = format!("{:#}", Krun::load(dir.path()).err().unwrap());
        assert!(err.contains(LIBKRUN_FILE), "{err}");
        assert!(err.contains("SANDCASTLE_LIBKRUN_DIR"), "{err}");
    }
}
