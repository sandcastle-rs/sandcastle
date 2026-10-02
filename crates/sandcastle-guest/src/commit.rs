//! Turns an overlay upper dir into an OCI layer tar, walking it in sorted
//! order so equal trees give equal diff_ids.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::fs::{lgetxattr, llistxattr, major, minor};

use crate::layer::{self, HashWriter, Kind, Meta, OPAQUE_XATTR, OVERLAY_XATTR_PREFIX};

const WRITE_BUFFER: usize = 1 << 20;
type Xattrs = Vec<(String, Vec<u8>)>;
/// Linux caps both an xattr name list and one value at 64 KiB.
const XATTR_MAX: usize = 1 << 16;

/// Writes `upper` as an uncompressed layer tar to `out` and returns its
/// diff_id, or `None` (and no file) when the step changed nothing.
pub fn commit(upper: &Path, out: &Path) -> Result<Option<String>> {
    if fs::read_dir(upper)?.next().is_none() {
        return Ok(None);
    }
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut tar = tar::Builder::new(HashWriter::new(BufWriter::with_capacity(
        WRITE_BUFFER,
        file,
    )));
    walk(&mut tar, upper, Path::new(""), &mut Scratch::new())?;
    let (buffered, diff_id) = tar.into_inner()?.finish();
    let file = buffered
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(Some(diff_id))
}

/// State shared across the walk: hardlink targets and xattr buffers, so
/// per-entry work is just the syscalls.
struct Scratch {
    links: HashMap<(u64, u64), PathBuf>,
    names: Vec<u8>,
    value: Vec<u8>,
}

impl Scratch {
    fn new() -> Self {
        Self {
            links: HashMap::new(),
            names: vec![0; XATTR_MAX],
            value: vec![0; XATTR_MAX],
        }
    }
}

fn walk<W: Write>(
    tar: &mut tar::Builder<W>,
    root: &Path,
    rel: &Path,
    scratch: &mut Scratch,
) -> Result<()> {
    let mut names: Vec<OsString> = fs::read_dir(root.join(rel))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let rel = rel.join(&name);
        let path = root.join(&rel);
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        let (opaque, xattrs) = read_xattrs(&path, &mut scratch.names, &mut scratch.value)?;
        let mut m = Meta {
            mode: meta.mode(),
            uid: meta.uid().into(),
            gid: meta.gid().into(),
            mtime: meta.mtime().max(0) as u64,
            size: 0,
            xattrs,
        };
        let kind = if ft.is_dir() {
            Kind::Dir { opaque }
        } else if ft.is_char_device() && meta.rdev() == 0 {
            Kind::Whiteout
        } else if ft.is_file() {
            if meta.nlink() > 1 {
                let key = (meta.dev(), meta.ino());
                if let Some(first) = scratch.links.get(&key) {
                    layer::append(tar, &rel, &Kind::Hardlink(first.clone()), &m, io::empty())?;
                    continue;
                }
                scratch.links.insert(key, rel.clone());
            }
            m.size = meta.len();
            layer::append(tar, &rel, &Kind::File, &m, File::open(&path)?)?;
            continue;
        } else if ft.is_symlink() {
            Kind::Symlink(fs::read_link(&path)?)
        } else if ft.is_char_device() {
            Kind::Char(major(meta.rdev()), minor(meta.rdev()))
        } else if ft.is_block_device() {
            Kind::Block(major(meta.rdev()), minor(meta.rdev()))
        } else if ft.is_fifo() {
            Kind::Fifo
        } else {
            eprintln!("sandcastle-guest: not committing socket /{}", rel.display());
            continue;
        };
        layer::append(tar, &rel, &kind, &m, io::empty())?;
        if ft.is_dir() {
            walk(tar, root, &rel, scratch)?;
        }
    }
    Ok(())
}

/// Returns whether the overlay marked the directory opaque, and the xattrs
/// that belong in the image.
fn read_xattrs(path: &Path, names: &mut [u8], value: &mut [u8]) -> Result<(bool, Xattrs)> {
    let len = match llistxattr(path, &mut *names) {
        Ok(len) => len,
        Err(rustix::io::Errno::NOTSUP) => return Ok((false, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("listing xattrs of {}", path.display())),
    };
    let mut opaque = false;
    let mut out = Vec::new();
    for name in names[..len].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        let n = lgetxattr(path, name.as_str(), &mut *value)?;
        if name == OPAQUE_XATTR {
            opaque = &value[..n] == b"y";
        } else if !name.starts_with(OVERLAY_XATTR_PREFIX) {
            out.push((name, value[..n].to_vec()));
        }
    }
    Ok((opaque, out))
}
