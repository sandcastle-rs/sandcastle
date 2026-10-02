//! Extracts a layer blob into an overlay-format directory: OCI `.wh.` files
//! become whiteout char devices, `.wh..wh..opq` the opaque xattr. The
//! uncompressed stream must hash to the layer's diff_id.

use std::fs::{self, File};
use std::io::{self, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::MultiGzDecoder;
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, Timespec, Timestamps, XattrFlags, chownat, lsetxattr, makedev,
    mknodat, utimensat,
};
use rustix::process::{Gid, Uid};
use sandcastle_proto::{LAYER_TAR, LAYER_TAR_GZIP};

use crate::layer::{
    self, HashReader, OPAQUE_XATTR, OVERLAY_XATTR_PREFIX, TarPath, XATTR_PAX_PREFIX,
};

const READ_BUFFER: usize = 1 << 20;

pub fn unpack(blob: &Path, media_type: &str, diff_id: &str, dest: &Path) -> Result<()> {
    let file = BufReader::with_capacity(READ_BUFFER, File::open(blob)?);
    let reader: Box<dyn Read> = match media_type {
        LAYER_TAR => Box::new(file),
        LAYER_TAR_GZIP => Box::new(MultiGzDecoder::new(file)),
        other => bail!("unsupported layer media type {other}"),
    };
    let mut hashing = HashReader::new(reader);
    fs::create_dir(dest)?;
    extract(&mut hashing, dest)?;
    // Bytes after the end-of-archive marker are part of the diff_id too.
    io::copy(&mut hashing, &mut io::sink())?;
    let (_, got) = hashing.finish();
    ensure!(
        got == diff_id,
        "layer decompressed to {got}, expected {diff_id}"
    );
    Ok(())
}

fn extract(reader: impl Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);
    // Creating children changes a directory's mtime; re-apply at the end.
    let mut dirs = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        match layer::classify(&raw)? {
            TarPath::Root => {}
            TarPath::Whiteout(target) => {
                let path = place(dest, &target)?;
                remove_existing(&path)?;
                mknodat(
                    CWD,
                    &path,
                    FileType::CharacterDevice,
                    Mode::empty(),
                    makedev(0, 0),
                )
                .with_context(|| format!("creating whiteout {}", target.display()))?;
            }
            TarPath::Opaque(dir) => {
                let path = mkdir_in(dest, &dir)?;
                lsetxattr(&path, OPAQUE_XATTR, b"y", XattrFlags::empty())?;
            }
            TarPath::Plain(rel) => {
                let kind = entry.header().entry_type();
                if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
                    special(&entry, dest, &rel)?;
                } else if !entry.unpack_in(dest)? {
                    bail!("layer entry {} leaves the layer root", raw.display());
                }
                let path = dest.join(&rel);
                set_xattrs(&mut entry, &path)?;
                if kind.is_dir() {
                    dirs.push((path, entry.header().mtime()?));
                }
            }
        }
    }
    for (dir, mtime) in dirs.iter().rev() {
        set_mtime(dir, *mtime)?;
    }
    Ok(())
}

/// Creates `rel` (relative to `dest`) as directories, refusing symlinks so
/// nothing is written outside the layer.
fn mkdir_in(dest: &Path, rel: &Path) -> Result<PathBuf> {
    let mut cur = dest.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!("layer path {} is not a directory", rel.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir(&cur)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(cur)
}

/// Path for a new entry `rel`, with its parent created inside `dest`.
fn place(dest: &Path, rel: &Path) -> Result<PathBuf> {
    let parent = mkdir_in(dest, rel.parent().unwrap_or(Path::new("")))?;
    Ok(parent.join(rel.file_name().context("layer entry has no name")?))
}

fn remove_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn special<R: Read>(entry: &tar::Entry<R>, dest: &Path, rel: &Path) -> Result<()> {
    let h = entry.header();
    let path = place(dest, rel)?;
    remove_existing(&path)?;
    let kind = h.entry_type();
    let (file_type, dev) = if kind.is_fifo() {
        (FileType::Fifo, 0)
    } else {
        let ft = if kind.is_character_special() {
            FileType::CharacterDevice
        } else {
            FileType::BlockDevice
        };
        let major = h.device_major()?.unwrap_or(0);
        let minor = h.device_minor()?.unwrap_or(0);
        (ft, makedev(major, minor))
    };
    let mode = Mode::from_raw_mode(h.mode()? & 0o7777);
    mknodat(CWD, &path, file_type, mode, dev)?;
    chownat(
        CWD,
        &path,
        Some(Uid::from_raw(h.uid()? as u32)),
        Some(Gid::from_raw(h.gid()? as u32)),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    // mknod applies the umask and chown clears setuid; set the mode last.
    rustix::fs::chmod(&path, mode)?;
    set_mtime(&path, h.mtime()?)
}

/// Applies `SCHILY.xattr.*` records except overlay bookkeeping ones, which
/// would change how the layer stack merges.
fn set_xattrs<R: Read>(entry: &mut tar::Entry<R>, path: &Path) -> Result<()> {
    let Some(exts) = entry.pax_extensions()? else {
        return Ok(());
    };
    for ext in exts {
        let ext = ext?;
        let Ok(key) = ext.key() else { continue };
        let Some(name) = key.strip_prefix(XATTR_PAX_PREFIX) else {
            continue;
        };
        if name.starts_with(OVERLAY_XATTR_PREFIX) {
            continue;
        }
        lsetxattr(path, name, ext.value_bytes(), XattrFlags::empty())
            .with_context(|| format!("setting xattr {name} on {}", path.display()))?;
    }
    Ok(())
}

fn set_mtime(path: &Path, mtime: u64) -> Result<()> {
    let t = Timespec {
        tv_sec: mtime as i64,
        tv_nsec: 0,
    };
    utimensat(
        CWD,
        path,
        &Timestamps {
            last_access: t,
            last_modification: t,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}
