# Registry Pull + OCI Image Writer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `sandcastle pull <ref> -o <dir>` pulls a base image anonymously into the store's content-addressed blob store and writes it back out as an OCI image layout that `skopeo` reads and verifies. The layout is built through the same config-state and layout-writer code that `sandcastle build` will use in plan 4.

**Architecture:**
- `BlobStore` (borrowed from the locked `Store`) owns `<store>/blobs/sha256/` with crash-safe temp files.
- `registry` is the only async module, running on a private Tokio runtime with `oci-client`. It resolves `linux/<host arch>`, verifies digests while streaming, and maps Docker media types to OCI.
- `image` holds the mutable `ConfigState`, a one-pass gzip + double-SHA-256 `LayerWriter`, and `write_layout`. `write_layout` always emits a fresh OCI config, manifest and index (canonical JSON). Layer blobs are reused byte for byte.

**Tech Stack:** Rust 2024, oci-client 0.18.0 (rustls), oci-spec 0.10.0 (image), tokio 1.53 (current-thread runtime), futures-util 0.3, flate2 1.1 (`zlib-rs` backend), sha2 0.11, tempfile 3.27 (now a normal dependency), skopeo for verification (ignored tests and CI only).

**Spec:** `docs/superpowers/specs/2026-10-02-sandcastle-v1-design.md`, sections "Host binary" (modules `registry`, `image`, `store`), "Data flow" steps 2 and 5, and Scope ("anonymous pulls only").

## Global Constraints

- Anonymous pulls only (`RegistryAuth::Anonymous`). No credential handling.
- Async (Tokio) only inside `src/registry.rs`; its public API is synchronous.
- Platform: `linux` + host architecture (`oci_spec::image::Arch::default()`). Never the client's default resolver, which matches the host OS (`darwin` on macOS).
- Blob store layout: `<store root>/blobs/sha256/<hex>`, temp files in `<store root>/blobs/tmp/`. `Store::open` empties `blobs/tmp/`. A blob only appears under its digest after a verified write plus an atomic rename.
- Output layout: `oci-layout` = `{"imageLayoutVersion":"1.0.0"}`, `index.json` with exactly one manifest annotated `org.opencontainers.image.ref.name`, and every blob under `blobs/sha256/`. Config and manifest media types are OCI (`application/vnd.oci.image.config.v1+json`, `application/vnd.oci.image.manifest.v1+json`). Layers use OCI layer media types.
- JSON written by sandcastle is canonical (keys sorted), so equal state gives equal digests.
- Layer hashing and compression is a hot path: one pass over the bytes. Compression is gzip at the default level via the `zlib-rs` backend, behind a 1 MiB `BufWriter`.
- No `unsafe` outside `src/vm/krun.rs`. Plan-2 public interfaces stay unchanged (only additions).
- Tests: no network in `cargo test`. Registry tests are `#[ignore]` and run via `just it-registry`, which needs network and `skopeo`. `just it` keeps running only the VM tests. No tautological tests. Never run the README conformance suite.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A pull killed mid-download** must never leave a blob that looks complete. The next `Store::open` must also remove the partial file. Pinned in Task 1: `uncommitted_temp_is_not_a_blob`, `open_removes_partial_blobs`.
2. **Docker schema2 images** (common on mirror.gcr.io and Docker Hub) must export with OCI layer media types. Foreign or non-distributable layers are rejected with a clear message. Pinned in Task 4: `layer_media_types_map_docker_to_oci`, `foreign_layers_are_rejected`.
3. **A multi-arch index pulled on macOS** must select `linux/arm64`, not a darwin, attestation (`unknown/unknown`) or amd64 entry. Pinned in Task 4: `select_platform_prefers_linux_host_arch`, `select_platform_ignores_attestations`.
4. **Re-running `pull -o` into an existing layout** must succeed and leave `index.json` pointing only at the new image. Pinned in Task 3: `rewriting_layout_points_index_at_latest_image`.
5. **A corrupt base image** whose config `diff_ids` count differs from the manifest layer count must be rejected before anything is exported. Pinned in Task 2: `from_base_rejects_layer_count_mismatch`.

---

## File Structure

```
Cargo.toml                    + oci-client, oci-spec, tokio, futures-util, flate2, sha2; tempfile → [dependencies]
src/lib.rs                    + pub mod blobs; image; registry
src/blobs.rs                  BlobStore: content addresses, temp files, commit, put_bytes; sha256 helpers
src/store.rs                  Store::open prepares blobs/; Store::blobs(); test_support module (moved fixtures)
src/image.rs                  ConfigState, LayerWriter, Layer, write_layout, canonical_json
src/registry.rs               pull (sync API over private Tokio runtime), select_platform, layer media types, default_tag
src/main.rs                   + `pull <ref> -o <dir> [--tag]`
tests/registry.rs             ignored network tests verified with skopeo
justfile                      `it` limited to VM tests; new `it-registry`
.github/workflows/ci.yml      + `registry` job (ubuntu, skopeo)
```

---

### Task 1: Blob store

**Files:**
- Create: `src/blobs.rs`
- Modify: `src/lib.rs`, `src/store.rs`, `Cargo.toml`

