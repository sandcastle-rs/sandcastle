//! Layer images: each layer as a read-only erofs image at
//! `<store>/images/<diff_id hex>.erofs`, the form step VMs read layers in.
//! The bundled `mkfs.erofs` builds an image from the layer's uncompressed
//! tar, streamed on its stdin; nothing is unpacked on the host.

use std::fs::{self, File};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use oci_spec::image::{Descriptor, Digest, DigestAlgorithm, MediaType};
use tempfile::NamedTempFile;

use crate::blobs::{BlobStore, digest_from};
use crate::image::ConfigState;
use crate::store::Store;

/// Converter arguments: build from a tar on stdin (`--tar=f`), turn OCI
/// whiteouts into overlayfs ones (`--aufs`), fixed times and no UUID so
/// equal tars give equal images (`-T0 -Uclear`), and 4 KiB blocks: the
/// default is the host page size (16 KiB on Apple silicon), which the
/// guest kernel refuses to mount.
const CONVERTER_ARGS: [&str; 6] = ["--quiet", "--tar=f", "--aufs", "-T0", "-Uclear", "-b4096"];
/// Converter stderr kept for error messages.
const MAX_STDERR: u64 = 16 << 10;
const CHUNK: usize = 1 << 20;

pub struct LayerImages<'s> {
    dir: PathBuf,
    converter: PathBuf,
    _store: PhantomData<&'s Store>,
}

impl LayerImages<'_> {
    pub(crate) fn new(dir: PathBuf, converter: PathBuf) -> Self {
        Self {
            dir,
            converter,
            _store: PhantomData,
        }
    }

    /// Path of a layer's image, whether or not it exists.
    pub fn path(&self, diff_id: &Digest) -> Result<PathBuf> {
        ensure!(
            matches!(diff_id.algorithm(), DigestAlgorithm::Sha256),
            "unsupported digest algorithm in {diff_id}"
        );
        Ok(self.dir.join(format!("{}.erofs", diff_id.digest())))
    }

    /// Builds the image of layer `diff_id` from the tar `open_tar` returns,
    /// unless it exists. Nothing is stored unless the tar's digest is
    /// `diff_id`, so a present image always matches its name.
    pub fn ensure<R: Read>(
        &self,
        diff_id: &Digest,
        open_tar: impl FnOnce() -> Result<R>,
    ) -> Result<PathBuf> {
        let path = self.path(diff_id)?;
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return Ok(path);
        }
        let temp = NamedTempFile::new_in(self.dir.join("tmp")).context("creating a layer image")?;
        let digest = convert(&self.converter, open_tar()?, temp.path())
            .with_context(|| format!("converting layer {diff_id} to an erofs image"))?;
        ensure!(
            &digest == diff_id,
            "layer {diff_id}: its tar has digest {digest}"
        );
        temp.as_file().sync_all()?;
        temp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("storing the image of layer {diff_id}"))?;
        Ok(path)
    }
}

impl LayerImages<'_> {
    /// Builds the image of every layer of `state` from its blob in `blobs`.
    pub fn ensure_state(&self, blobs: &BlobStore<'_>, state: &ConfigState) -> Result<()> {
        for (layer, diff_id) in state.layers.iter().zip(&state.diff_ids) {
            self.ensure(diff_id, || layer_tar(blobs, layer))?;
        }
        Ok(())
    }
}

/// The uncompressed tar of a layer blob in `blobs`.
fn layer_tar(blobs: &BlobStore<'_>, layer: &Descriptor) -> Result<Box<dyn Read>> {
    let gzip = match layer.media_type() {
        MediaType::ImageLayer => false,
        MediaType::ImageLayerGzip => true,
        other => bail!(
            "layer {} is {other}; sandcastle can only read uncompressed and gzip layers (zstd is not supported yet)",
            layer.digest()
        ),
    };
    let file = BufReader::with_capacity(CHUNK, File::open(blobs.path(layer.digest())?)?);
    Ok(if gzip {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    })
}

/// Runs `converter` to write the image of `tar` at `dest` and returns the
/// tar's digest. A root directory entry goes first: a tar without one
/// would get the image root from the user running the converter, and the
/// tar's own root entry, if any, comes later and wins.
fn convert(converter: &Path, mut tar: impl Read, dest: &Path) -> Result<Digest> {
    let mut child = Command::new(converter)
        .args(CONVERTER_ARGS)
        .arg(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", converter.display()))?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stderr).take(MAX_STDERR).read_to_string(&mut text);
        // Keep draining so the converter never blocks on a full pipe.
        let _ = io::copy(&mut stderr, &mut io::sink());
        text
    });
    let mut stdin = Some(child.stdin.take().expect("stdin is piped"));
    let mut hasher = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let fed = (|| -> io::Result<()> {
        feed(&mut stdin, &root_header())?;
        let mut buf = vec![0; CHUNK];
        loop {
            let n = match tar.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            hasher.update(&buf[..n]);
            feed(&mut stdin, &buf[..n])?;
        }
    })();
    drop(stdin);
    let status = child.wait()?;
    let stderr = reader.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "{} failed ({status}): {}",
            converter.display(),
            stderr.trim()
        );
    }
    fed.context("reading the layer tar")?;
    let hash = hasher.finish();
    Ok(digest_from(
        hash.as_ref().try_into().expect("SHA-256 is 32 bytes"),
    ))
}

/// Writes to the converter until it stops reading: it may exit after the
/// tar's end-of-archive blocks, and its exit status then decides.
fn feed(stdin: &mut Option<std::process::ChildStdin>, bytes: &[u8]) -> io::Result<()> {
    if let Some(pipe) = stdin {
        match pipe.write_all(bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => *stdin = None,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A ustar header for the directory `./`: mode 0755, owner 0:0, time 0.
fn root_header() -> [u8; 512] {
    let mut h = [0u8; 512];
    let mut put =
        |offset: usize, field: &[u8]| h[offset..offset + field.len()].copy_from_slice(field);
    put(0, b"./");
    put(100, b"0000755\0");
    put(108, b"0000000\0");
    put(116, b"0000000\0");
    put(124, b"00000000000\0");
    put(136, b"00000000000\0");
    put(156, b"5");
    put(257, b"ustar\0");
    put(263, b"00");
    // The checksum is computed with its own field as spaces.
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn zstd_layers_are_refused_before_their_blob_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().to_path_buf());
        let digest = Digest::from_str(&format!("sha256:{}", "ab".repeat(32))).unwrap();
        let layer = Descriptor::new(MediaType::ImageLayerZstd, 1, digest);
        let err = layer_tar(&blobs, &layer).err().unwrap();
        assert!(
            format!("{err:#}").contains("zstd is not supported"),
            "{err:#}"
        );
    }

    #[test]
    fn root_header_is_a_root_owned_0755_directory() {
        let header = root_header();
        let mut archive = tar::Archive::new(&header[..]);
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        let h = entry.header();
        assert_eq!(h.entry_type(), tar::EntryType::Directory);
        assert_eq!(entry.path().unwrap(), Path::new("./"));
        assert_eq!(
            (h.mode().unwrap(), h.uid().unwrap(), h.gid().unwrap()),
            (0o755, 0, 0)
        );
        assert_eq!((h.size().unwrap(), h.mtime().unwrap()), (0, 0));
    }
}
