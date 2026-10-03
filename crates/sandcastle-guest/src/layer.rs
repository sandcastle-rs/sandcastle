//! The OCI layer tar format as the guest reads and writes it: whiteout
//! names, tar entries for an overlay upper dir, and SHA-256 hashing of the
//! uncompressed stream (the diff_id). No Linux APIs, so it tests anywhere.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use tar::{EntryType, Header};

pub const WHITEOUT_PREFIX: &str = ".wh.";
pub const OPAQUE_MARKER: &str = ".wh..wh..opq";
/// Overlayfs marks an opaque directory with this xattr set to `y`.
pub const OPAQUE_XATTR: &str = "trusted.overlay.opaque";
/// Overlayfs bookkeeping xattrs; never part of an image.
pub const OVERLAY_XATTR_PREFIX: &str = "trusted.overlay.";
/// PAX record prefix for extended attributes (GNU tar, Docker).
pub const XATTR_PAX_PREFIX: &str = "SCHILY.xattr.";

/// What a layer tar path means once OCI whiteout names are decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum TarPath {
    /// The layer root itself (`./`); carries no change.
    Root,
    Plain(PathBuf),
    /// `dir/.wh.name`: `dir/name` is deleted.
    Whiteout(PathBuf),
    /// `dir/.wh..wh..opq`: lower contents of `dir` are hidden.
    Opaque(PathBuf),
}

/// Normalises a tar path to a relative one and decodes whiteout names.
/// Paths that climb out of the layer root are refused.
pub fn classify(raw: &Path) -> io::Result<TarPath> {
    let mut clean = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(name) => clean.push(name),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid(raw, "leaves the layer root"));
            }
        }
    }
    let Some(name) = clean.file_name() else {
        return Ok(TarPath::Root);
    };
    let Some(name) = name.to_str() else {
        return Ok(TarPath::Plain(clean));
    };
    if name == OPAQUE_MARKER {
        let dir = clean.parent().unwrap_or(Path::new("")).to_path_buf();
        return Ok(TarPath::Opaque(dir));
    }
    if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX) {
        if target.is_empty() || target == "." || target == ".." {
            return Err(invalid(raw, "is not a valid whiteout"));
        }
        return Ok(TarPath::Whiteout(clean.with_file_name(target)));
    }
    Ok(TarPath::Plain(clean))
}

fn invalid(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("layer entry {} {why}", path.display()),
    )
}

/// One upper-dir entry as it goes into the layer tar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir {
        opaque: bool,
    },
    File,
    Symlink(PathBuf),
    /// Target is a layer-relative path written earlier in the same tar.
    Hardlink(PathBuf),
    /// An overlay whiteout (char device 0/0).
    Whiteout,
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    /// Byte length of a regular file's contents; ignored for other kinds.
    pub size: u64,
    /// Extended attributes to record, overlay ones already removed.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

/// Appends `path` (relative to the layer root) in OCI form: whiteouts become
/// `.wh.<name>` files and an opaque directory is followed by `.wh..wh..opq`.
/// `data` is read only for [`Kind::File`].
pub fn append<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &Path,
    kind: &Kind,
    meta: &Meta,
    data: impl Read,
) -> io::Result<()> {
    if !meta.xattrs.is_empty() {
        let records: Vec<(String, Vec<u8>)> = meta
            .xattrs
            .iter()
            .map(|(k, v)| (format!("{XATTR_PAX_PREFIX}{k}"), v.clone()))
            .collect();
        let body = pax_records(&records);
        let mut h = Header::new_ustar();
        h.set_entry_type(EntryType::XHeader);
        h.set_path("././@PaxHeader")?;
        h.set_mode(0o644);
        h.set_size(body.len() as u64);
        h.set_cksum();
        tar.append(&h, &body[..])?;
    }
    match kind {
        Kind::Dir { opaque } => {
            let mut h = header(EntryType::Directory, meta, 0);
            tar.append_data(&mut h, path, io::empty())?;
            if *opaque {
                let mut h = header(EntryType::Regular, meta, 0);
                tar.append_data(&mut h, path.join(OPAQUE_MARKER), io::empty())?;
            }
        }
        Kind::File => {
            let mut h = header(EntryType::Regular, meta, meta.size);
            tar.append_data(&mut h, path, data)?;
        }
        Kind::Symlink(target) => {
            let mut h = header(EntryType::Symlink, meta, 0);
            tar.append_link(&mut h, path, target)?;
        }
        Kind::Hardlink(target) => {
            let mut h = header(EntryType::Link, meta, 0);
            tar.append_link(&mut h, path, target)?;
        }
        Kind::Whiteout => {
            let name = path
                .file_name()
                .ok_or_else(|| invalid(path, "has no name"))?;
            let mut wh = OsString::from(WHITEOUT_PREFIX);
            wh.push(name);
            let mut h = header(EntryType::Regular, meta, 0);
            tar.append_data(&mut h, path.with_file_name(wh), io::empty())?;
        }
        Kind::Char(major, minor) | Kind::Block(major, minor) => {
            let ty = if matches!(kind, Kind::Char(..)) {
                EntryType::Char
            } else {
                EntryType::Block
            };
            let mut h = header(ty, meta, 0);
            h.set_device_major(*major)?;
            h.set_device_minor(*minor)?;
            tar.append_data(&mut h, path, io::empty())?;
        }
        Kind::Fifo => {
            let mut h = header(EntryType::Fifo, meta, 0);
            tar.append_data(&mut h, path, io::empty())?;
        }
    }
    Ok(())
}