**Interfaces:**
- Consumes: `Store` (plan 2): `Store::open(root, &Install)`, private `root` field.
- Produces:
  - `blobs::BlobStore<'s>` with methods `path(&self, &Digest) -> Result<PathBuf>`, `contains(&self, &Digest) -> Result<bool>`, `temp(&self) -> Result<NamedTempFile>`, `commit(&self, NamedTempFile, &Digest) -> Result<PathBuf>`, `put_bytes(&self, MediaType, &[u8]) -> Result<Descriptor>`.
  - Free functions `blobs::sha256(&[u8]) -> Digest` and `blobs::digest_from(&[u8]) -> Digest`.
  - `Store::blobs(&self) -> BlobStore<'_>`.
  - `#[cfg(test)] store::test_support::{template, fixture, store}`.

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]` add:

```toml
flate2 = { version = "1.1.10", default-features = false, features = ["zlib-rs"] }
futures-util = "0.3.34"
oci-client = { version = "0.18.0", default-features = false, features = ["rustls-tls"] }
oci-spec = { version = "0.10.0", default-features = false, features = ["image"] }
sha2 = "0.11.0"
tempfile = "3.27.0"
tokio = { version = "1.53.1", features = ["rt", "fs", "io-util"] }
```

and remove `tempfile` from `[dev-dependencies]`, since it is now a normal dependency and dev-dependencies inherit it.

- [ ] **Step 2: Move the store test fixtures into a shared test module**

In `src/store.rs`, move the `template` and `fixture` functions out of `mod tests` into a new module placed just above `mod tests`, unchanged except for visibility and imports. Then add `store()`:

```rust
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
```

In `mod tests`, replace the removed definitions with `use super::test_support::{fixture, template};`. Keep the body of `fixture` exactly as it was in the old tests module. If it differs from the code above, copy the existing one.

Run: `cargo test -p sandcastle store::`
Expected: PASS. These are the existing 7 store tests, now using the moved fixtures.

- [ ] **Step 3: Write the failing tests**

`src/lib.rs`: add `pub mod blobs;` (alphabetical, first).

Append to the `mod tests` in `src/store.rs`:

```rust
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
```

`src/blobs.rs` (tests only first):

```rust
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
        assert!(path.ends_with("blobs/sha256/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
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
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p sandcastle blobs:: store::tests::open_removes_partial_blobs`
Expected: FAIL to compile with "cannot find function `sha256`" or "no method named `blobs`".

- [ ] **Step 5: Implement the blob store**

Prepend to `src/blobs.rs`:

```rust
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
        Self { dir, _store: PhantomData }
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
    digest_from(&Sha256::digest(bytes))
}

/// Turns a finished SHA-256 hash into an OCI digest.
pub fn digest_from(hash: &[u8]) -> Digest {
    let mut s = String::with_capacity(7 + 2 * hash.len());
    s.push_str("sha256:");
    for b in hash {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
    }
    Digest::from_str(&s).expect("a sha256 hex string is a valid digest")
}
```

In `src/store.rs`:
- add `use crate::blobs::BlobStore;`
- in `Store::open`, directly after the `jobs` block (`fs::create_dir_all(&jobs)?;`), insert:

```rust
        let blobs = root.join("blobs");
        let blob_tmp = blobs.join("tmp");
        if blob_tmp.exists() {
            fs::remove_dir_all(&blob_tmp).context("removing partial blobs left by an earlier run")?;
        }
        fs::create_dir_all(blobs.join("sha256"))?;
        fs::create_dir_all(&blob_tmp)?;
```

- add the method below `new_job_dir`:

```rust
    /// The content-addressed blob store. Borrowing keeps the store lock held.
    pub fn blobs(&self) -> BlobStore<'_> {
        BlobStore::new(self.root.join("blobs"))
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p sandcastle blobs:: store::`
Expected: PASS (3 blob tests and 8 store tests).

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src
git commit -m "Add content-addressed blob store to the sandcastle store" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Image config state and layer writer

**Files:**
- Create: `src/image.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `BlobStore::{temp, commit}`, `blobs::{digest_from, sha256}` (Task 1).
- Produces:
  - `image::Layer { pub descriptor: Descriptor, pub diff_id: Digest }`.
  - `image::LayerWriter<'b, 's>` (implements `std::io::Write`): `LayerWriter::new(&'b BlobStore<'s>) -> Result<Self>`, `finish(self) -> Result<Layer>`.
  - `image::ConfigState`, a struct with pub fields `architecture: Arch`, `os: Os`, `variant: Option<String>`, `os_version: Option<String>`, `env: Vec<String>`, `cmd: Option<Vec<String>>`, `entrypoint: Option<Vec<String>>`, `working_dir: Option<String>`, `user: Option<String>`, `labels: BTreeMap<String, String>`, `exposed_ports: BTreeSet<String>`, `volumes: BTreeSet<String>`, `stop_signal: Option<String>`, `history: Vec<History>`, `layers: Vec<Descriptor>`, `diff_ids: Vec<Digest>`.
  - `ConfigState` methods: `from_base(&ImageConfiguration, Vec<Descriptor>) -> Result<Self>`, `add_layer(&mut self, Layer, created_by: &str) -> Result<()>`, `add_empty(&mut self, created_by: &str) -> Result<()>`, `to_configuration(&self) -> Result<ImageConfiguration>`.

- [ ] **Step 1: Write the failing tests**

`src/lib.rs`: add `pub mod image;` (after `doctor`).

`src/image.rs` (tests only first):

```rust
#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};

    use super::*;
    use crate::blobs::sha256;
    use crate::store::test_support;

    /// A Docker-style config as served by registries (extra keys included).
    const DOCKER_CONFIG: &str = r#"{
        "architecture": "arm64",
        "variant": "v8",
        "os": "linux",
        "container_config": {"Hostname": "abc"},
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Cmd": ["sh"],
            "WorkingDir": "/srv",
            "User": "1000:1000",
            "Labels": {"org.example.b": "2", "org.example.a": "1"},
            "ExposedPorts": {"8080/tcp": {}},
            "StopSignal": "SIGQUIT"
        },
        "rootfs": {"type": "layers", "diff_ids": [
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]},
        "history": [
            {"created_by": "ADD rootfs.tar /"},
            {"created_by": "CMD [\"sh\"]", "empty_layer": true}
        ]
    }"#;

    fn base_layer() -> Descriptor {
        Descriptor::new(
            MediaType::ImageLayerGzip,
            10,
            Digest::from_str("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
        )
    }

    fn docker_config() -> ImageConfiguration {
        ImageConfiguration::from_reader(DOCKER_CONFIG.as_bytes()).unwrap()
    }

    #[test]
    fn from_base_reads_docker_config() {
        let state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        assert_eq!(state.architecture, Arch::ARM64);
        assert_eq!(state.variant.as_deref(), Some("v8"));
        assert_eq!(state.cmd, Some(vec!["sh".to_string()]));
        assert_eq!(state.working_dir.as_deref(), Some("/srv"));
        assert_eq!(state.user.as_deref(), Some("1000:1000"));
        assert_eq!(state.labels.keys().collect::<Vec<_>>(), ["org.example.a", "org.example.b"]);
        assert!(state.exposed_ports.contains("8080/tcp"));
        assert_eq!(state.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.diff_ids.len(), 1);
    }

    #[test]
    fn from_base_rejects_layer_count_mismatch() {
        let err = ConfigState::from_base(&docker_config(), vec![base_layer(), base_layer()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("2 layers") && msg.contains("1 diff_ids"), "{msg}");
    }

    #[test]
    fn to_configuration_keeps_every_field() {
        let state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        let again = ConfigState::from_base(&state.to_configuration().unwrap(), vec![base_layer()]).unwrap();
        assert_eq!(again, state);
    }

    #[test]
    fn layer_writer_hashes_tar_and_gzip_in_one_pass() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut writer = LayerWriter::new(&blobs).unwrap();
        for chunk in data.chunks(4096) {
            writer.write_all(chunk).unwrap();
        }
        let layer = writer.finish().unwrap();

        let blob = fs::read(blobs.path(layer.descriptor.digest()).unwrap()).unwrap();
        assert_eq!(layer.descriptor.size(), blob.len() as u64);
        assert_eq!(layer.descriptor.digest(), &sha256(&blob));
        assert_eq!(layer.descriptor.media_type(), &MediaType::ImageLayerGzip);
        assert_eq!(layer.diff_id, sha256(&data));
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(&blob[..]).read_to_end(&mut unpacked).unwrap();
        assert_eq!(unpacked, data);
    }

    #[test]
    fn add_layer_and_add_empty_record_history() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let mut state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        let mut writer = LayerWriter::new(&blobs).unwrap();
        writer.write_all(b"tar bytes").unwrap();
        let layer = writer.finish().unwrap();
        let diff_id = layer.diff_id.clone();
        state.add_layer(layer, "RUN true").unwrap();
        state.add_empty("ENV A=b").unwrap();
        assert_eq!(state.layers.len(), 2);
        assert_eq!(state.diff_ids.last(), Some(&diff_id));
        let last_two: Vec<_> = state.history[2..].iter().map(|h| (h.created_by().clone(), *h.empty_layer())).collect();
        assert_eq!(
            last_two,
            [(Some("RUN true".to_string()), None), (Some("ENV A=b".to_string()), Some(true))]
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle image::`
Expected: FAIL to compile with "cannot find type `ConfigState`".

- [ ] **Step 3: Implement config state and layer writer**

Prepend to `src/image.rs`:

```rust
//! OCI image assembly: the mutable configuration state of an image being
//! built, gzip layer blobs, and the OCI image layout written at the end.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use flate2::Compression;
use flate2::write::GzEncoder;
use oci_spec::image::{
    Arch, ConfigBuilder, Descriptor, Digest, History, HistoryBuilder, ImageConfiguration,
    ImageConfigurationBuilder, MediaType, Os, RootFsBuilder,
};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::blobs::{BlobStore, digest_from};

/// Output buffer for compressed layer bytes.
const LAYER_BUFFER: usize = 1 << 20;

/// A layer blob in the store and the digest of its uncompressed tar.
#[derive(Debug, Clone)]
pub struct Layer {
    pub descriptor: Descriptor,
    pub diff_id: Digest,
}

/// Everything sandcastle tracks about an image while building it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    pub architecture: Arch,
    pub os: Os,
    pub variant: Option<String>,
    pub os_version: Option<String>,
    pub env: Vec<String>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub exposed_ports: BTreeSet<String>,
    pub volumes: BTreeSet<String>,
    pub stop_signal: Option<String>,
    pub history: Vec<History>,
    /// Layer blobs, bottom first; `diff_ids[i]` belongs to `layers[i]`.
    pub layers: Vec<Descriptor>,
    pub diff_ids: Vec<Digest>,
}

impl ConfigState {
    /// Starts from a base image's config and its manifest layers.
    pub fn from_base(config: &ImageConfiguration, layers: Vec<Descriptor>) -> Result<Self> {
        let diff_ids = config
            .rootfs()
            .diff_ids()
            .iter()
            .map(|d| Digest::from_str(d))
            .collect::<Result<Vec<_>, _>>()
            .context("base image config has an invalid diff_id")?;
        ensure!(
            diff_ids.len() == layers.len(),
            "base image is inconsistent: {} layers in the manifest but {} diff_ids in the config",
            layers.len(),
            diff_ids.len()
        );
        let c = config.config().clone().unwrap_or_default();
        Ok(Self {
            architecture: config.architecture().clone(),
            os: config.os().clone(),
            variant: config.variant().clone(),
            os_version: config.os_version().clone(),
            env: c.env().clone().unwrap_or_default(),
            cmd: c.cmd().clone(),
            entrypoint: c.entrypoint().clone(),
            working_dir: c.working_dir().clone(),
            user: c.user().clone(),
            labels: c.labels().clone().unwrap_or_default().into_iter().collect(),
            exposed_ports: c.exposed_ports().clone().unwrap_or_default().into_iter().collect(),
            volumes: c.volumes().clone().unwrap_or_default().into_iter().collect(),
            stop_signal: c.stop_signal().clone(),
            history: config.history().clone().unwrap_or_default(),
            layers,
            diff_ids,
        })
    }

    pub fn add_layer(&mut self, layer: Layer, created_by: &str) -> Result<()> {
        self.layers.push(layer.descriptor);
        self.diff_ids.push(layer.diff_id);
        self.history.push(HistoryBuilder::default().created_by(created_by).build()?);
        Ok(())
    }

    pub fn add_empty(&mut self, created_by: &str) -> Result<()> {
        self.history
            .push(HistoryBuilder::default().created_by(created_by).empty_layer(true).build()?);
        Ok(())
    }

    pub fn to_configuration(&self) -> Result<ImageConfiguration> {
        let mut config = ConfigBuilder::default();
        if !self.env.is_empty() {
            config = config.env(self.env.clone());
        }
        if let Some(cmd) = &self.cmd {
            config = config.cmd(cmd.clone());
        }
        if let Some(entrypoint) = &self.entrypoint {
            config = config.entrypoint(entrypoint.clone());
        }
        if let Some(dir) = &self.working_dir {
            config = config.working_dir(dir.clone());
        }
        if let Some(user) = &self.user {
            config = config.user(user.clone());
        }
        if !self.labels.is_empty() {
            config = config.labels(self.labels.clone().into_iter().collect::<HashMap<_, _>>());
        }
        if !self.exposed_ports.is_empty() {
            config = config.exposed_ports(self.exposed_ports.iter().cloned().collect::<Vec<_>>());
        }
        if !self.volumes.is_empty() {
            config = config.volumes(self.volumes.iter().cloned().collect::<Vec<_>>());
        }
        if let Some(signal) = &self.stop_signal {
            config = config.stop_signal(signal.clone());
        }
        let rootfs = RootFsBuilder::default()
            .typ("layers")
            .diff_ids(self.diff_ids.iter().map(ToString::to_string).collect::<Vec<_>>())
            .build()?;
        let mut image = ImageConfigurationBuilder::default()
            .architecture(self.architecture.clone())
            .os(self.os.clone())
            .config(config.build()?)
            .rootfs(rootfs)
            .history(self.history.clone());
        if let Some(variant) = &self.variant {
            image = image.variant(variant.clone());
        }
        if let Some(version) = &self.os_version {
            image = image.os_version(version.clone());
        }
        Ok(image.build()?)
    }
}

/// Passes bytes through to `inner` while hashing them.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W> Hashing<W> {
    fn new(inner: W) -> Self {
        Self { inner, hasher: Sha256::new(), len: 0 }
    }

    fn into_parts(self) -> (W, Digest, u64) {
        (self.inner, digest_from(&self.hasher.finalize()), self.len)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streams an uncompressed layer tar into a gzip blob. Each byte is hashed
/// once on the way in (diff_id) and once on the way out (blob digest).
pub struct LayerWriter<'b, 's> {
    blobs: &'b BlobStore<'s>,
    temp: NamedTempFile,
    sink: Hashing<GzEncoder<Hashing<BufWriter<File>>>>,
}

impl<'b, 's> LayerWriter<'b, 's> {
    pub fn new(blobs: &'b BlobStore<'s>) -> Result<Self> {
        let temp = blobs.temp()?;
        let file = BufWriter::with_capacity(LAYER_BUFFER, temp.as_file().try_clone()?);
        let sink = Hashing::new(GzEncoder::new(Hashing::new(file), Compression::default()));
        Ok(Self { blobs, temp, sink })
    }

    pub fn finish(self) -> Result<Layer> {
        let Self { blobs, temp, sink } = self;
        let (encoder, diff_id, _) = sink.into_parts();
        let (file, digest, size) = encoder.finish()?.into_parts();
        file.into_inner().map_err(io::IntoInnerError::into_error)?;
        blobs.commit(temp, &digest)?;
        Ok(Layer { descriptor: Descriptor::new(MediaType::ImageLayerGzip, size, digest), diff_id })
    }
}

impl Write for LayerWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle image::`
Expected: PASS (5 tests).

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "Add image config state and one-pass gzip layer writer" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: OCI layout writer

**Files:**
- Modify: `src/image.rs`

**Interfaces:**
- Consumes: `ConfigState`, `LayerWriter` (Task 2), `BlobStore::{put_bytes, path}` (Task 1).
- Produces: `image::write_layout(&BlobStore<'_>, &ConfigState, out_dir: &Path, ref_name: &str) -> Result<Descriptor>` (the manifest descriptor with its ref-name annotation); `image::canonical_json<T: Serialize>(&T) -> Result<Vec<u8>>`; `image::REF_NAME_ANNOTATION`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/image.rs`:

```rust
    use oci_spec::image::{ImageIndex, ImageManifest};

    fn built_state(blobs: &BlobStore<'_>, cmd: &str) -> ConfigState {
        let mut base = docker_config();
        base.rootfs_mut().set_diff_ids(vec![]);
        let mut state = ConfigState::from_base(&base, vec![]).unwrap();
        let mut writer = LayerWriter::new(blobs).unwrap();
        writer.write_all(b"layer contents").unwrap();
        state.add_layer(writer.finish().unwrap(), "RUN make").unwrap();
        state.cmd = Some(vec![cmd.to_string()]);
        state
    }

    /// Re-hashes a blob in the layout and checks it against its descriptor.
    fn read_verified(out: &Path, desc: &Descriptor) -> Vec<u8> {
        let bytes = fs::read(out.join("blobs/sha256").join(desc.digest().digest())).unwrap();
        assert_eq!(&sha256(&bytes), desc.digest());
        assert_eq!(bytes.len() as u64, desc.size());
        bytes
    }

    #[test]
    fn write_layout_produces_verifiable_layout() {
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let state = built_state(&blobs, "app");
        let out = dir.path().join("out layout");
        write_layout(&blobs, &state, &out, "demo").unwrap();

        let layout: serde_json::Value = serde_json::from_slice(&fs::read(out.join("oci-layout")).unwrap()).unwrap();
        assert_eq!(layout["imageLayoutVersion"], "1.0.0");
        let index = ImageIndex::from_file(out.join("index.json")).unwrap();
        assert_eq!(index.manifests().len(), 1);
        let entry = &index.manifests()[0];
        assert_eq!(entry.media_type(), &MediaType::ImageManifest);
        assert_eq!(entry.annotations().as_ref().unwrap()[REF_NAME_ANNOTATION], "demo");

        let manifest = ImageManifest::from_reader(&read_verified(&out, entry)[..]).unwrap();
        assert_eq!(manifest.config().media_type(), &MediaType::ImageConfig);
        let config = ImageConfiguration::from_reader(&read_verified(&out, manifest.config())[..]).unwrap();
        assert_eq!(config.rootfs().diff_ids(), &vec![state.diff_ids[0].to_string()]);
        assert_eq!(config.config().as_ref().unwrap().cmd(), &Some(vec!["app".to_string()]));
        assert_eq!(manifest.layers().len(), 1);
        read_verified(&out, &manifest.layers()[0]);
    }

    #[test]
    fn rewriting_layout_points_index_at_latest_image() {
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let out = dir.path().join("out");
        let first = write_layout(&blobs, &built_state(&blobs, "one"), &out, "demo").unwrap();
        let second = write_layout(&blobs, &built_state(&blobs, "two"), &out, "demo").unwrap();
        assert_ne!(first.digest(), second.digest());
        let index = ImageIndex::from_file(out.join("index.json")).unwrap();
        assert_eq!(index.manifests().len(), 1);
        assert_eq!(index.manifests()[0].digest(), second.digest());
        read_verified(&out, &index.manifests()[0]);
    }

    #[test]
    fn config_json_is_identical_for_identical_state() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let mut state = built_state(&blobs, "app");
        for i in 0..32 {
            state.labels.insert(format!("label.{i}"), i.to_string());
        }
        // Each conversion builds a new HashMap with its own random iteration order.
        let a = canonical_json(&state.to_configuration().unwrap()).unwrap();
        let b = canonical_json(&state.to_configuration().unwrap()).unwrap();
        assert_eq!(a, b);
    }
