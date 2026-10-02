//! On-disk state: the ext4 store disk, the guest root shared as `/`, and
//! per-job directories. One process at a time: the ext4 disk cannot be
//! mounted by two VMs.

use std::fs::{self, File, Permissions, TryLockError};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail, ensure};

use crate::blobs::BlobStore;
use crate::install::Install;

pub use sandcastle_proto::GUEST_HELPER_PATH;
/// Mount points libkrun's init and the guest helper expect on the root share.
const GUEST_DIRS: &[&str] = &["dev", "proc", "sys", "tmp", "out", "store", "blobs", "ctx"];
/// Largest single template record accepted, bounding the read buffer.
const MAX_EXTENT: u32 = 64 << 20;
const TEMPLATE_MAGIC: &[u8; 4] = b"SCX1";

pub struct Store {
    root: PathBuf,
    next_job: AtomicU32,
    /// Held for the life of the store; released by the OS if we crash.
    _lock: File,
}

impl Store {
    pub fn open(root: &Path, install: &Install) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating store {}", root.display()))?;
        let root = fs::canonicalize(root)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another sandcastle process is using the store at {}",
                    root.display()
                )
            }
            Err(TryLockError::Error(e)) => return Err(e).context("locking the store"),
        }
        // `__vm` children inherit the lock, so a VM that outlives this
        // process keeps the store locked instead of sharing the ext4 disk
        // with the next build.
        rustix::io::fcntl_setfd(&lock, rustix::io::FdFlags::empty())
            .context("making the store lock inheritable")?;

        let jobs = root.join("jobs");
        if jobs.exists() {
            fs::remove_dir_all(&jobs).context("removing job directories left by an earlier run")?;
        }
        fs::create_dir_all(&jobs)?;

        let blobs = root.join("blobs");
        let blob_tmp = blobs.join("tmp");
        if blob_tmp.exists() {
            fs::remove_dir_all(&blob_tmp)
                .context("removing partial blobs left by an earlier run")?;
        }
        fs::create_dir_all(blobs.join("sha256"))?;
        fs::create_dir_all(&blob_tmp)?;

        let store = Self {
            root,
            next_job: AtomicU32::new(0),
            _lock: lock,
        };
        store.prepare_guest_root(&install.guest_bin)?;
        let disk = store.disk();
        if !disk.exists() {
            let template = install.store_template();
            let file =
                File::open(&template).with_context(|| format!("opening {}", template.display()))?;
            expand_sparse(zstd::Decoder::new(file)?, &disk)
                .with_context(|| format!("creating store disk {}", disk.display()))?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn disk(&self) -> PathBuf {
        self.root.join("store.ext4")
    }

    pub fn guest_root(&self) -> PathBuf {
        self.root.join("guest-root")
    }

    /// Creates `jobs/<n>/out/` and returns `jobs/<n>`.
    pub fn new_job_dir(&self) -> Result<PathBuf> {
        let n = self.next_job.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.join("jobs").join(n.to_string());
        fs::create_dir_all(dir.join("out"))?;
        Ok(dir)
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// The content-addressed blob store. Borrowing keeps the store lock held.
    pub fn blobs(&self) -> BlobStore<'_> {
        BlobStore::new(self.blobs_dir())
    }

    fn prepare_guest_root(&self, helper: &Path) -> Result<()> {
        let guest_root = self.guest_root();
        fs::create_dir_all(&guest_root)?;
        // The guest can write to its root, so never follow links planted there.
        for dir in GUEST_DIRS {
            let path = guest_root.join(dir);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => continue,
                Ok(_) => fs::remove_file(&path)?,
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            fs::create_dir(&path)?;
        }
        let want = fs::read(helper).with_context(|| format!("reading {}", helper.display()))?;
        let dest = guest_root.join(GUEST_HELPER_PATH.trim_start_matches('/'));
        let current = match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.is_file() => fs::read(&dest).ok(),
            _ => None,
        };
        if current.as_deref() != Some(&want[..]) {
            // Stage in the host-only store root; rename replaces a planted link.
            let tmp = self.root.join(".sandcastle-guest.tmp");
            fs::write(&tmp, &want)?;
            fs::set_permissions(&tmp, Permissions::from_mode(0o755))?;
            fs::rename(&tmp, &dest)?;
        }
        Ok(())
    }
}

