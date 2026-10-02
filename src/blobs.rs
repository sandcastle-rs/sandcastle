//! Content-addressed blob store at `<store>/blobs/sha256/<hex>`, shared by
//! registry pulls and locally built layers. Borrowing it from [`Store`] keeps
//! the store lock held for as long as blobs are written.

use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use oci_spec::image::{Descriptor, Digest, DigestAlgorithm, MediaType};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::store::Store;

pub struct BlobStore<'s> {
    dir: PathBuf,
    _store: PhantomData<&'s Store>,
}

impl BlobStore<'_> {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            _store: PhantomData,
        }
    }

    /// Path of a blob, whether or not it exists. Only sha256 is stored.
    pub fn path(&self, digest: &Digest) -> Result<PathBuf> {
        ensure!(
            matches!(digest.algorithm(), DigestAlgorithm::Sha256),
            "unsupported digest algorithm in {digest}"
        );
        Ok(self.dir.join("sha256").join(digest.digest()))
    }

    pub fn contains(&self, digest: &Digest) -> Result<bool> {
        Ok(fs::symlink_metadata(self.path(digest)?).is_ok_and(|m| m.is_file()))
    }

    /// A temporary file inside the store. It is deleted unless committed;
    /// leftovers from a crash are removed by the next `Store::open`.
    pub fn temp(&self) -> Result<NamedTempFile> {
        NamedTempFile::new_in(self.dir.join("tmp")).context("creating a temporary blob")
    }

    /// Moves a fully written and verified temp file to its content address.
    pub fn commit(&self, temp: NamedTempFile, digest: &Digest) -> Result<PathBuf> {
        let path = self.path(digest)?;
        temp.as_file().sync_all()?;
        temp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("storing blob {digest}"))?;
        Ok(path)
    }

    pub fn put_bytes(&self, media_type: MediaType, bytes: &[u8]) -> Result<Descriptor> {
        let digest = sha256(bytes);
        if !self.contains(&digest)? {
            let mut temp = self.temp()?;
            temp.write_all(bytes)?;
            self.commit(temp, &digest)?;
        }
        Ok(Descriptor::new(media_type, bytes.len() as u64, digest))
    }
}

pub fn sha256(bytes: &[u8]) -> Digest {
    digest_from(&Sha256::digest(bytes).into())
}

/// Turns a finished SHA-256 hash into an OCI digest.
pub fn digest_from(hash: &[u8; 32]) -> Digest {
    let mut s = String::with_capacity(7 + 2 * hash.len());
    s.push_str("sha256:");
    for b in hash {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
    }
    Digest::from_str(&s).expect("a sha256 hex string is a valid digest")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::store::test_support;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn put_bytes_stores_blob_at_its_digest() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let desc = blobs.put_bytes(MediaType::ImageConfig, b"abc").unwrap();
        assert_eq!(desc.size(), 3);
        assert_eq!(desc.media_type(), &MediaType::ImageConfig);
        let path = blobs.path(desc.digest()).unwrap();
        assert!(path.ends_with(
            "blobs/sha256/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert_eq!(fs::read(path).unwrap(), b"abc");
        assert!(blobs.contains(desc.digest()).unwrap());
    }

    #[test]
    fn uncommitted_temp_is_not_a_blob() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let digest = sha256(b"partial");
        let mut temp = blobs.temp().unwrap();
        temp.write_all(b"part").unwrap();
        drop(temp);
        assert!(!blobs.contains(&digest).unwrap());
        assert!(!blobs.path(&digest).unwrap().exists());
    }
}