```

Add `use std::path::Path;` to the test module imports.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle image::`
Expected: FAIL to compile with "cannot find function `write_layout`".

- [ ] **Step 3: Implement the layout writer**

Change the `std::fs` import in `src/image.rs` to `use std::fs::{self, File};` and add `use std::path::Path;`, `use serde::Serialize;`, and `ImageIndexBuilder, ImageManifestBuilder` to the `oci_spec::image` list.

Append below the `LayerWriter` code:

```rust
/// Annotation tools such as skopeo and podman use to find an image in a layout.
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const SCHEMA_VERSION: u32 = 2;
const OCI_LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

/// Serializes with sorted object keys, so equal values give equal digests.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::to_value(value)?)?)
}

/// Writes `state` as the single image of an OCI layout at `out_dir`, named
/// `ref_name`. An existing layout is updated in place: blobs are content
/// addressed, and `index.json` is replaced to point at this image only.
pub fn write_layout(
    blobs: &BlobStore<'_>,
    state: &ConfigState,
    out_dir: &Path,
    ref_name: &str,
) -> Result<Descriptor> {
    let config = blobs.put_bytes(MediaType::ImageConfig, &canonical_json(&state.to_configuration()?)?)?;
    let manifest = ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .config(config.clone())
        .layers(state.layers.clone())
        .build()?;
    let mut manifest_desc = blobs.put_bytes(MediaType::ImageManifest, &canonical_json(&manifest)?)?;

    let out_blobs = out_dir.join("blobs").join("sha256");
    fs::create_dir_all(&out_blobs).with_context(|| format!("creating {}", out_blobs.display()))?;
    for desc in state.layers.iter().chain([&config, &manifest_desc]) {
        let dest = out_blobs.join(desc.digest().digest());
        if !dest.exists() {
            let src = blobs.path(desc.digest())?;
            // fs::copy clones on APFS and uses copy_file_range on Linux.
            let partial = dest.with_extension("partial");
            fs::copy(&src, &partial)
                .with_context(|| format!("copying blob {} into the layout", desc.digest()))?;
            fs::rename(&partial, &dest)?;
        }
    }

    write_atomic(&out_dir.join("oci-layout"), OCI_LAYOUT)?;
    manifest_desc.set_annotations(Some(HashMap::from([(
        REF_NAME_ANNOTATION.to_string(),
        ref_name.to_string(),
    )])));
    let index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![manifest_desc.clone()])
        .build()?;
    write_atomic(&out_dir.join("index.json"), &canonical_json(&index)?)?;
    Ok(manifest_desc)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", path.display()))?;
    fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle image::`