fn header(ty: EntryType, meta: &Meta, size: u64) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(ty);
    h.set_mode(meta.mode & 0o7777);
    h.set_uid(meta.uid);
    h.set_gid(meta.gid);
    h.set_mtime(meta.mtime);
    h.set_size(size);
    h
}

/// PAX extended header body: `"<len> <key>=<value>\n"` per record, where
/// `<len>` is the byte length of the whole record including its own digits.
fn pax_records(records: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in records {
        let rest = 1 + key.len() + 1 + value.len() + 1;
        let mut len = rest + 1;
        while len != rest + len.to_string().len() {
            len = rest + len.to_string().len();
        }
        out.extend_from_slice(format!("{len} {key}=").as_bytes());
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

/// Passes bytes through to `inner`, hashing them.
pub struct HashWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W> HashWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// Returns the inner writer and `sha256:<hex>` of everything written.
    pub fn finish(self) -> (W, String) {
        (self.inner, to_digest(self.hasher))
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Passes bytes through from `inner`, hashing them.
pub struct HashReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> HashReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finish(self) -> (R, String) {
        (self.inner, to_digest(self.hasher))
    }
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

fn to_digest(hasher: Sha256) -> String {
    let mut s = String::from("sha256:");
    for b in hasher.finalize() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The hex part of a `sha256:<64 lowercase hex>` digest. Anything else is
/// refused, so a digest is always safe to use as a file name.
pub fn digest_hex(digest: &str) -> io::Result<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid digest {digest:?}"),
            )
        })
}