/// Expands a decoded store template (see `scripts/pack-sparse.py`) into a
/// sparse file at `dest`. Only listed extents are written; the rest stays a hole.
pub fn expand_sparse(mut src: impl Read, dest: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    src.read_exact(&mut magic)
        .context("reading template header")?;
    ensure!(&magic == TEMPLATE_MAGIC, "not a sandcastle store template");
    let mut size = [0u8; 8];
    src.read_exact(&mut size)?;
    let size = u64::from_le_bytes(size);

    let tmp = dest.with_extension("partial");
    let result = write_extents(&mut src, &tmp, size);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn write_extents(src: &mut impl Read, path: &Path, size: u64) -> Result<()> {
    let out = File::create(path)?;
    out.set_len(size)?;
    let mut buf = Vec::new();
    loop {
        let mut header = [0u8; 12];
        // A clean end of stream is only allowed between records.
        match src.read(&mut header[..1]) {
            Ok(0) => break,
            Ok(_) => src
                .read_exact(&mut header[1..])
                .context("truncated template record")?,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let offset = u64::from_le_bytes(header[..8].try_into()?);
        let len = u32::from_le_bytes(header[8..].try_into()?);
        ensure!(len <= MAX_EXTENT, "template extent too large ({len} bytes)");
        ensure!(
            offset
                .checked_add(u64::from(len))
                .is_some_and(|end| end <= size),
            "template extent at {offset} (+{len}) is past the end of the {size}-byte disk"
        );
        buf.resize(len as usize, 0);
        src.read_exact(&mut buf)
            .context("truncated template extent")?;
        out.write_all_at(&buf, offset)?;
    }
    out.sync_all()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;

    use super::Store;
    use crate::install::Install;

    pub fn template(size: u64, extents: &[(u64, &[u8])]) -> Vec<u8> {
        let mut raw = b"SCX1".to_vec();
        raw.extend_from_slice(&size.to_le_bytes());
        for (off, data) in extents {
            raw.extend_from_slice(&off.to_le_bytes());
            raw.extend_from_slice(&(data.len() as u32).to_le_bytes());
            raw.extend_from_slice(data);
        }
        zstd::encode_all(&raw[..], 3).unwrap()
    }

    pub fn fixture() -> (tempfile::TempDir, Install) {
        let dir = tempfile::tempdir().unwrap();
        let lib_dir = dir.path().join("lib");
        fs::create_dir(&lib_dir).unwrap();
        fs::write(
            lib_dir.join("store-template.ext4.zst"),
            template(1 << 20, &[(0, b"superblock")]),
        )
        .unwrap();
        let guest_bin = dir.path().join("sandcastle-guest");
        fs::write(&guest_bin, b"helper v1").unwrap();
        (dir, Install { lib_dir, guest_bin })
    }

    /// An opened store backed by a tiny fake disk template and helper.
    pub fn store() -> (tempfile::TempDir, Store) {
        let (dir, install) = fixture();
        let store = Store::open(&dir.path().join("store"), &install).unwrap();
        (dir, store)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::test_support::{fixture, template};
    use super::*;

    #[test]
    fn expand_sparse_writes_extents_and_leaves_holes() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let size = 64 << 20;
        let src = template(size, &[(0, &[0xab; 4096]), (size - 4096, &[0xcd; 4096])]);
        expand_sparse(zstd::Decoder::new(&src[..]).unwrap(), &disk).unwrap();

        let bytes = fs::read(&disk).unwrap();
        assert_eq!(bytes.len() as u64, size);
        assert!(bytes[..4096].iter().all(|&b| b == 0xab));
        assert!(bytes[4096..(size - 4096) as usize].iter().all(|&b| b == 0));
        assert!(bytes[(size - 4096) as usize..].iter().all(|&b| b == 0xcd));
        let allocated = fs::metadata(&disk).unwrap().blocks() * 512;
        assert!(
            allocated < size / 4,
            "disk is not sparse: {allocated} bytes allocated"
        );
    }

    #[test]
    fn expand_sparse_rejects_extent_past_end() {
        let dir = tempfile::tempdir().unwrap();
        let src = template(4096, &[(4000, &[1; 200])]);
        let err = expand_sparse(zstd::Decoder::new(&src[..]).unwrap(), &dir.path().join("d"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("past the end"), "{err:#}");
        assert!(!dir.path().join("d").exists());
    }

    #[test]
    fn expand_sparse_rejects_oversized_extent() {
        let dir = tempfile::tempdir().unwrap();
        let mut raw = b"SCX1".to_vec();
        raw.extend_from_slice(&(1u64 << 40).to_le_bytes());
        raw.extend_from_slice(&0u64.to_le_bytes());
        raw.extend_from_slice(&((64u32 << 20) + 1).to_le_bytes());
        let dest = dir.path().join("d");
        let err = expand_sparse(&raw[..], &dest).unwrap_err();
        assert!(format!("{err:#}").contains("extent too large"), "{err:#}");
        assert!(!dest.exists());
    }

    #[test]
    fn open_replaces_planted_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let (dir, install) = fixture();
        let root = dir.path().join("store");
        drop(Store::open(&root, &install).unwrap());

        let victim = dir.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let guest_root = root.join("guest-root");
        let helper = guest_root.join("sandcastle-guest");
        fs::remove_file(&helper).unwrap();
        symlink(&victim, &helper).unwrap();
        fs::remove_dir(guest_root.join("tmp")).unwrap();
        symlink(&outside, guest_root.join("tmp")).unwrap();
        fs::write(&install.guest_bin, b"helper v2").unwrap();

        drop(Store::open(&root, &install).unwrap());

        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        let meta = fs::symlink_metadata(&helper).unwrap();
        assert!(meta.is_file(), "helper is not a regular file");
        assert_eq!(fs::read(&helper).unwrap(), b"helper v2");
        let tmp = fs::symlink_metadata(guest_root.join("tmp")).unwrap();
        assert!(tmp.is_dir() && !tmp.file_type().is_symlink());
    }

    #[test]
    fn second_open_fails_while_locked() {
        let (dir, install) = fixture();
        let root = dir.path().join("store");
        let _first = Store::open(&root, &install).unwrap();
        let err = Store::open(&root, &install).err().unwrap();
        assert!(
            format!("{err:#}").contains("another sandcastle process"),
            "{err:#}"
        );
    }

    #[test]
    fn open_removes_stale_job_dirs() {
        let (dir, install) = fixture();
        let root = dir.path().join("store");
        let stale = {
            let store = Store::open(&root, &install).unwrap();
            store.new_job_dir().unwrap()
        };
        fs::write(stale.join("out/status.json"), b"{}").unwrap();
        let _store = Store::open(&root, &install).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn open_refreshes_changed_guest_helper() {
        let (dir, install) = fixture();
        let root = dir.path().join("store");
        drop(Store::open(&root, &install).unwrap());
        fs::write(&install.guest_bin, b"helper v2").unwrap();
        let store = Store::open(&root, &install).unwrap();
        let copied = store
            .guest_root()
            .join(GUEST_HELPER_PATH.trim_start_matches('/'));
        assert_eq!(fs::read(copied).unwrap(), b"helper v2");
    }

    #[test]
    fn open_removes_partial_blobs() {
        let (dir, install) = fixture();
        let root = dir.path().join("store");
        let leftover = {
            let store = Store::open(&root, &install).unwrap();
            let temp = store.blobs().temp().unwrap();
            // Simulate a crash: the temp file is never committed or cleaned up.
            temp.into_temp_path().keep().unwrap()
        };
        assert!(leftover.exists());
        let _store = Store::open(&root, &install).unwrap();
        assert!(!leftover.exists());
    }
}