Expected: PASS (8 tests).

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "Write OCI image layouts with canonical config and manifest" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Registry pull

**Files:**
- Create: `src/registry.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `BlobStore::{contains, temp, commit}` (Task 1).
- Produces:
  - `registry::PulledImage { pub config: ImageConfiguration, pub layers: Vec<Descriptor> }`.
  - `registry::pull(&BlobStore<'_>, reference: &str) -> Result<PulledImage>`.
  - `registry::default_tag(reference: &str) -> Result<String>`.
  - Crate-private `select_platform(&[ImageIndexEntry], &Arch) -> Option<String>` and `layer_media_type(&str) -> Result<MediaType>`.

- [ ] **Step 1: Write the failing tests**

`src/lib.rs`: add `pub mod registry;` (after `install`).

`src/registry.rs` (tests only first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Index entries in the shape registries serve, including a BuildKit
    /// attestation manifest (`unknown/unknown`).
    fn entries() -> Vec<ImageIndexEntry> {
        serde_json::from_str(
            r#"[
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
             "platform": {"architecture": "amd64", "os": "linux"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
             "platform": {"architecture": "unknown", "os": "unknown"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
             "platform": {"architecture": "arm64", "os": "darwin"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:4444444444444444444444444444444444444444444444444444444444444444",
             "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}}
        ]"#,
        )
        .unwrap()
    }

    #[test]
    fn select_platform_prefers_linux_host_arch() {
        assert_eq!(select_platform(&entries(), &Arch::ARM64).unwrap(), format!("sha256:{}", "4".repeat(64)));
        assert_eq!(select_platform(&entries(), &Arch::Amd64).unwrap(), format!("sha256:{}", "1".repeat(64)));
    }

    #[test]
    fn select_platform_ignores_attestations() {
        let only_attestation: Vec<_> = entries().into_iter().filter(|e| e.digest.contains("2222")).collect();
        assert_eq!(select_platform(&only_attestation, &Arch::Amd64), None);
        assert_eq!(select_platform(&entries(), &Arch::RISCV64), None);
    }

    #[test]
    fn layer_media_types_map_docker_to_oci() {
        assert_eq!(layer_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip").unwrap(), MediaType::ImageLayerGzip);
        assert_eq!(layer_media_type("application/vnd.oci.image.layer.v1.tar+gzip").unwrap(), MediaType::ImageLayerGzip);
        assert_eq!(layer_media_type("application/vnd.oci.image.layer.v1.tar+zstd").unwrap(), MediaType::ImageLayerZstd);
        assert_eq!(layer_media_type("application/vnd.oci.image.layer.v1.tar").unwrap(), MediaType::ImageLayer);
    }

    #[test]
    fn foreign_layers_are_rejected() {
        let err = layer_media_type("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip").unwrap_err();
        assert!(format!("{err:#}").contains("non-distributable"), "{err:#}");
        let err = layer_media_type("application/x-unknown").unwrap_err();
        assert!(format!("{err:#}").contains("application/x-unknown"), "{err:#}");
    }

    #[test]
    fn default_tag_falls_back_to_latest() {
        assert_eq!(default_tag("mirror.gcr.io/library/busybox:1.36").unwrap(), "1.36");
        assert_eq!(default_tag("alpine").unwrap(), "latest");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle registry::`
Expected: FAIL to compile with "cannot find function `select_platform`".

- [ ] **Step 3: Implement the registry module**

Prepend to `src/registry.rs`:

```rust
//! Anonymous pulls from OCI registries into the blob store. This is the only
//! async code in sandcastle; it runs on a private current-thread Tokio runtime.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use oci_client::client::ClientConfig;
use oci_client::manifest::{ImageIndexEntry, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use oci_spec::image::{Arch, Descriptor, Digest, ImageConfiguration, MediaType, Os};

use crate::blobs::BlobStore;

/// Layer downloads in flight at once.
const PARALLEL_DOWNLOADS: usize = 4;

/// A base image whose layer blobs are all present in the blob store.
#[derive(Debug)]
pub struct PulledImage {
    pub config: ImageConfiguration,
    /// Manifest layers with OCI media types, bottom first.
    pub layers: Vec<Descriptor>,
}

/// Pulls the `linux/<host arch>` image `reference` and stores its layers.
pub fn pull(blobs: &BlobStore<'_>, reference: &str) -> Result<PulledImage> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference {reference:?}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime
        .block_on(pull_async(blobs, &parsed))
        .with_context(|| format!("pulling {reference}"))
}

/// The tag to name an exported image after: the reference's tag, or `latest`.
pub fn default_tag(reference: &str) -> Result<String> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference {reference:?}"))?;
    Ok(parsed.tag().unwrap_or("latest").to_string())
}

async fn pull_async(blobs: &BlobStore<'_>, reference: &Reference) -> Result<PulledImage> {
    let arch = Arch::default();
    let resolver_arch = arch.clone();
    let client = Client::new(ClientConfig {
        // The default resolver matches the host OS, which is `darwin` on macOS.
        platform_resolver: Some(Box::new(move |entries: &[ImageIndexEntry]| {
            select_platform(entries, &resolver_arch)
        })),
        ..Default::default()
    });
    let (manifest, _, config_json) = client
        .pull_manifest_and_config(reference, &RegistryAuth::Anonymous)
        .await
        .with_context(|| format!("fetching the linux/{arch} manifest and config"))?;
    let config = ImageConfiguration::from_reader(config_json.as_bytes()).context("parsing the image config")?;
    let layers = manifest.layers.iter().map(to_descriptor).collect::<Result<Vec<_>>>()?;

    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for (remote, local) in manifest.layers.iter().zip(&layers) {
        if blobs.contains(local.digest())? {
            eprintln!("{}: already present", local.digest());
        } else if seen.insert(local.digest().to_string()) {
            missing.push((remote, local));
        }
    }
    let results: Vec<Result<()>> = futures_util::stream::iter(missing)
        .map(|(remote, local)| fetch_layer(&client, reference, blobs, remote, local))
        .buffer_unordered(PARALLEL_DOWNLOADS)
        .collect()
        .await;
    results.into_iter().collect::<Result<()>>()?;
    Ok(PulledImage { config, layers })
}

async fn fetch_layer(
    client: &Client,
    reference: &Reference,
    blobs: &BlobStore<'_>,
    remote: &OciDescriptor,
    local: &Descriptor,
) -> Result<()> {
    let temp = blobs.temp()?;
    let file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
    // pull_blob verifies the digest while streaming and fails on a mismatch.
    client
        .pull_blob(reference, remote, file)
        .await
        .with_context(|| format!("downloading layer {}", local.digest()))?;
    blobs.commit(temp, local.digest())?;
    eprintln!("{}: downloaded ({} bytes)", local.digest(), local.size());
    Ok(())
}

/// Picks the `linux/<arch>` manifest from an image index.
pub(crate) fn select_platform(entries: &[ImageIndexEntry], arch: &Arch) -> Option<String> {
    entries
        .iter()
        .find(|e| {
            e.platform
                .as_ref()
                .is_some_and(|p| p.os == Os::Linux && &p.architecture == arch)
        })
        .map(|e| e.digest.clone())
}

fn to_descriptor(remote: &OciDescriptor) -> Result<Descriptor> {
    let digest = Digest::from_str(&remote.digest)
        .with_context(|| format!("invalid layer digest {:?}", remote.digest))?;
    let size = u64::try_from(remote.size).context("negative layer size")?;
    Ok(Descriptor::new(layer_media_type(&remote.media_type)?, size, digest))
}

/// Maps registry layer media types (Docker schema2 or OCI) to OCI ones.
pub(crate) fn layer_media_type(media_type: &str) -> Result<MediaType> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(MediaType::ImageLayerGzip),
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(MediaType::ImageLayerZstd),
        "application/vnd.oci.image.layer.v1.tar" => Ok(MediaType::ImageLayer),
        other if other.contains("foreign") || other.contains("nondistributable") => {
            bail!("non-distributable (foreign) layer {other} is not supported")
        }
        other => bail!("unsupported layer media type {other}"),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle registry::`
Expected: PASS (5 tests).

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "Pull linux/<host arch> images anonymously into the blob store" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: `sandcastle pull`, registry integration tests, CI

**Files:**
- Create: `tests/registry.rs`
- Modify: `src/main.rs`, `justfile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `registry::{pull, default_tag}`, `image::{ConfigState, write_layout}`, `Store::{open, blobs}`, `install::{Install, store_root}`.
- Produces: CLI `sandcastle pull <REFERENCE> -o <DIR> [--tag <NAME>]`, which prints `<manifest digest> <dir>:<tag>` on stdout and per-layer progress on stderr. Also `just it-registry` and the CI job `registry`.

`pull` is a real user command, not a test hook. It warms the blob cache and exports a base image. It exercises exactly the path `sandcastle build` will use (`registry::pull` → `ConfigState::from_base` → `write_layout`), so there is one export path.

- [ ] **Step 1: Write the ignored integration tests**

`tests/registry.rs`:

```rust
//! Registry integration tests: need network and skopeo. Run with `just it-registry`.

use std::path::Path;
use std::process::{Command, Output};

const IMAGE: &str = "mirror.gcr.io/library/busybox:1.36";

fn sandcastle(store: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sandcastle"))
        .args(args)
        .env("SANDCASTLE_ROOT", store)
        .output()
        .unwrap()
}

fn skopeo(args: &[&str]) -> Output {
    Command::new("skopeo")
        .args(args)
        .output()
        .expect("skopeo not found: install it (brew install skopeo / apt-get install skopeo)")
}

fn assert_ok(what: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs network and skopeo; run with `just it-registry`"]
fn pulled_layout_is_readable_by_skopeo() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("busybox layout");
    let out_arg = out.to_str().unwrap();
    assert_ok("pull", &sandcastle(&dir.path().join("store"), &["pull", IMAGE, "-o", out_arg, "--tag", "test"]));

    let layout = format!("oci:{out_arg}:test");
    let inspect = skopeo(&["inspect", &layout]);
    assert_ok("skopeo inspect", &inspect);
    let info: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(info["Os"], "linux");
    // skopeo copy reads and digest-checks every blob of the layout.
    let copy = format!("dir:{}", dir.path().join("copy").display());
    assert_ok("skopeo copy", &skopeo(&["copy", &layout, &copy]));
}

#[test]
#[ignore = "needs network and skopeo; run with `just it-registry`"]
fn second_pull_reuses_stored_layers() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let out = dir.path().join("out");
    let out_arg = out.to_str().unwrap();
    assert_ok("first pull", &sandcastle(&store, &["pull", IMAGE, "-o", out_arg]));
    let second = sandcastle(&store, &["pull", IMAGE, "-o", out_arg]);
    assert_ok("second pull", &second);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already present") && !stderr.contains("downloaded"), "{stderr}");
    assert_ok("skopeo inspect", &skopeo(&["inspect", &format!("oci:{out_arg}:1.36")]));
}

#[test]
#[ignore = "needs network; run with `just it-registry`"]
fn missing_tag_fails_naming_the_reference() {
    let dir = tempfile::tempdir().unwrap();
    let reference = "mirror.gcr.io/library/busybox:sandcastle-no-such-tag";
    let output = sandcastle(&dir.path().join("store"), &["pull", reference, "-o", dir.path().join("out").to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(reference));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p sandcastle --test registry -- --ignored pulled_layout`
Expected: FAIL, because the CLI has no `pull` subcommand (clap error in the stderr assertion message).

- [ ] **Step 3: Add the `pull` command**

In `src/main.rs`, add to `enum Command` (before `Vm`):

```rust
    /// Pull an image and write it as an OCI image layout directory.
    Pull {
        /// Image reference, e.g. `mirror.gcr.io/library/busybox:1.36`.
        reference: String,
        /// Output OCI layout directory (created or updated in place).
        #[arg(short, long)]
        output: PathBuf,
        /// Name recorded in the layout index; defaults to the reference's tag.
        #[arg(long)]
        tag: Option<String>,
    },
```

and the arm `Command::Pull { reference, output, tag } => pull(&reference, &output, tag),` in `main`, plus:

```rust
fn pull(reference: &str, output: &Path, tag: Option<String>) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let install = Install::locate(&exe)?;
    let store = Store::open(&install::store_root()?, &install)?;
    let blobs = store.blobs();
    let image = registry::pull(&blobs, reference)?;
    let state = ConfigState::from_base(&image.config, image.layers)?;
    let tag = match tag {
        Some(tag) => tag,
        None => registry::default_tag(reference)?,
    };
    let manifest = image::write_layout(&blobs, &state, output, &tag)?;
    println!("{} {}:{tag}", manifest.digest(), output.display());
    Ok(())
}
```

Update the imports at the top of `src/main.rs` to:

```rust
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::image::{self, ConfigState};
use sandcastle::install::{self, Install};
use sandcastle::store::Store;
use sandcastle::{registry, vm};
```

- [ ] **Step 4: Add `just it-registry` and limit `just it` to VM tests**

In `justfile`, change the last line of the `it` recipe to run only the VM test targets:

```just
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_BIN={{it_bin}} cargo test --release -p sandcastle --test vm --test cli -- --ignored --test-threads=1
```

and add after `it`:

```just
# Registry tests: need network, lib/ (store template) and skopeo.
it-registry:
    cargo build --release -p sandcastle-guest --target {{guest_target}}
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_GUEST={{justfile_directory()}}/target/{{guest_target}}/release/sandcastle-guest cargo test --release -p sandcastle --test registry -- --ignored --test-threads=1
```

- [ ] **Step 5: Run the integration tests**

Run: `command -v skopeo || echo "skopeo missing"`. If it is missing, report it to the controller and do not install it. The user decides whether to `brew install skopeo`. In that case run only `cargo test --release -p sandcastle --test registry -- --ignored missing_tag` locally and rely on CI for the skopeo tests.

Run (with skopeo): `just it-registry`
Expected: PASS (3 tests). Stderr of the first pull shows `downloaded` lines.

Run: `just it`
Expected: still exactly the 3 VM/CLI tests pass (macOS).

- [ ] **Step 6: Add the CI job**

Append to `.github/workflows/ci.yml` `jobs:`:

```yaml
  registry:
    needs: libs
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          name: sandcastle-libs-linux-x86_64
          path: lib
      - run: sudo apt-get update && sudo apt-get install -y skopeo
      - uses: taiki-e/install-action@just
      - run: rustup target add x86_64-unknown-linux-musl
      - run: just it-registry
```

Validate: `ruby -ryaml -e 'YAML.load_file(".github/workflows/ci.yml")'`.

- [ ] **Step 7: Full check and commit**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: clean, all fast tests pass.

```bash
git add src tests justfile .github
git commit -m "Add sandcastle pull exporting OCI layouts; registry integration tests" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Out of scope for this plan

- Dockerfile parsing (plan 1).
- Guest `unpack`/`run`/`copy` and `sandcastle build` (plan 4). `ConfigState::add_layer`/`add_empty` and `LayerWriter` are the hooks plan 4 consumes.
- Registry push and authentication.
- zstd layer decompression, which plan 4's unpack must handle (the media type is preserved here).
- Docker `HEALTHCHECK` config, which oci-spec's `Config` does not model. It is dropped on re-export.
