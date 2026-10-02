//! Where sandcastle finds its bundled files and its store.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Overrides the bundled library directory (`<exe dir>/lib`).
pub const LIB_DIR_ENV: &str = "SANDCASTLE_LIBKRUN_DIR";
/// Overrides the guest helper path (`<exe dir>/sandcastle-guest`).
pub const GUEST_ENV: &str = "SANDCASTLE_GUEST";
/// Overrides the store root (`~/.local/share/sandcastle`).
pub const ROOT_ENV: &str = "SANDCASTLE_ROOT";

const STORE_TEMPLATE: &str = "store-template.ext4.zst";
const GUEST_BIN: &str = "sandcastle-guest";

/// Absolute paths of the files shipped next to the `sandcastle` executable.
#[derive(Debug, Clone)]
pub struct Install {
    pub lib_dir: PathBuf,
    pub guest_bin: PathBuf,
}

impl Install {
    pub fn locate(exe: &Path) -> Result<Self> {
        let exe_dir = exe
            .parent()
            .context("executable path has no parent directory")?;
        let lib_dir = env::var_os(LIB_DIR_ENV).map_or_else(|| exe_dir.join("lib"), PathBuf::from);
        let guest_bin =
            env::var_os(GUEST_ENV).map_or_else(|| exe_dir.join(GUEST_BIN), PathBuf::from);
        Ok(Self {
            lib_dir: fs::canonicalize(&lib_dir).with_context(|| {
                format!(
                    "library directory {} not found (set {LIB_DIR_ENV} or run `just build-libs`)",
                    lib_dir.display()
                )
            })?,
            guest_bin: fs::canonicalize(&guest_bin).with_context(|| {
                format!(
                    "guest helper {} not found (set {GUEST_ENV} or run `just build`)",
                    guest_bin.display()
                )
            })?,
        })
    }

    pub fn store_template(&self) -> PathBuf {
        self.lib_dir.join(STORE_TEMPLATE)
    }
}

pub fn store_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os(ROOT_ENV) {
        return Ok(root.into());
    }
    let home = env::var_os("HOME").with_context(|| format!("HOME is not set; set {ROOT_ENV}"))?;
    Ok(PathBuf::from(home).join(".local/share/sandcastle"))
}