/// Overlay `lowerdir+` order for a bottom-first layer list: top-most first.
/// A layer listed twice is kept only at its top-most position; the lower
/// copy is fully covered by the upper one, so the merged view is the same.
pub fn overlay_stack(bottom_first: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    bottom_first
        .iter()
        .rev()
        .map(String::as_str)
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn classify_maps_oci_names() {
        let c = |p: &str| classify(Path::new(p)).unwrap();
        assert_eq!(c("./etc/.wh.motd"), TarPath::Whiteout("etc/motd".into()));
        assert_eq!(c("usr/.wh..wh..opq"), TarPath::Opaque("usr".into()));
        assert_eq!(c(".wh..wh..opq"), TarPath::Opaque("".into()));
        assert_eq!(c("/abs/file"), TarPath::Plain("abs/file".into()));
        assert_eq!(c("./"), TarPath::Root);
        assert!(classify(Path::new("a/../../etc/passwd")).is_err());
        assert!(classify(Path::new("dir/.wh.")).is_err());
    }

    fn meta(mode: u32) -> Meta {
        Meta {
            mode,
            uid: 0,
            gid: 0,
            mtime: 1_700_000_000,
            size: 0,
            xattrs: vec![],
        }
    }

    #[test]
    fn append_writes_oci_entries_that_classify_back() {
        let mut tar = tar::Builder::new(Vec::new());
        let file = b"hello";
        append(
            &mut tar,
            Path::new("etc"),
            &Kind::Dir { opaque: true },
            &meta(0o755),
            io::empty(),
        )
        .unwrap();
        append(
            &mut tar,
            Path::new("etc/app.conf"),
            &Kind::File,
            &Meta {
                size: file.len() as u64,
                xattrs: vec![("security.capability".into(), vec![1, 2, 3])],
                ..meta(0o644)
            },
            &file[..],
        )
        .unwrap();
        append(
            &mut tar,
            Path::new("etc/gone"),
            &Kind::Whiteout,
            &meta(0),
            io::empty(),
        )
        .unwrap();
        append(
            &mut tar,
            Path::new("etc/link"),
            &Kind::Symlink("app.conf".into()),
            &meta(0o777),
            io::empty(),
        )
        .unwrap();
        append(
            &mut tar,
            Path::new("etc/hard"),
            &Kind::Hardlink("etc/app.conf".into()),
            &meta(0o644),
            io::empty(),
        )
        .unwrap();
        append(
            &mut tar,
            Path::new("dev/null"),
            &Kind::Char(1, 3),
            &meta(0o666),
            io::empty(),
        )
        .unwrap();
        let bytes = tar.into_inner().unwrap();

        let mut seen = Vec::new();
        let mut archive = tar::Archive::new(&bytes[..]);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let kind = entry.header().entry_type();
            if path == Path::new("etc/app.conf") {
                let caps = entry
                    .pax_extensions()
                    .unwrap()
                    .unwrap()
                    .map(|e| e.unwrap())
                    .find(|e| e.key().unwrap() == "SCHILY.xattr.security.capability")
                    .unwrap()
                    .value_bytes()
                    .to_vec();
                assert_eq!(caps, [1, 2, 3]);
                let mut body = Vec::new();
                entry.read_to_end(&mut body).unwrap();
                assert_eq!(body, file);
            }
            if path == Path::new("dev/null") {
                assert_eq!(entry.header().device_major().unwrap(), Some(1));
                assert_eq!(entry.header().device_minor().unwrap(), Some(3));
            }
            seen.push((classify(&path).unwrap(), kind));
        }
        use tar::EntryType as E;
        assert_eq!(
            seen,
            [
                (TarPath::Plain("etc".into()), E::Directory),
                (TarPath::Opaque("etc".into()), E::Regular),
                (TarPath::Plain("etc/app.conf".into()), E::Regular),
                (TarPath::Whiteout("etc/gone".into()), E::Regular),
                (TarPath::Plain("etc/link".into()), E::Symlink),
                (TarPath::Plain("etc/hard".into()), E::Link),
                (TarPath::Plain("dev/null".into()), E::Char),
            ]
        );
    }

    #[test]
    fn pax_record_length_counts_itself() {
        for len in 0..300 {
            let value = vec![b'v'; len];
            let record = pax_records(&[("SCHILY.xattr.user.k".into(), value)]);
            let (prefix, _) = record.split_at(record.iter().position(|&b| b == b' ').unwrap());
            let declared: usize = std::str::from_utf8(prefix).unwrap().parse().unwrap();
            assert_eq!(declared, record.len(), "value length {len}");
        }
    }

    #[test]
    fn hash_writer_gives_sha256_digest() {
        let mut w = HashWriter::new(Vec::new());
        w.write_all(b"abc").unwrap();
        let (inner, digest) = w.finish();
        assert_eq!(inner, b"abc");
        assert_eq!(
            digest,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digest_hex_rejects_other_shapes() {
        let ok = format!("sha256:{}", "a".repeat(64));
        assert_eq!(digest_hex(&ok).unwrap(), "a".repeat(64));
        assert!(digest_hex("sha256:../../etc").is_err());
        assert!(digest_hex(&format!("sha512:{}", "a".repeat(64))).is_err());
        assert!(digest_hex(&format!("sha256:{}", "A".repeat(64))).is_err());
    }

    #[test]
    fn overlay_stack_is_top_first_and_keeps_top_duplicate() {
        let ids: Vec<String> = ["a", "b", "a", "c"].map(String::from).to_vec();
        assert_eq!(overlay_stack(&ids), ["c", "a", "b"]);
    }
}
