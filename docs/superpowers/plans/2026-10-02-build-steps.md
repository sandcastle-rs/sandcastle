# Build Steps (RUN/COPY in the guest + `sandcastle build`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `sandcastle build -t demo -o ./out ./ctx` builds a single-stage Dockerfile using every v1 instruction into an OCI layout that podman runs.

**Architecture:** The guest helper gains `run` and `copy` jobs. Each job unpacks any missing lower layer from the read-only `/blobs` share into the ext4 store, mounts an overlay with a fresh upper dir, does the step, and tars the upper dir (overlay whiteouts → OCI `.wh.`) to `/out/layer.tar`. The host adapts the parsed Dockerfile, applies metadata instructions to `ConfigState` with Docker's expansion rules, runs one VM per `RUN`/`COPY`, gzips and verifies each layer, and writes the OCI layout.

**Tech Stack:** Rust 2024 (MSRV 1.89), libkrun 1.19.6 (`krun_add_virtiofs3` for read-only shares), rustix 1.1 (mount API, namespaces, credentials), `tar` 0.4, `flate2` (zlib-rs), `sha2`, `sandcastle-dockerfile`, `oci-spec`.

**Spec:** `docs/superpowers/specs/2026-10-02-sandcastle-v1-design.md`

## Rulings (where this plan departs from the spec's wording)

Each ruling below is binding for implementers. Format: decision — why — cost if wrong.

1. **No separate `unpack` job.** `run` and `copy` jobs carry their whole lower stack, with blob digest and media type. The guest unpacks any layer missing from `/store/layers/` before it mounts. — The host cannot see the ext4 store, so it cannot know when an unpack is needed. A Dockerfile with only metadata instructions also never boots a VM. — Cost if wrong: a separate unpack job could be added later; the guest code stays the same.
2. **RUN gets Docker's container view instead of binds of the guest's `/dev`, `/proc`, `/sys`.** The step runs in its own PID namespace (the command is init, so the kernel kills leftover daemons and the overlay can unmount) and its own mount namespace. It gets a fresh `proc`, a read-only `sysfs`, and a tmpfs `/dev` with `null zero full random urandom tty`, `devpts`, `shm` and the standard symlinks. — The guest's `/dev` contains the store disk `/dev/vda`. — Cost if wrong: a few lines in `run.rs`.
3. **Mount points come from a bottom "stub" lower layer**, not from stub files created in the upper dir. The stub layer holds `etc/{resolv.conf,hosts,hostname}` and `dev/ proc/ sys/`. — Stubs in the upper dir would copy up `/etc` into every RUN layer, and a no-op RUN would no longer give an empty diff. — Cost if wrong: none to the image format.
4. **Empty layers are dropped.** When a step changes nothing, the guest reports `layer: None`, and the host records history with `empty_layer: true` and no layer, as BuildKit's exporter does. — Cost if wrong: one branch.
5. **zstd base layers are rejected** with a clear error before any VM boots. — The guest must stay pure Rust, and ruzstd's streaming decoder handles a single frame only. — Cost if wrong: a deferred feature.
6. **Guest DNS.** The host's `/etc/resolv.conf` is passed with loopback and non-IPv4 nameservers removed. A systemd-resolved stub (only `127.0.0.53`) is replaced by `/run/systemd/resolve/resolv.conf`. If no nameserver remains, `8.8.8.8`/`8.8.4.4` are used. This is Docker's behaviour, keeping IPv4 only because macOS lists link-local IPv6 servers. — Cost if wrong: one function.
7. **Helper failures after `/out` is mounted are reported in `status.json` (`error`)**, and the host shows them instead of "setup failed before the step ran". — Cost if wrong: none.
8. **The store lock fd is inherited by the `__vm` child** (`FD_CLOEXEC` cleared), and on Linux the child gets `PR_SET_PDEATHSIG=SIGKILL`. An orphaned VM therefore keeps the store locked instead of sharing the ext4 disk with the next build. This fixes a deferred plan-2 item. — Cost if wrong: none.
9. **Guest file reads on the host refuse links and special files** (`O_NOFOLLOW | O_NONBLOCK`, regular file only) for `status.json` and `layer.tar`. This fixes a deferred plan-2 item.
10. **COPY metadata:** files, dirs and symlinks are owned `0:0`, mode and mtime come from the source, and directories the copy creates get `0755`. Existing destination directories keep their metadata. Special files in the context are skipped with a warning.

## Global Constraints

- Edition 2024, `rust-version = "1.89"` (workspace).
- Host `unsafe` stays in `src/vm/krun.rs` only. Guest `unsafe` is limited to the two `rustix::thread::unshare_unsafe` calls in `crates/sandcastle-guest/src/run.rs`, each with a `// SAFETY:` comment.
- The guest crate must cross-build with `cargo build -p sandcastle-guest --target <arch>-unknown-linux-musl` (pure-Rust deps only, `rust-lld`). No new C dependency in the guest.
- Guest pure logic (`layer.rs`, `user.rs`, `copy.rs`) uses no Linux-only APIs and is unit-tested on macOS.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass on macOS and Linux. Also run `cargo clippy -p sandcastle-guest --target aarch64-unknown-linux-musl -- -D warnings` (or the x86_64 musl target on Linux) for the guest's Linux code.
- Integration tests are `#[ignore]` and run only via `just it` (needs `lib/`, a hypervisor and network). Never run the README conformance suite.
- `anyhow` in binaries. Error context names the step: `step 4/9 RUN apt-get …: exited with 100`.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- No meta-design docs in user-facing docs, no tautological tests, one implementation path.

## Review Focus

1. **The image's `/etc/resolv.conf` is a dangling symlink or missing.** RUN still works: the bind is skipped when the target is not a regular file. Test: Task 7, `run_survives_resolv_conf_symlink`.
2. **A RUN step leaves a background process running** (`sleep 1000 &`). The step finishes, and the overlay unmounts. Test: Task 7, `run_kills_leftover_processes`.
3. **COPY destination goes through an image symlink** (e.g. debian's `/bin -> usr/bin`). The file lands inside the image root. Test: Task 4, `dest_through_image_symlink_stays_in_root`.
4. **A directory deleted and recreated in a RUN** gives an opaque dir. Later steps see only the new contents. Test: Task 7, `opaque_dir_hides_lower_contents`.
5. **`USER` names a user that is not in `/etc/passwd`.** The build fails with `unable to find user …`, not a guest crash. Test: Task 3 unit, plus Task 11 `unknown_user_fails_step`.

---

## File Structure

```
crates/sandcastle-proto/src/lib.rs     Job::{Run,Copy}, LowerLayer, Status{layer,error}, share tags, consts
crates/sandcastle-guest/
  Cargo.toml                           + tar, flate2, sha2, serde; rustix features
  src/main.rs                          module wiring, `--exec` dispatch
  src/layer.rs        (pure)           OCI tar paths, whiteouts, tar entry writer, hashing, overlay stack
  src/user.rs         (pure)           USER → uid/gid/groups/home from passwd/group text
  src/copy.rs         (std only)       resolve_in_root, glob, Docker COPY
  src/linux.rs        (Linux)          job dispatch, status writing
  src/store.rs        (Linux)          /store mount, layer dirs, ensure_layers
  src/unpack.rs       (Linux)          layer blob → overlay-format dir
  src/overlay.rs      (Linux)          overlay mount/unmount
  src/commit.rs       (Linux)          upper dir → OCI layer tar
  src/run.rs          (Linux)          RUN job and the `--exec` child
src/
  vm/mod.rs                            Vm runner, Share, Resources, Finished, open_guest_file
  vm/krun.rs                           krun_add_virtiofs3
  vm/child.rs                          shares, PDEATHSIG
  vm/landlock.rs                       rules from shares
  store.rs                             lock fd inheritable, blobs_dir, guest dirs
  image.rs                             ConfigState::lower_layers
  dockerfile.rs                        parse + v1 scope check → Recipe
  build/mod.rs                         orchestration, layer ingest
  build/config.rs     (pure)           instruction → config, expansion, EXPOSE, workdir
  build/dns.rs        (pure + read)    guest resolv.conf
  doctor.rs, main.rs, lib.rs
tests/vm.rs, tests/build.rs, tests/podman.rs, tests/cli.rs
tests/fixtures/build/{alpine,debian}/  Dockerfiles and contexts
justfile, .github/workflows/ci.yml
```

---

### Task 1: Wire types for RUN and COPY jobs

**Files:**
- Modify: `crates/sandcastle-proto/src/lib.rs`
- Modify: `src/store.rs` (use `GUEST_HELPER_PATH` from proto)
- Modify: `src/vm/child.rs` (import path)
- Modify: `crates/sandcastle-guest/src/linux.rs`, `src/vm/mod.rs` (Status construction)

**Interfaces:**
- Produces:
  - `sandcastle_proto::{Job, RunJob, CopyJob, LowerLayer, Status, ProbeReport}`
  - consts `JOB_FILE, STATUS_FILE, LAYER_FILE, GUEST_HELPER_PATH, SHARE_OUT, SHARE_BLOBS, SHARE_CTX, LAYER_TAR, LAYER_TAR_GZIP`
  - `Status: Default`

- [ ] **Step 1: Write the failing wire-format tests**

Append to the `tests` module in `crates/sandcastle-proto/src/lib.rs`:

```rust
    #[test]
    fn run_job_wire_format() {
        let job: Job = serde_json::from_str(
            r#"{"mode":"run","lower":[{"diff_id":"sha256:aa","blob":"sha256:bb","media_type":"application/vnd.oci.image.layer.v1.tar+gzip"}],
                "argv":["/bin/sh","-c","true"],"env":["PATH=/bin"],"user":"","workdir":"/","resolv_conf":"nameserver 8.8.8.8\n"}"#,
        )
        .unwrap();
        let Job::Run(run) = job else { panic!("not a run job") };
        assert_eq!(run.lower[0].blob, "sha256:bb");
        assert_eq!(run.argv, ["/bin/sh", "-c", "true"]);
    }

    #[test]
    fn copy_job_wire_format() {
        let job: Job = serde_json::from_str(
            r#"{"mode":"copy","lower":[],"sources":["a","b/"],"dest":"/x/","workdir":"/app"}"#,
        )
        .unwrap();
        assert_eq!(
            job,
            Job::Copy(CopyJob {
                lower: vec![],
                sources: vec!["a".into(), "b/".into()],
                dest: "/x/".into(),
                workdir: "/app".into(),
            })
        );
    }

    #[test]
    fn status_with_layer_and_error() {
        let status: Status =
            serde_json::from_str(r#"{"exit_code":0,"layer":"sha256:cc"}"#).unwrap();
        assert_eq!(status.layer.as_deref(), Some("sha256:cc"));
        let failed: Status =
            serde_json::from_str(r#"{"exit_code":1,"error":"mounting the overlay"}"#).unwrap();
        assert_eq!(failed.error.as_deref(), Some("mounting the overlay"));
        assert_eq!(
            serde_json::to_string(&Status { exit_code: 3, ..Default::default() }).unwrap(),
            r#"{"exit_code":3}"#
        );
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle-proto`
Expected: compile errors (`RunJob`, `CopyJob`, `layer`, `error` don't exist yet).

- [ ] **Step 3: Implement the types**

Replace everything above `#[cfg(test)]` in `crates/sandcastle-proto/src/lib.rs` with:

```rust
//! Wire types shared by the host and the in-VM guest helper.
//!
//! The host writes a [`Job`] to `/out/job.json` on the per-job virtio-fs
//! share; the guest writes a [`Status`] to `/out/status.json` before exiting.

use serde::{Deserialize, Serialize};

/// Job file name inside the guest's `/out` share.
pub const JOB_FILE: &str = "job.json";
/// Status file name inside the guest's `/out` share.
pub const STATUS_FILE: &str = "status.json";
/// Uncompressed layer tar a `run` or `copy` job leaves in `/out`.
pub const LAYER_FILE: &str = "layer.tar";
/// Path of the guest helper inside the VM.
pub const GUEST_HELPER_PATH: &str = "/sandcastle-guest";

/// virtio-fs tags; the guest mounts each at `/<tag>`.
pub const SHARE_OUT: &str = "out";
pub const SHARE_BLOBS: &str = "blobs";
pub const SHARE_CTX: &str = "ctx";

/// Layer media types the guest can unpack.
pub const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const LAYER_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Work the guest helper performs in one VM boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Job {
    /// Self-test used by `sandcastle doctor`. The helper exits with `exit_code`.
    Probe { exit_code: i32 },
    Run(RunJob),
    Copy(CopyJob),
}

/// One image layer a step builds on. The guest unpacks it from
/// `/blobs/sha256/<blob hex>` if `/store/layers/<diff_id hex>` is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowerLayer {
    pub diff_id: String,
    pub blob: String,
    pub media_type: String,
}

/// A `RUN` step. `lower` is bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunJob {
    pub lower: Vec<LowerLayer>,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    /// Dockerfile `USER` value; empty means root.
    pub user: String,
    pub workdir: String,
    /// Contents of the step's `/etc/resolv.conf`.
    pub resolv_conf: String,
}

/// A `COPY` step from the build context (shared at `/ctx`). `lower` is bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyJob {
    pub lower: Vec<LowerLayer>,
    pub sources: Vec<String>,
    pub dest: String,
    pub workdir: String,
}

/// Result written by the guest helper. Authoritative over the VM process
/// exit code, because libkrun's init reserves 125, 126 and 127.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
    /// diff_id of `/out/layer.tar`; `None` when the step changed nothing
    /// or the command failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// The helper itself failed; `exit_code` is meaningless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Guest capabilities found by [`Job::Probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub kernel_release: String,
    /// Overlayfs accepts `lowerdir+` (kernel ≥ 6.8), needed for long layer stacks.
    pub overlay_lowerdir_plus: bool,
}
```

In the existing `status_wire_format` test, replace the struct literal `Status { exit_code: 3, probe: None }` with `Status { exit_code: 3, ..Default::default() }`.

- [ ] **Step 4: Update users of the old shapes**

- `src/store.rs`: delete `pub const GUEST_HELPER_PATH …` and add `use sandcastle_proto::GUEST_HELPER_PATH;`. Re-export it so existing imports keep working: `pub use sandcastle_proto::GUEST_HELPER_PATH;`. Use the `pub use` form only, not both.
- `crates/sandcastle-guest/src/linux.rs`: the probe arm becomes
  `Job::Probe { exit_code } => Status { exit_code, probe: Some(crate::probe::run()?), ..Default::default() },`
  and add a temporary arm `Job::Run(_) | Job::Copy(_) => anyhow::bail!("job not supported by this helper"),`. Task 6 replaces it.
- `src/vm/mod.rs` tests: replace `Status { exit_code: 127, probe: None }` with `Status { exit_code: 127, ..Default::default() }`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sandcastle-proto src/store.rs src/vm crates/sandcastle-guest/src/linux.rs
git commit -m "Add run and copy job wire types

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Guest layer tar format (pure)

**Files:**
- Create: `crates/sandcastle-guest/src/layer.rs`
- Modify: `crates/sandcastle-guest/src/main.rs`, `crates/sandcastle-guest/Cargo.toml`

**Interfaces:**
- Produces (used by Tasks 6–7):
  - `layer::TarPath { Root, Plain(PathBuf), Whiteout(PathBuf), Opaque(PathBuf) }`, `layer::classify(&Path) -> io::Result<TarPath>`
  - `layer::Kind`, `layer::Meta`, `layer::append<W: Write>(&mut tar::Builder<W>, &Path, &Kind, &Meta, impl Read) -> io::Result<()>`
  - `layer::HashWriter<W>::{new, finish(self) -> (W, String)}`, `layer::HashReader<R>::{new, finish(self) -> (R, String)}` (digest strings are `sha256:<hex>`)
  - `layer::digest_hex(&str) -> io::Result<&str>` (validates `sha256:` + 64 lowercase hex)
  - `layer::overlay_stack(&[String]) -> Vec<&str>` (bottom-first in, top-first out, duplicates dropped keeping the top-most)
  - consts `WHITEOUT_PREFIX`, `OPAQUE_MARKER`, `OPAQUE_XATTR = "trusted.overlay.opaque"`, `OVERLAY_XATTR_PREFIX = "trusted.overlay."`, `XATTR_PAX_PREFIX = "SCHILY.xattr."`

- [ ] **Step 1: Add dependencies and the module**

`crates/sandcastle-guest/Cargo.toml` `[dependencies]` becomes:

```toml
[dependencies]
anyhow = "1.0.104"
flate2 = { version = "1.1.10", default-features = false, features = ["zlib-rs"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
sandcastle-proto = { path = "../sandcastle-proto" }
sha2 = "0.11.0"
tar = { version = "0.4.46", default-features = false }

[target.'cfg(target_os = "linux")'.dependencies]
rustix = { version = "1.1.5", features = ["fs", "mount", "process", "system", "thread"] }

[dev-dependencies]
tempfile = "3.27.0"
```

In `crates/sandcastle-guest/src/main.rs`, add the module above the Linux modules:

```rust
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod layer;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/sandcastle-guest/src/layer.rs` with only the test module below (Step 4 adds the code above it):

```rust
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
        Meta { mode, uid: 0, gid: 0, mtime: 1_700_000_000, size: 0, xattrs: vec![] }
    }

    #[test]
    fn append_writes_oci_entries_that_classify_back() {
        let mut tar = tar::Builder::new(Vec::new());
        let file = b"hello";
        append(&mut tar, Path::new("etc"), &Kind::Dir { opaque: true }, &meta(0o755), io::empty()).unwrap();
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
        append(&mut tar, Path::new("etc/gone"), &Kind::Whiteout, &meta(0), io::empty()).unwrap();
        append(&mut tar, Path::new("etc/link"), &Kind::Symlink("app.conf".into()), &meta(0o777), io::empty()).unwrap();
        append(&mut tar, Path::new("etc/hard"), &Kind::Hardlink("etc/app.conf".into()), &meta(0o644), io::empty()).unwrap();
        append(&mut tar, Path::new("dev/null"), &Kind::Char(1, 3), &meta(0o666), io::empty()).unwrap();
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
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test -p sandcastle-guest layer`
Expected: compile errors (items undefined).

- [ ] **Step 4: Implement**

Put this above the tests in `layer.rs`:

```rust
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
    Dir { opaque: bool },
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
            let name = path.file_name().ok_or_else(|| invalid(path, "has no name"))?;
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
        Self { inner, hasher: Sha256::new() }
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
        Self { inner, hasher: Sha256::new() }
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
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid digest {digest:?}"))
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
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p sandcastle-guest layer && cargo build -p sandcastle-guest --target aarch64-unknown-linux-musl`
(On an x86_64 Linux machine use `x86_64-unknown-linux-musl`.)
Expected: PASS, and the musl build succeeds. That confirms `tar` and `flate2` stay pure Rust.

- [ ] **Step 6: Commit**

```bash
git add crates/sandcastle-guest Cargo.lock
git commit -m "Add guest layer tar format: whiteouts, entries, hashing

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Guest USER resolution (pure)

**Files:**
- Create: `crates/sandcastle-guest/src/user.rs`
- Modify: `crates/sandcastle-guest/src/main.rs`

**Interfaces:**
- Produces: `user::Ids { uid: u32, gid: u32, groups: Vec<u32>, home: String }` and `user::resolve(spec: &str, passwd: &str, group: &str) -> anyhow::Result<Ids>`

These rules follow runc's `user.GetExecUser`. An empty spec means root. `name` or `uid`, optionally followed by `:group` or `:gid`. A numeric uid not in passwd keeps gid 0 and home `/`. Supplementary groups are the groups that list the user as a member, but only when no group was given.

- [ ] **Step 1: Write the failing tests**

Create `crates/sandcastle-guest/src/user.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
# comment
broken line
app:x:1500:1500::/home/app:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
";
    const GROUP: &str = "\
root:x:0:
app:x:1500:
grp:x:2000:app,other
wheel:x:10:root
";

    #[test]
    fn empty_spec_is_root_with_its_home() {
        let ids = resolve("", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.home.as_str()), (0, 0, "/root"));
        assert_eq!(ids.groups, [10]);
    }

    #[test]
    fn name_gets_primary_and_member_groups() {
        let ids = resolve("app", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid), (1500, 1500));
        assert_eq!(ids.groups, [2000]);
        assert_eq!(ids.home, "/home/app");
    }

    #[test]
    fn numeric_uid_in_passwd_uses_its_entry() {
        let ids = resolve("65534", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.home.as_str()), (65534, 65534, "/nonexistent"));
    }

    #[test]
    fn unknown_numeric_ids_are_used_as_is() {
        let ids = resolve("4321:4322", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.home.as_str()), (4321, 4322, "/"));
        assert!(ids.groups.is_empty());
        let ids = resolve("4321", "", "").unwrap();
        assert_eq!((ids.uid, ids.gid), (4321, 0));
    }

    #[test]
    fn explicit_group_by_name_replaces_supplementary_groups() {
        let ids = resolve("app:grp", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid), (1500, 2000));
        assert!(ids.groups.is_empty());
    }

    #[test]
    fn unknown_names_are_errors() {
        let err = resolve("ghost", PASSWD, GROUP).unwrap_err().to_string();
        assert_eq!(err, "unable to find user ghost: no matching entries in passwd file");
        let err = resolve("app:ghosts", PASSWD, GROUP).unwrap_err().to_string();
        assert_eq!(err, "unable to find group ghosts: no matching entries in group file");
    }
}
```

Add to `main.rs`:

```rust
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod user;
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle-guest user`
Expected: compile errors.

- [ ] **Step 3: Implement**

Put above the tests:

```rust
//! Resolves a Dockerfile `USER` value against the image's `/etc/passwd`
//! and `/etc/group`, following runc's `user.GetExecUser`.

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups for `setgroups`.
    pub groups: Vec<u32>,
    /// Used for `HOME` when the image env does not set it.
    pub home: String,
}

struct PasswdEntry<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

struct GroupEntry<'a> {
    name: &'a str,
    gid: u32,
    members: Vec<&'a str>,
}

pub fn resolve(spec: &str, passwd: &str, group: &str) -> Result<Ids> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let users: Vec<PasswdEntry> = passwd.lines().filter_map(parse_passwd).collect();
    let groups: Vec<GroupEntry> = group.lines().filter_map(parse_group).collect();

    let numeric_user = user_part.parse::<u32>().ok();
    let found = if user_part.is_empty() {
        users.iter().find(|u| u.uid == 0)
    } else {
        users
            .iter()
            .find(|u| u.name == user_part || Some(u.uid) == numeric_user)
    };
    let (uid, mut gid, home, name) = match (found, numeric_user) {
        (Some(u), _) => (u.uid, u.gid, u.home.to_string(), Some(u.name)),
        (None, Some(uid)) => (uid, 0, "/".to_string(), None),
        (None, None) if user_part.is_empty() => (0, 0, "/".to_string(), None),
        (None, None) => {
            bail!("unable to find user {user_part}: no matching entries in passwd file")
        }
    };

    let supplementary = match group_part {
        Some(g) => {
            let numeric = g.parse::<u32>().ok();
            gid = match groups.iter().find(|e| e.name == g || Some(e.gid) == numeric) {
                Some(e) => e.gid,
                None => match numeric {
                    Some(n) => n,
                    None => bail!("unable to find group {g}: no matching entries in group file"),
                },
            };
            Vec::new()
        }
        None => {
            let mut ids: Vec<u32> = match name {
                Some(name) => groups
                    .iter()
                    .filter(|e| e.members.contains(&name))
                    .map(|e| e.gid)
                    .collect(),
                None => Vec::new(),
            };
            ids.dedup();
            ids
        }
    };
    Ok(Ids { uid, gid, groups: supplementary, home })
}

fn parse_passwd(line: &str) -> Option<PasswdEntry<'_>> {
    if line.starts_with('#') {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 7 {
        return None;
    }
    Some(PasswdEntry {
        name: f[0],
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        home: f[5],
    })
}

fn parse_group(line: &str) -> Option<GroupEntry<'_>> {
    if line.starts_with('#') {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 4 {
        return None;
    }
    Some(GroupEntry {
        name: f[0],
        gid: f[2].parse().ok()?,
        members: f[3].split(',').filter(|m| !m.is_empty()).collect(),
    })
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sandcastle-guest user`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sandcastle-guest/src/user.rs crates/sandcastle-guest/src/main.rs
git commit -m "Resolve USER against passwd and group like runc

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Guest COPY semantics (std only)

**Files:**
- Create: `crates/sandcastle-guest/src/copy.rs`
- Modify: `crates/sandcastle-guest/src/main.rs`

**Interfaces:**
- Produces:
  - `copy::resolve_in_root(root: &Path, path: &Path) -> anyhow::Result<PathBuf>` (Task 7 reads `/etc/passwd` through it)
  - `copy::matches(pattern: &str, name: &str) -> anyhow::Result<bool>`
  - `copy::Copy<'a> { ctx: &'a Path, root: &'a Path, workdir: &'a str, owner: Option<(u32, u32)> }` with:
    - `run(&self, sources: &[String], dest: &str) -> anyhow::Result<()>`
    - `ensure_workdir(&self) -> anyhow::Result<()>`

Rules, following Docker/BuildKit `COPY`:
- **Source paths:**
  - A source resolves inside the context. `..` and absolute symlinks never leave it.
  - A top-level source symlink is followed. Symlinks nested inside a copied directory are copied as symlinks.
  - Wildcards use Go's `filepath.Match` per path component, so `*` matches dotfiles.
  - A source that matches nothing is an error.
- **Directory sources:** a directory source copies its contents, not the directory itself.
- **Destination:**
  - A relative destination is joined to the workdir.
  - The destination is a directory if it ends with `/` or `/.`, if it is `.`, if more than one source path matched, or if it already exists as a directory. Otherwise it is the file name.
  - The destination resolves inside the image root, so image symlinks like `/bin -> usr/bin` are followed but cannot escape.
  - An existing non-directory destination is replaced. Replacing a directory with a non-directory is an error.

- [ ] **Step 1: Write the failing tests**

Create `crates/sandcastle-guest/src/copy.rs` with:

```rust
#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: PathBuf,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        let root = dir.path().join("root");
        fs::create_dir_all(ctx.join("tree/sub")).unwrap();
        fs::write(ctx.join("hello.txt"), "hi").unwrap();
        fs::write(ctx.join("one.conf"), "1").unwrap();
        fs::write(ctx.join("two.conf"), "2").unwrap();
        fs::write(ctx.join("tree/a.txt"), "a").unwrap();
        fs::write(ctx.join("tree/sub/b.txt"), "b").unwrap();
        symlink("a.txt", ctx.join("tree/link")).unwrap();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        symlink("usr/bin", root.join("bin")).unwrap();
        Fixture { _dir: dir, ctx, root }
    }

    fn copy<'a>(f: &'a Fixture, workdir: &'a str) -> Copy<'a> {
        Copy { ctx: &f.ctx, root: &f.root, workdir, owner: None }
    }

    #[test]
    fn resolve_in_root_keeps_links_inside_root() {
        let f = fixture();
        symlink("/usr", f.root.join("abs")).unwrap();
        assert_eq!(resolve_in_root(&f.root, Path::new("/abs/bin/x")).unwrap(), f.root.join("usr/bin/x"));
        assert_eq!(resolve_in_root(&f.root, Path::new("../../bin")).unwrap(), f.root.join("usr/bin"));
        symlink("loop", f.root.join("loop")).unwrap();
        assert!(resolve_in_root(&f.root, Path::new("loop")).is_err());
    }

    #[test]
    fn glob_matches_like_go() {
        for (pattern, name, want) in [
            ("*.txt", "a.txt", true),
            ("*.txt", "a.md", false),
            ("*", ".hidden", true),
            ("a?c", "abc", true),
            ("[a-c]x", "bx", true),
            ("[^a-c]x", "bx", false),
            ("[^a-c]x", "dx", true),
            ("\\*", "*", true),
            ("\\*", "a", false),
        ] {
            assert_eq!(matches(pattern, name).unwrap(), want, "{pattern} vs {name}");
        }
        assert!(matches("[a-", "a").is_err());
    }

    #[test]
    fn file_into_dir_and_file_rename() {
        let f = fixture();
        copy(&f, "/").run(&["hello.txt".into()], "/app/").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("app/hello.txt")).unwrap(), "hi");
        copy(&f, "/").run(&["hello.txt".into()], "/renamed.txt").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("renamed.txt")).unwrap(), "hi");
    }

    #[test]
    fn dir_source_copies_contents_and_keeps_nested_links() {
        let f = fixture();
        copy(&f, "/").run(&["tree".into()], "/data").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("data/a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(f.root.join("data/sub/b.txt")).unwrap(), "b");
        assert_eq!(fs::read_link(f.root.join("data/link")).unwrap(), Path::new("a.txt"));
        assert!(!f.root.join("data/tree").exists());
    }

    #[test]
    fn wildcard_multiple_matches_go_into_dir() {
        let f = fixture();
        copy(&f, "/").run(&["*.conf".into()], "/etc/demo").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("etc/demo/one.conf")).unwrap(), "1");
        assert_eq!(fs::read_to_string(f.root.join("etc/demo/two.conf")).unwrap(), "2");
    }

    #[test]
    fn relative_dest_uses_workdir() {
        let f = fixture();
        copy(&f, "/srv/app").run(&["hello.txt".into()], "./").unwrap();
        assert!(f.root.join("srv/app/hello.txt").is_file());
    }

    #[test]
    fn dest_through_image_symlink_stays_in_root() {
        let f = fixture();
        copy(&f, "/").run(&["hello.txt".into()], "/bin/").unwrap();
        assert!(f.root.join("usr/bin/hello.txt").is_file());
    }

    #[test]
    fn source_symlink_outside_context_resolves_inside_it() {
        let f = fixture();
        symlink("/hello.txt", f.ctx.join("abs-link")).unwrap();
        copy(&f, "/").run(&["abs-link".into()], "/got").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("got")).unwrap(), "hi");
        let err = copy(&f, "/").run(&["../../etc/passwd".into()], "/x").unwrap_err();
        assert!(format!("{err:#}").contains("not found in the build context"), "{err:#}");
    }

    #[test]
    fn missing_sources_are_errors() {
        let f = fixture();
        let err = copy(&f, "/").run(&["nope.txt".into()], "/x").unwrap_err();
        assert!(format!("{err:#}").contains("nope.txt: not found in the build context"), "{err:#}");
        let err = copy(&f, "/").run(&["*.rs".into()], "/x/").unwrap_err();
        assert!(format!("{err:#}").contains("*.rs: no files in the build context match"), "{err:#}");
    }

    #[test]
    fn mode_and_mtime_are_preserved() {
        let f = fixture();
        let src = f.ctx.join("run.sh");
        fs::write(&src, "#!/bin/sh").unwrap();
        fs::set_permissions(&src, Permissions::from_mode(0o750)).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options().write(true).open(&src).unwrap().set_modified(mtime).unwrap();
        copy(&f, "/").run(&["run.sh".into()], "/run.sh").unwrap();
        let meta = fs::metadata(f.root.join("run.sh")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o750);
        assert_eq!(meta.modified().unwrap(), mtime);
    }

    #[test]
    fn overwrites_files_but_not_dirs_and_keeps_existing_dir_metadata() {
        let f = fixture();
        fs::write(f.root.join("renamed.txt"), "old").unwrap();
        copy(&f, "/").run(&["hello.txt".into()], "/renamed.txt").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("renamed.txt")).unwrap(), "hi");

        fs::create_dir_all(f.root.join("data/a.txt")).unwrap();
        let err = copy(&f, "/").run(&["tree".into()], "/data").unwrap_err();
        assert!(format!("{err:#}").contains("cannot replace directory"), "{err:#}");

        fs::create_dir(f.root.join("keep")).unwrap();
        fs::set_permissions(f.root.join("keep"), Permissions::from_mode(0o700)).unwrap();
        copy(&f, "/").run(&["hello.txt".into()], "/keep/").unwrap();
        assert_eq!(fs::metadata(f.root.join("keep")).unwrap().mode() & 0o7777, 0o700);
    }
}
```

Add to `main.rs`:

```rust
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod copy;
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle-guest copy`
Expected: compile errors.

- [ ] **Step 3: Implement**

Put above the tests:

```rust
//! Docker `COPY` from the build context into an image root. Paths resolve
//! as if each root were `/`. Uses only std, so the rules test on any host.

use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Metadata, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// Most symlinks followed while resolving one path (Linux's MAXSYMLINKS).
const MAX_LINKS: usize = 40;

/// Resolves `path` inside `root` as if `root` were `/`: `..` stops at the
/// root and absolute link targets restart from it. Components that do not
/// exist are kept as written.
pub fn resolve_in_root(root: &Path, path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut pending: Vec<OsString> = parts(path);
    pending.reverse();
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        match fs::symlink_metadata(root.join(&candidate)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                ensure!(
                    links <= MAX_LINKS,
                    "too many levels of symbolic links in {}",
                    path.display()
                );
                let target = fs::read_link(root.join(&candidate))?;
                if target.is_absolute() {
                    resolved = PathBuf::new();
                }
                let mut target_parts = parts(&target);
                target_parts.reverse();
                pending.extend(target_parts);
            }
            Ok(_) => resolved = candidate,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                resolved = candidate;
            }
            Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
        }
    }
    Ok(root.join(resolved))
}

/// Normal components and `..`, in order; `.` and the root are dropped.
fn parts(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

fn has_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '\\'])
}

/// Go's `filepath.Match` for one path component: `*`, `?`, `[...]` with
/// ranges and `^` negation, and `\` escapes.
pub fn matches(pattern: &str, name: &str) -> Result<bool> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_at(&p, &n).with_context(|| format!("bad pattern {pattern:?}"))
}

fn match_at(p: &[char], n: &[char]) -> Result<bool> {
    let Some((&first, rest)) = p.split_first() else {
        return Ok(n.is_empty());
    };
    match first {
        '*' => {
            for skip in 0..=n.len() {
                if match_at(rest, &n[skip..])? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        '?' => Ok(!n.is_empty() && match_at(rest, &n[1..])?),
        '[' => {
            let (hit, after) = class(rest, n.first().copied())?;
            Ok(hit && match_at(after, &n[1..])?)
        }
        '\\' => {
            let (&c, rest) = rest.split_first().context("trailing backslash")?;
            Ok(n.first() == Some(&c) && match_at(rest, &n[1..])?)
        }
        c => Ok(n.first() == Some(&c) && match_at(rest, &n[1..])?),
    }
}

/// Parses a character class after `[`; returns whether `c` is in it and the
/// pattern after the closing `]`.
fn class(mut p: &[char], c: Option<char>) -> Result<(bool, &[char])> {
    let negate = p.first() == Some(&'^');
    if negate {
        p = &p[1..];
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let Some((&ch, rest)) = p.split_first() else {
            bail!("missing ]");
        };
        if ch == ']' && !first {
            p = rest;
            break;
        }
        first = false;
        let (lo, rest) = class_char(p)?;
        p = rest;
        let hi = if p.first() == Some(&'-') && p.get(1) != Some(&']') {
            let (hi, rest) = class_char(&p[1..])?;
            p = rest;
            hi
        } else {
            lo
        };
        if c.is_some_and(|c| lo <= c && c <= hi) {
            hit = true;
        }
    }
    Ok((c.is_some() && hit != negate, p))
}

fn class_char(p: &[char]) -> Result<(char, &[char])> {
    match p {
        ['\\', c, rest @ ..] => Ok((*c, rest)),
        [c, rest @ ..] if *c != ']' => Ok((*c, rest)),
        _ => bail!("bad character class"),
    }
}

/// Source paths in the context for one COPY argument, sorted.
fn expand_source(ctx: &Path, source: &str) -> Result<Vec<PathBuf>> {
    if !has_meta(source) {
        let path = resolve_in_root(ctx, Path::new(source))?;
        ensure!(
            fs::symlink_metadata(&path).is_ok(),
            "{source}: not found in the build context"
        );
        return Ok(vec![path]);
    }
    let mut current = vec![PathBuf::new()];
    for part in parts(Path::new(source)) {
        let part_str = part.to_string_lossy();
        let mut next = Vec::new();
        for base in &current {
            if part == ".." {
                let mut up = base.clone();
                up.pop();
                next.push(up);
            } else if has_meta(&part_str) {
                let Ok(entries) = fs::read_dir(resolve_in_root(ctx, base)?) else {
                    continue;
                };
                let mut names: Vec<OsString> =
                    entries.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
                names.sort();
                for name in names {
                    if let Some(s) = name.to_str()
                        && matches(&part_str, s)?
                    {
                        next.push(base.join(&name));
                    }
                }
            } else {
                next.push(base.join(&part));
            }
        }
        current = next;
    }
    let mut out = Vec::new();
    for rel in current {
        let path = resolve_in_root(ctx, &rel)?;
        if fs::symlink_metadata(&path).is_ok() {
            out.push(path);
        }
    }
    ensure!(!out.is_empty(), "{source}: no files in the build context match");
    Ok(out)
}

/// One COPY step: `ctx` is the build context, `root` the image root.
pub struct Copy<'a> {
    pub ctx: &'a Path,
    pub root: &'a Path,
    pub workdir: &'a str,
    /// Owner for everything written; `None` keeps the process's own ids.
    pub owner: Option<(u32, u32)>,
}

impl Copy<'_> {
    pub fn run(&self, sources: &[String], dest: &str) -> Result<()> {
        let mut srcs = Vec::new();
        for source in sources {
            srcs.extend(expand_source(self.ctx, source)?);
        }
        let into_dir = dest.ends_with('/') || dest == "." || dest.ends_with("/.") || srcs.len() > 1;
        let target = resolve_in_root(self.root, &Path::new(self.workdir).join(dest))?;
        for src in &srcs {
            if fs::metadata(src)?.is_dir() {
                self.mkdir_p(&target)?;
                self.copy_children(src, &target)?;
            } else {
                let file_dest = if into_dir || target.is_dir() {
                    self.mkdir_p(&target)?;
                    target.join(src.file_name().context("source has no file name")?)
                } else {
                    self.mkdir_p(target.parent().context("destination has no parent")?)?;
                    target.clone()
                };
                self.copy_entry(src, &file_dest)?;
            }
        }
        Ok(())
    }

    /// Creates the step's working directory like `mkdir -p`.
    pub fn ensure_workdir(&self) -> Result<()> {
        self.mkdir_p(&resolve_in_root(self.root, Path::new(self.workdir))?)
    }

    /// `dir` is already resolved inside the root, so every existing
    /// component is a real directory and missing ones are created `0755`.
    fn mkdir_p(&self, dir: &Path) -> Result<()> {
        let rel = dir
            .strip_prefix(self.root)
            .context("destination leaves the image root")?;
        let mut cur = self.root.to_path_buf();
        for component in rel.components() {
            cur.push(component);
            match fs::symlink_metadata(&cur) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("{} is not a directory", self.shown(&cur)),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&cur)?;
                    self.chown(&cur)?;
                    fs::set_permissions(&cur, Permissions::from_mode(0o755))?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn copy_children(&self, src_dir: &Path, dest_dir: &Path) -> Result<()> {
        let mut names: Vec<OsString> = fs::read_dir(src_dir)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            self.copy_entry(&src_dir.join(&name), &dest_dir.join(&name))?;
        }
        Ok(())
    }

    fn copy_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(src)?;
        let ft = meta.file_type();
        let mut created = true;
        match fs::symlink_metadata(dst) {
            Ok(existing) if existing.is_dir() && ft.is_dir() => created = false,
            Ok(existing) if existing.is_dir() => bail!(
                "cannot replace directory {} with a non-directory",
                self.shown(dst)
            ),
            Ok(_) => fs::remove_file(dst)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if ft.is_dir() {
            if created {
                fs::create_dir(dst)?;
            }
            self.copy_children(src, dst)?;
            if created {
                self.finish(dst, &meta)?;
            }
        } else if ft.is_file() {
            let mut from = File::open(src)?;
            let mut to = File::create_new(dst)?;
            io::copy(&mut from, &mut to)
                .with_context(|| format!("copying {}", src.display()))?;
            drop(to);
            self.finish(dst, &meta)?;
        } else if ft.is_symlink() {
            symlink(fs::read_link(src)?, dst)?;
            self.chown(dst)?;
        } else {
            eprintln!("sandcastle-guest: skipping special file {}", src.display());
        }
        Ok(())
    }

    /// Owner first (chown clears setuid bits), then mtime, then mode.
    fn finish(&self, path: &Path, meta: &Metadata) -> Result<()> {
        self.chown(path)?;
        File::open(path)?.set_times(FileTimes::new().set_modified(meta.modified()?))?;
        fs::set_permissions(path, Permissions::from_mode(meta.mode() & 0o7777))?;
        Ok(())
    }

    fn chown(&self, path: &Path) -> Result<()> {
        if let Some((uid, gid)) = self.owner {
            lchown(path, Some(uid), Some(gid))?;
        }
        Ok(())
    }

    fn shown(&self, path: &Path) -> String {
        Path::new("/")
            .join(path.strip_prefix(self.root).unwrap_or(path))
            .display()
            .to_string()
    }
}
```

`File::open(path)?.set_times` on a file with mode `0o000` works because the guest runs as root. Tests never create such files.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sandcastle-guest copy && cargo clippy -p sandcastle-guest --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sandcastle-guest/src/copy.rs crates/sandcastle-guest/src/main.rs
git commit -m "Implement Docker COPY rules in the guest

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Host VM runner with read-only shares and a safe store lock

**Files:**
- Modify: `src/vm/krun.rs`, `src/vm/mod.rs`, `src/vm/child.rs`, `src/vm/landlock.rs`
- Modify: `src/store.rs`, `src/image.rs`, `src/doctor.rs`, `Cargo.toml`
- Modify: `tests/vm.rs`

**Interfaces:**
- Consumes: Task 1 proto types.
- Produces (used by Tasks 6, 7, 10, 11):
  - `vm::Resources { vcpus: u8, ram_mib: u32 }` + `Default`
  - `vm::Vm<'a> { exe: &'a Path, install: &'a Install, store: &'a Store, resources: Resources }` with `run(&self, job: &Job, ctx: Option<&Path>) -> Result<Finished>`
  - `vm::Finished { pub status: Status }` with `out_dir(&self) -> PathBuf` (removes the job dir on drop)
  - `vm::open_guest_file(&Path) -> Result<Option<File>>`
  - `vm::Share { tag, path, read_only }`
  - `Store::blobs_dir(&self) -> PathBuf`
  - `ConfigState::lower_layers(&self) -> Result<Vec<LowerLayer>>`
- `run_job` is removed; all callers move to `Vm::run`.

- [ ] **Step 1: Write the failing unit tests**

In `src/vm/mod.rs` `tests`, add:

```rust
    #[test]
    fn guest_error_in_status_is_reported() {
        let err = outcome(
            Some(1),
            Some(Status { exit_code: 1, error: Some("mounting the overlay: EINVAL".into()), ..Default::default() }),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("guest helper failed: mounting the overlay"), "{err:#}");
    }

    #[test]
    fn guest_files_must_be_regular_and_not_links() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        fs::write(&secret, b"{}").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("status.json")).unwrap();
        assert!(open_guest_file(&dir.path().join("status.json")).is_err());
        assert!(open_guest_file(&dir.path().join("missing")).unwrap().is_none());
        let fifo = dir.path().join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let err = open_guest_file(&fifo).unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
    }
```

In `src/image.rs` tests, add:

```rust
    #[test]
    fn lower_layers_map_media_types_and_reject_zstd() {
        let d = |n: u8| Digest::from_str(&format!("sha256:{}", format!("{n:02x}").repeat(32))).unwrap();
        let mut state = sample_state();
        state.layers = vec![
            Descriptor::new(MediaType::ImageLayerGzip, 1, d(1)),
            Descriptor::new(MediaType::ImageLayer, 1, d(2)),
        ];
        state.diff_ids = vec![d(3), d(4)];
        let lower = state.lower_layers().unwrap();
        assert_eq!(lower[0].media_type, sandcastle_proto::LAYER_TAR_GZIP);
        assert_eq!(lower[0].blob, d(1).to_string());
        assert_eq!(lower[1].diff_id, d(4).to_string());
        assert_eq!(lower[1].media_type, sandcastle_proto::LAYER_TAR);

        state.layers[1] = Descriptor::new(MediaType::ImageLayerZstd, 1, d(2));
        let err = state.lower_layers().unwrap_err();
        assert!(format!("{err:#}").contains("zstd"), "{err:#}");
    }
```

If `image.rs` tests have no `sample_state()` helper, create one in the test module. It builds a `ConfigState` by calling `ConfigState::from_base` on a minimal `ImageConfiguration` with `architecture`, `os` and empty `rootfs`. Reuse whichever helper the existing tests use if there is one.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle --lib`
Expected: compile errors (`open_guest_file`, `lower_layers` and the `error` handling don't exist).

- [ ] **Step 3: libkrun read-only shares**

In `src/vm/krun.rs`:
- Replace `type AddVirtiofsFn = …` with
  `type AddVirtiofsFn = unsafe extern "C" fn(u32, *const c_char, *const c_char, u64, bool) -> i32;`
- Load it with `add_virtiofs: symbol(&lib, c"krun_add_virtiofs3")?,`.
- Replace `Ctx::add_virtiofs` with:

```rust
    /// `shm_size` 0 keeps libkrun's default DAX window.
    pub fn add_virtiofs(&mut self, tag: &str, dir: &Path, read_only: bool) -> Result<()> {
        let tag = CString::new(tag)?;
        let dir = path_cstring(dir)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; libkrun copies them.
        check(
            unsafe { (self.krun.add_virtiofs)(self.id, tag.as_ptr(), dir.as_ptr(), 0, read_only) },
            "krun_add_virtiofs3",
        )
        .map(drop)
    }
```

- [ ] **Step 4: Runner, shares, guest-file reads**

Add `rustix = { version = "1.1.5", features = ["fs", "process"] }` to the root `[dependencies]`, and `tar = "0.4.46"` to a new root `[dev-dependencies]` section together with the existing dev-deps.

Replace the body of `src/vm/mod.rs` from `pub const SPEC_FILE` through the end of `outcome` with:

```rust
/// VM description the parent writes into the job dir for the `__vm` child.
pub const SPEC_FILE: &str = "vm.json";
/// The parent's pid, so the child can tell whether it outlived it.
pub const PARENT_PID_ENV: &str = "SANDCASTLE_PARENT_PID";
/// Largest `status.json` read from the guest.
const MAX_STATUS_BYTES: u64 = 1 << 20;

#[cfg(target_os = "linux")]
const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";
#[cfg(target_os = "macos")]
const LIB_PATH_ENV: &str = "DYLD_LIBRARY_PATH";

/// A host directory shared into the guest at `/<tag>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSpec {
    pub lib_dir: PathBuf,
    pub guest_root: PathBuf,
    pub disk: PathBuf,
    pub shares: Vec<Share>,
    pub vcpus: u8,
    pub ram_mib: u32,
}

/// vCPUs and memory of each build-step VM.
#[derive(Debug, Clone, Copy)]
pub struct Resources {
    pub vcpus: u8,
    pub ram_mib: u32,
}

impl Default for Resources {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            vcpus: cpus.min(usize::from(u8::MAX)) as u8,
            ram_mib: 2048,
        }
    }
}

/// Runs guest jobs. `exe` is the signed `sandcastle` binary used for the
/// `__vm` child.
pub struct Vm<'a> {
    pub exe: &'a Path,
    pub install: &'a Install,
    pub store: &'a Store,
    pub resources: Resources,
}

impl Vm<'_> {
    /// Runs `job` in a fresh microVM. `ctx` is shared read-only at `/ctx`.
    pub fn run(&self, job: &Job, ctx: Option<&Path>) -> Result<Finished> {
        let mut finished = Finished {
            status: Status::default(),
            dir: self.store.new_job_dir()?,
        };
        let out_dir = finished.out_dir();
        fs::write(out_dir.join(JOB_FILE), serde_json::to_vec(job)?)?;
        let mut shares = vec![Share {
            tag: SHARE_OUT.into(),
            path: out_dir.clone(),
            read_only: false,
        }];
        if !matches!(job, Job::Probe { .. }) {
            shares.push(Share {
                tag: SHARE_BLOBS.into(),
                path: self.store.blobs_dir(),
                read_only: true,
            });
        }
        if let Some(ctx) = ctx {
            shares.push(Share {
                tag: SHARE_CTX.into(),
                path: ctx.to_path_buf(),
                read_only: true,
            });
        }
        let spec = VmSpec {
            lib_dir: self.install.lib_dir.clone(),
            guest_root: self.store.guest_root(),
            disk: self.store.disk(),
            shares,
            vcpus: self.resources.vcpus,
            ram_mib: self.resources.ram_mib,
        };
        fs::write(finished.dir.join(SPEC_FILE), serde_json::to_vec(&spec)?)?;

        // libkrun dlopens libkrunfw by bare file name; point the loader at the bundle.
        let exit = Command::new(self.exe)
            .arg("__vm")
            .arg(&finished.dir)
            .env(LIB_PATH_ENV, &self.install.lib_dir)
            .env(PARENT_PID_ENV, std::process::id().to_string())
            .status()
            .with_context(|| format!("starting {}", self.exe.display()))?;
        let status = read_status(&out_dir.join(STATUS_FILE))?;
        finished.status = outcome(exit.code(), status)?;
        Ok(finished)
    }
}

/// A completed job. Its directory, with the files the guest left in
/// `out`, is removed when this is dropped.
pub struct Finished {
    pub status: Status,
    dir: PathBuf,
}

impl Finished {
    pub fn out_dir(&self) -> PathBuf {
        self.dir.join("out")
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            eprintln!("sandcastle: could not remove {}: {e}", self.dir.display());
        }
    }
}

/// Opens a file the guest wrote to its `out` share. The guest controls that
/// directory, so links and special files are refused and the open never
/// blocks. A missing file is `None`.
pub fn open_guest_file(path: &Path) -> Result<Option<File>> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => {
            return Err(std::io::Error::from(e))
                .with_context(|| format!("opening guest file {}", path.display()));
        }
    };
    let file = File::from(fd);
    ensure!(
        file.metadata()?.is_file(),
        "guest file {} is not a regular file",
        path.display()
    );
    Ok(Some(file))
}

fn read_status(path: &Path) -> Result<Option<Status>> {
    let Some(file) = open_guest_file(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_STATUS_BYTES).read_to_end(&mut bytes)?;
    match serde_json::from_slice(&bytes) {
        Ok(status) => Ok(Some(status)),
        Err(e) => {
            // The guest commits status atomically, so garbage means no commit.
            let e = anyhow::Error::new(e).context("parsing guest status");
            eprintln!("sandcastle: ignoring unreadable guest status: {e:#}");
            Ok(None)
        }
    }
}

/// Decides a job's result. A missing status file means the helper never
/// finished, whatever the child exit code looks like (libkrun's init itself
/// uses 125/126/127).
pub fn outcome(child_exit: Option<i32>, status: Option<Status>) -> Result<Status> {
    match (status, child_exit) {
        (Some(Status { error: Some(error), .. }), _) => bail!("guest helper failed: {error}"),
        (Some(status), _) => Ok(status),
        (None, Some(code)) => bail!(
            "VM or guest helper setup failed before the step ran (VM process exited with {code})"
        ),
        (None, None) => bail!(
            "VM or guest helper setup failed before the step ran (VM process was killed by a signal)"
        ),
    }
}
```

Imports for the top of `src/vm/mod.rs`:

```rust
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sandcastle_proto::{JOB_FILE, Job, SHARE_BLOBS, SHARE_CTX, SHARE_OUT, STATUS_FILE, Status};
use serde::{Deserialize, Serialize};

use crate::install::Install;
use crate::store::Store;
```

Remove `const RAM_MIB`. Remove `run_job`.

- [ ] **Step 5: Child, Landlock, store**

`src/vm/child.rs` `try_enter` becomes:

```rust
fn try_enter(job_dir: &Path) -> Result<Infallible> {
    #[cfg(target_os = "linux")]
    die_with_parent()?;
    let spec: VmSpec = serde_json::from_slice(&fs::read(job_dir.join(SPEC_FILE))?)?;
    let krun = Krun::load(&spec.lib_dir)?;
    krun.set_log_level_error()?;
    let mut ctx = krun.create_ctx()?;
    ctx.set_vm_config(spec.vcpus, spec.ram_mib)?;
    ctx.set_root(&spec.guest_root)?;
    ctx.add_disk("store", &spec.disk, false)?;
    for share in &spec.shares {
        ctx.add_virtiofs(&share.tag, &share.path, share.read_only)?;
    }
    ctx.set_workdir("/")?;
    ctx.set_exec(GUEST_HELPER_PATH, &[], &["HOME=/"])?;
    #[cfg(target_os = "linux")]
    super::landlock::restrict(&spec)?;
    Err(ctx.start_enter())
}

/// The VM keeps the store disk open, so it must not outlive the build that
/// started it. (The store lock is inherited too, so an orphan never shares
/// the disk with a new build; this just ends it promptly.) The signal fires
/// when the spawning *thread* exits, which is the build's main thread.
#[cfg(target_os = "linux")]
fn die_with_parent() -> Result<()> {
    use anyhow::ensure;
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    let parent: u32 = std::env::var(super::PARENT_PID_ENV)?.parse()?;
    ensure!(
        std::os::unix::process::parent_id() == parent,
        "sandcastle exited before the VM started"
    );
    Ok(())
}
```

Use `sandcastle_proto::GUEST_HELPER_PATH` as the import.

`src/vm/landlock.rs` `restrict` becomes:

```rust
pub fn restrict(spec: &VmSpec) -> Result<()> {
    // guest-root is the VM's `/` and is written by init (mount points).
    let mut rw_dirs = vec![spec.guest_root.as_path()];
    // libkrun dlopens libkrunfw from here inside krun_start_enter.
    let mut ro_dirs = vec![spec.lib_dir.as_path()];
    for share in &spec.shares {
        if share.read_only {
            ro_dirs.push(share.path.as_path());
        } else {
            rw_dirs.push(share.path.as_path());
        }
    }
    let enforced = restrict_paths(&Rules {
        rw_dirs: &rw_dirs,
        ro_dirs: &ro_dirs,
        rw_files: &[spec.disk.as_path()],
        devices: &[Path::new("/dev/kvm")],
    })?;
    if !enforced {
        eprintln!(
            "sandcastle: warning: kernel lacks Landlock; VM process runs without a host sandbox"
        );
    }
    Ok(())
}
```

`src/store.rs`:
- `GUEST_DIRS` becomes `&["dev", "proc", "sys", "tmp", "out", "store", "blobs", "ctx"]`.
- Add:

```rust
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }
```

  and make `blobs()` use it.
- In `Store::open`, right after the `match lock.try_lock()` block:

```rust
        // `__vm` children inherit the lock, so a VM that outlives this
        // process keeps the store locked instead of sharing the ext4 disk
        // with the next build.
        rustix::io::fcntl_setfd(&lock, rustix::io::FdFlags::empty())
            .context("making the store lock inheritable")?;
```

- [ ] **Step 6: `lower_layers`**

Add to `impl ConfigState` in `src/image.rs`:

```rust
    /// The layer stack as guest jobs take it, bottom first. Only layer
    /// formats the guest can unpack are accepted.
    pub fn lower_layers(&self) -> Result<Vec<LowerLayer>> {
        self.layers
            .iter()
            .zip(&self.diff_ids)
            .map(|(desc, diff_id)| {
                let media_type = match desc.media_type() {
                    MediaType::ImageLayer => LAYER_TAR,
                    MediaType::ImageLayerGzip => LAYER_TAR_GZIP,
                    other => bail!(
                        "layer {} is {other}; sandcastle can only unpack uncompressed and gzip layers (zstd is not supported yet)",
                        desc.digest()
                    ),
                };
                Ok(LowerLayer {
                    diff_id: diff_id.to_string(),
                    blob: desc.digest().to_string(),
                    media_type: media_type.to_string(),
                })
            })
            .collect()
    }
```

Imports: `use anyhow::bail;` and `use sandcastle_proto::{LAYER_TAR, LAYER_TAR_GZIP, LowerLayer};`.

- [ ] **Step 7: Doctor and the VM integration tests**

`src/doctor.rs`: replace the `run_job(…)` call with:

```rust
    let vm = Vm { exe, install, store: &store, resources: Resources::default() };
    let status = vm.run(&Job::Probe { exit_code: 0 }, None)?.status.clone();
```

and import `crate::vm::{Resources, Vm}`.

`tests/vm.rs`: replace `run_job(&exe, &store, &install, &job)` calls with a helper:

```rust
fn run(exe: &std::path::Path, install: &Install, store: &Store, job: &Job) -> Status {
    let vm = Vm { exe, install, store, resources: Resources::default() };
    vm.run(job, None).unwrap().status.clone()
}
```

Update the imports to use `sandcastle::vm::{Resources, Vm}` and `sandcastle_proto::{Job, Status}`.

- [ ] **Step 8: Run everything**

Run:
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
just it
```
Expected: unit tests PASS, and the two probe VM tests and the doctor CLI test PASS. (Copy `lib/` from the main checkout into the worktree first if it is missing.)

- [ ] **Step 9: Commit**

```bash
git add -A src Cargo.toml Cargo.lock tests/vm.rs
git commit -m "Run jobs with read-only blob and context shares; harden guest file reads

The __vm child inherits the store lock and dies with its parent on Linux,
so an orphaned VM never shares the store disk with a new build.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Guest runtime: layer unpack, overlay, commit, COPY job

**Files:**
- Create: `crates/sandcastle-guest/src/store.rs`, `unpack.rs`, `overlay.rs`, `commit.rs`
- Modify: `crates/sandcastle-guest/src/linux.rs`, `crates/sandcastle-guest/src/main.rs`
- Modify: `tests/vm.rs`

**Interfaces:**
- Consumes: `layer::*` (Task 2), `copy::Copy` (Task 4), proto (Task 1), and on the host `Vm`, `Finished`, `ConfigState::lower_layers` (Task 5).
- Produces (used by Task 7):
  - `store::Store::{mount() -> Result<Store>, unmount(self) -> Result<()>, layer_dir(&self, diff_id: &str) -> Result<PathBuf>, work(&self, name: &str) -> Result<PathBuf>, ensure_layers(&self, lower: &[LowerLayer]) -> Result<()>, lower_dirs(&self, lower: &[LowerLayer]) -> Result<Vec<PathBuf>>}`
  - `overlay::Overlay::{mount(lowers: &[PathBuf], work: &Path) -> Result<Overlay>, merged(&self) -> &Path, unmount(self) -> Result<PathBuf /*upper*/>}`
  - `linux::commit_upper(store: &Store, upper: &Path, out: &Path) -> Result<Status>`
  - `linux::OUT: &str`

Guest layout on the store disk:
- `/store/layers/<diff_id hex>/`: one directory per layer, in overlay format. The directory's presence means the layer is complete.
- `/store/work/`: per-job scratch, wiped at every boot.

- [ ] **Step 1: Write the failing integration test**

Add to `tests/vm.rs`:

```rust
use std::collections::BTreeMap;
use std::io::Read;

use sandcastle::image::ConfigState;
use sandcastle::registry;
use sandcastle_proto::{CopyJob, LAYER_FILE, LowerLayer};

const BUSYBOX: &str = "mirror.gcr.io/library/busybox:1.36";

/// Pulls busybox into the store and returns its layer stack.
fn busybox(store: &Store) -> Vec<LowerLayer> {
    let image = registry::pull(&store.blobs(), BUSYBOX).unwrap();
    ConfigState::from_base(&image.config, image.layers)
        .unwrap()
        .lower_layers()
        .unwrap()
}

/// path → (entry type, uid, contents or link target) of the job's layer.tar.
fn layer_entries(out_dir: &std::path::Path) -> BTreeMap<String, (tar::EntryType, u64, Vec<u8>)> {
    let file = std::fs::File::open(out_dir.join(LAYER_FILE)).unwrap();
    let mut archive = tar::Archive::new(file);
    let mut map = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let kind = entry.header().entry_type();
        let uid = entry.header().uid().unwrap();
        let body = match entry.link_name().unwrap() {
            Some(target) => target.to_string_lossy().into_owned().into_bytes(),
            None => {
                let mut b = Vec::new();
                entry.read_to_end(&mut b).unwrap();
                b
            }
        };
        map.insert(path, (kind, uid, body));
    }
    map
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn copy_job_writes_context_files_as_root() {
    let (dir, exe, install, store) = store();
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(ctx.join("tree/sub")).unwrap();
    std::fs::write(ctx.join("hello.txt"), "hello\n").unwrap();
    std::fs::write(ctx.join("tree/sub/b.txt"), "b\n").unwrap();
    let job = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec!["hello.txt".into(), "tree".into()],
        dest: "/app/".into(),
        workdir: "/srv".into(),
    });
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let finished = vm.run(&job, Some(&ctx)).unwrap();
    assert_eq!(finished.status.exit_code, 0);
    let diff_id = finished.status.layer.clone().expect("a layer");
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(entries["app/hello.txt"], (tar::EntryType::Regular, 0, b"hello\n".to_vec()));
    assert_eq!(entries["app/sub/b.txt"].2, b"b\n");
    assert!(entries.contains_key("srv"), "workdir is created: {:?}", entries.keys());

    let bytes = std::fs::read(finished.out_dir().join(LAYER_FILE)).unwrap();
    assert_eq!(sandcastle::blobs::sha256(&bytes).to_string(), diff_id);
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn copy_job_missing_source_is_a_guest_error() {
    let (dir, exe, install, store) = store();
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    let job = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec!["nope.txt".into()],
        dest: "/x".into(),
        workdir: "/".into(),
    });
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let err = vm.run(&job, Some(&ctx)).err().unwrap();
    assert!(format!("{err:#}").contains("nope.txt: not found in the build context"), "{err:#}");
}
```

Note: `store()` returns `(TempDir, PathBuf, Install, Store)`. The tests above bind the temp dir as `dir`.

- [ ] **Step 2: Run them to see them fail**

Run: `just it`
Expected: the new tests FAIL with `guest helper failed: job not supported by this helper`.

- [ ] **Step 3: Guest store and unpack**

`crates/sandcastle-guest/src/store.rs`:

```rust
//! The ext4 store disk inside the guest: `/store/layers/<diff_id hex>/`
//! holds one overlay-format directory per layer, `/store/work/` is per-job
//! scratch wiped at every boot.

use std::ffi::CStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use sandcastle_proto::{LowerLayer, SHARE_BLOBS};

use crate::{layer, unpack};

/// First virtio-blk device: the store disk added by the host.
const STORE_DEV: &str = "/dev/vda";
const STORE: &str = "/store";
const BLOBS: &str = "/blobs";

pub struct Store {
    layers: PathBuf,
    work: PathBuf,
}

impl Store {
    pub fn mount() -> Result<Self> {
        mount(STORE_DEV, STORE, "ext4", MountFlags::NOATIME, None::<&CStr>)
            .context("mounting the store disk")?;
        let root = Path::new(STORE);
        let store = Self {
            layers: root.join("layers"),
            work: root.join("work"),
        };
        match fs::remove_dir_all(&store.work) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("removing work left by an interrupted job"),
        }
        fs::create_dir_all(&store.layers)?;
        fs::create_dir_all(&store.work)?;
        Ok(store)
    }

    /// Unmounting flushes the guest page cache to the disk; without it the
    /// VM teardown would drop written layers.
    pub fn unmount(self) -> Result<()> {
        unmount(STORE, UnmountFlags::empty()).context("unmounting the store disk")
    }

    pub fn layer_dir(&self, diff_id: &str) -> Result<PathBuf> {
        Ok(self.layers.join(layer::digest_hex(diff_id)?))
    }

    /// A fresh directory under `/store/work`.
    pub fn work(&self, name: &str) -> Result<PathBuf> {
        let dir = self.work.join(name);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    /// Unpacks every layer of `lower` that is not in the store yet.
    pub fn ensure_layers(&self, lower: &[LowerLayer]) -> Result<()> {
        let mut blobs_mounted = false;
        for l in lower {
            let dest = self.layer_dir(&l.diff_id)?;
            if dest.is_dir() {
                continue;
            }
            if !blobs_mounted {
                mount(SHARE_BLOBS, BLOBS, "virtiofs", MountFlags::RDONLY, None::<&CStr>)
                    .context("mounting the blob share")?;
                blobs_mounted = true;
            }
            let blob = Path::new(BLOBS).join("sha256").join(layer::digest_hex(&l.blob)?);
            let tmp = self.work.join(format!("unpack-{}", layer::digest_hex(&l.diff_id)?));
            unpack::unpack(&blob, &l.media_type, &l.diff_id, &tmp)
                .with_context(|| format!("unpacking layer {}", l.diff_id))?;
            fs::rename(&tmp, &dest)?;
        }
        Ok(())
    }

    /// Overlay lower dirs for a bottom-first stack: top-most first.
    pub fn lower_dirs(&self, lower: &[LowerLayer]) -> Result<Vec<PathBuf>> {
        let ids: Vec<String> = lower.iter().map(|l| l.diff_id.clone()).collect();
        layer::overlay_stack(&ids)
            .into_iter()
            .map(|id| self.layer_dir(id))
            .collect()
    }
}
```

`crates/sandcastle-guest/src/unpack.rs`:

```rust
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
    ensure!(got == diff_id, "layer decompressed to {got}, expected {diff_id}");
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
                mknodat(CWD, &path, FileType::CharacterDevice, Mode::empty(), makedev(0, 0))
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
    let t = Timespec { tv_sec: mtime as i64, tv_nsec: 0 };
    utimensat(
        CWD,
        path,
        &Timestamps { last_access: t, last_modification: t },
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}
```

- [ ] **Step 4: Overlay and commit**

`crates/sandcastle-guest/src/overlay.rs`:

```rust
//! The overlay a step runs in: layer dirs as lowers (added with
//! `lowerdir+`, so long stacks fit), a fresh upper dir that becomes the
//! step's layer, and options that keep the upper dir a self-contained diff.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::fs::CWD;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, UnmountFlags, fsconfig_create,
    fsconfig_set_string, fsmount, fsopen, move_mount, unmount,
};

const OPTIONS: [(&str, &str); 4] = [
    ("index", "off"),
    ("metacopy", "off"),
    ("redirect_dir", "off"),
    ("xino", "off"),
];

pub struct Overlay {
    merged: PathBuf,
    upper: PathBuf,
}

impl Overlay {
    /// Mounts `lowers` (top-most first) at `work/merged` with an empty
    /// upper dir `work/upper`.
    pub fn mount(lowers: &[PathBuf], work: &Path) -> Result<Self> {
        let upper = work.join("upper");
        let ovl_work = work.join("ovl-work");
        let merged = work.join("merged");
        let empty = work.join("empty");
        for dir in [&upper, &ovl_work, &merged, &empty] {
            fs::create_dir_all(dir)?;
        }
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        // Overlay needs at least one lower; an image may have no layers.
        let lowers: Vec<&Path> = if lowers.is_empty() {
            vec![&empty]
        } else {
            lowers.iter().map(PathBuf::as_path).collect()
        };
        for lower in lowers {
            fsconfig_set_string(&fs, "lowerdir+", lower)
                .with_context(|| format!("adding overlay lower {}", lower.display()))?;
        }
        fsconfig_set_string(&fs, "upperdir", &upper)?;
        fsconfig_set_string(&fs, "workdir", &ovl_work)?;
        for (key, value) in OPTIONS {
            fsconfig_set_string(&fs, key, value)?;
        }
        fsconfig_create(&fs).context("creating the overlay")?;
        let mnt = fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(&mnt, "", CWD, &merged, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)
            .context("mounting the overlay")?;
        Ok(Self { merged, upper })
    }

    pub fn merged(&self) -> &Path {
        &self.merged
    }

    /// Unmounts and returns the upper dir, now an ordinary directory.
    pub fn unmount(self) -> Result<PathBuf> {
        unmount(&self.merged, UnmountFlags::empty())
            .or_else(|_| unmount(&self.merged, UnmountFlags::DETACH))
            .context("unmounting the overlay")?;
        Ok(self.upper)
    }
}
```

`crates/sandcastle-guest/src/commit.rs`:

```rust
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
/// Linux caps both an xattr name list and one value at 64 KiB.
const XATTR_MAX: usize = 1 << 16;

/// Writes `upper` as an uncompressed layer tar to `out` and returns its
/// diff_id, or `None` (and no file) when the step changed nothing.
pub fn commit(upper: &Path, out: &Path) -> Result<Option<String>> {
    if fs::read_dir(upper)?.next().is_none() {
        return Ok(None);
    }
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut tar = tar::Builder::new(HashWriter::new(BufWriter::with_capacity(WRITE_BUFFER, file)));
    walk(&mut tar, upper, Path::new(""), &mut HashMap::new())?;
    let (buffered, diff_id) = tar.into_inner()?.finish();
    let file = buffered.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(Some(diff_id))
}

fn walk<W: Write>(
    tar: &mut tar::Builder<W>,
    root: &Path,
    rel: &Path,
    links: &mut HashMap<(u64, u64), PathBuf>,
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
        let (opaque, xattrs) = read_xattrs(&path)?;
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
                if let Some(first) = links.get(&key) {
                    layer::append(tar, &rel, &Kind::Hardlink(first.clone()), &m, io::empty())?;
                    continue;
                }
                links.insert(key, rel.clone());
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
            walk(tar, root, &rel, links)?;
        }
    }
    Ok(())
}

/// Returns whether the overlay marked the directory opaque, and the xattrs
/// that belong in the image.
fn read_xattrs(path: &Path) -> Result<(bool, Vec<(String, Vec<u8>)>)> {
    let mut names = vec![0u8; XATTR_MAX];
    let len = match llistxattr(path, &mut names[..]) {
        Ok(len) => len,
        Err(rustix::io::Errno::NOTSUP) => return Ok((false, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("listing xattrs of {}", path.display())),
    };
    let mut opaque = false;
    let mut out = Vec::new();
    let mut value = vec![0u8; XATTR_MAX];
    for name in names[..len].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        let n = lgetxattr(path, name.as_str(), &mut value[..])?;
        if name == OPAQUE_XATTR {
            opaque = &value[..n] == b"y";
        } else if !name.starts_with(OVERLAY_XATTR_PREFIX) {
            out.push((name, value[..n].to_vec()));
        }
    }
    Ok((opaque, out))
}
```

- [ ] **Step 5: Job dispatch and the COPY job**

Replace `crates/sandcastle-guest/src/linux.rs` with:

```rust
use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process;

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{
    CopyJob, JOB_FILE, Job, LAYER_FILE, SHARE_CTX, SHARE_OUT, STATUS_FILE, Status,
};

use crate::commit;
use crate::copy::Copy;
use crate::overlay::Overlay;
use crate::store::Store;

pub const OUT: &str = "/out";
const CTX: &str = "/ctx";

pub fn main() -> ! {
    match job_main() {
        Ok(code) => process::exit(code),
        Err(e) => {
            // No status file: the host reports a setup failure.
            eprintln!("sandcastle-guest: {e:#}");
            process::exit(125)
        }
    }
}

fn job_main() -> Result<i32> {
    mount(SHARE_OUT, OUT, "virtiofs", MountFlags::empty(), None::<&CStr>)
        .context("mounting the out share")?;
    let out = Path::new(OUT);
    let status = match read_job(out).and_then(|job| run_job(job, out)) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("sandcastle-guest: {e:#}");
            Status { exit_code: 1, error: Some(format!("{e:#}")), ..Default::default() }
        }
    };
    write_status(out, &status)?;
    Ok(status.exit_code)
}

fn read_job(out: &Path) -> Result<Job> {
    serde_json::from_slice(&fs::read(out.join(JOB_FILE)).context("reading job")?)
        .context("parsing job")
}

fn run_job(job: Job, out: &Path) -> Result<Status> {
    match job {
        Job::Probe { exit_code } => Ok(Status {
            exit_code,
            probe: Some(crate::probe::run()?),
            ..Default::default()
        }),
        Job::Copy(job) => with_store(|store| copy_job(store, &job, out)),
        Job::Run(_) => anyhow::bail!("job not supported by this helper"),
    }
}

/// Mounts the store for `f` and always unmounts it, so written layers reach
/// the disk before the VM stops.
pub fn with_store(f: impl FnOnce(&Store) -> Result<Status>) -> Result<Status> {
    let store = Store::mount()?;
    let result = f(&store);
    let unmounted = store.unmount();
    let status = result?;
    unmounted?;
    Ok(status)
}

fn copy_job(store: &Store, job: &CopyJob, out: &Path) -> Result<Status> {
    mount(SHARE_CTX, CTX, "virtiofs", MountFlags::RDONLY, None::<&CStr>)
        .context("mounting the build context")?;
    store.ensure_layers(&job.lower)?;
    let work = store.work("copy")?;
    let overlay = Overlay::mount(&store.lower_dirs(&job.lower)?, &work)?;
    let copy = Copy {
        ctx: Path::new(CTX),
        root: overlay.merged(),
        workdir: &job.workdir,
        owner: Some((0, 0)),
    };
    let result = copy
        .ensure_workdir()
        .and_then(|()| copy.run(&job.sources, &job.dest));
    let upper = overlay.unmount()?;
    result?;
    commit_upper(store, &upper, out)
}

/// Writes the step's layer and keeps its upper dir as the layer's store copy.
pub fn commit_upper(store: &Store, upper: &Path, out: &Path) -> Result<Status> {
    let layer = commit::commit(upper, &out.join(LAYER_FILE))?;
    match &layer {
        Some(diff_id) => {
            let dest = store.layer_dir(diff_id)?;
            if dest.exists() {
                fs::remove_dir_all(upper)?;
            } else {
                fs::rename(upper, &dest)?;
            }
        }
        None => fs::remove_dir(upper)?,
    }
    Ok(Status { exit_code: 0, layer, ..Default::default() })
}

fn write_status(out: &Path, status: &Status) -> Result<()> {
    // Write-then-rename so the host never sees a partial status.
    let tmp = out.join(format!("{STATUS_FILE}.tmp"));
    let mut file = File::create(&tmp).context("creating status file")?;
    file.write_all(&serde_json::to_vec(status)?)
        .context("writing status")?;
    // The VM is torn down right after exit; make sure the host sees the bytes.
    file.sync_all().context("writing status")?;
    fs::rename(&tmp, out.join(STATUS_FILE)).context("writing status")?;
    Ok(())
}
```

`main.rs` module list for Linux:

```rust
#[cfg(target_os = "linux")]
mod commit;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod overlay;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod unpack;
```

- [ ] **Step 6: Run the checks**

Run:
```bash
cargo clippy -p sandcastle-guest --target aarch64-unknown-linux-musl -- -D warnings
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
just it
```
Expected: everything PASSES, including `copy_job_writes_context_files_as_root` and `copy_job_missing_source_is_a_guest_error`.

- [ ] **Step 7: Commit**

```bash
git add crates/sandcastle-guest tests/vm.rs
git commit -m "Run COPY jobs in the guest: unpack layers, overlay, commit

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Guest RUN job

**Files:**
- Create: `crates/sandcastle-guest/src/run.rs`
- Modify: `crates/sandcastle-guest/src/linux.rs`, `crates/sandcastle-guest/src/main.rs`
- Modify: `tests/vm.rs`

**Interfaces:**
- Consumes:
  - `Store`, `Overlay`, `with_store`, `commit_upper` (Task 6)
  - `user::resolve` (Task 3)
  - `copy::resolve_in_root` (Task 4)
  - `RunJob` (Task 1)
- Produces: `run::run_job(store: &Store, job: &RunJob, out: &Path) -> Result<Status>`, `run::EXEC_ARG`, and `run::exec_main(spec: Option<OsString>) -> !`.

**How a RUN step runs:**
1. The helper unpacks the lower layers.
2. It adds the stub layer at the bottom and mounts the overlay.
3. It mounts a tmpfs on the VM's `/tmp` and writes `resolv.conf`, `hosts` and `hostname` there.
4. It resolves the user from the merged root's `passwd` and `group`.
5. It calls `unshare(CLONE_NEWPID)` and spawns itself as `/sandcastle-guest --exec <spec json>`. That child is PID 1 of the new namespace.
6. The child does `unshare(CLONE_NEWNS)`, makes `/` private, mounts `proc`/`sysfs`/the `/dev` tmpfs, and binds the three etc files over regular-file targets.
7. The child then chroots, creates the workdir, sets groups/gid/uid, and `exec`s the command. Exec failures exit 127 for not-found and 126 otherwise. Setup failures exit 125.
8. When the command exits, the kernel kills the rest of the namespace and drops its mounts. The helper unmounts the overlay and commits it if the exit code is 0.

- [ ] **Step 1: Write the failing integration tests**

Add to `tests/vm.rs`:

```rust
use sandcastle_proto::RunJob;

fn run_job(lower: Vec<LowerLayer>, script: &str, user: &str) -> Job {
    Job::Run(RunJob {
        lower,
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".into()],
        user: user.into(),
        workdir: "/work".into(),
        resolv_conf: "nameserver 8.8.8.8\n".into(),
    })
}

/// Runs `job` and, if it made a layer, returns the lower stack with it on top.
fn run_step(
    vm: &Vm,
    store: &Store,
    lower: &[LowerLayer],
    job: &Job,
) -> (sandcastle::vm::Finished, Vec<LowerLayer>) {
    let finished = vm.run(job, None).unwrap();
    let mut stack = lower.to_vec();
    if let Some(diff_id) = &finished.status.layer {
        // Ingest the layer so later jobs can find it as a blob too.
        let mut writer = sandcastle::image::LayerWriter::new(&store.blobs()).unwrap();
        std::io::copy(
            &mut std::fs::File::open(finished.out_dir().join(LAYER_FILE)).unwrap(),
            &mut writer,
        )
        .unwrap();
        let layer = writer.finish().unwrap();
        assert_eq!(layer.diff_id.to_string(), *diff_id);
        stack.push(LowerLayer {
            diff_id: diff_id.clone(),
            blob: layer.descriptor.digest().to_string(),
            media_type: sandcastle_proto::LAYER_TAR_GZIP.into(),
        });
    }
    (finished, stack)
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_commits_changes_and_whiteouts() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let job = run_job(base.clone(), "echo hi > out.txt && rm /etc/group && ls /dev/null /proc/self >/dev/null", "");
    let (finished, _) = run_step(&vm, &store, &base, &job);
    assert_eq!(finished.status.exit_code, 0);
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(entries["work/out.txt"].2, b"hi\n");
    assert!(entries.contains_key("etc/.wh.group"), "{:?}", entries.keys());
    for stub in ["etc/resolv.conf", "etc/hosts", "dev", "proc", "sys"] {
        assert!(!entries.contains_key(stub), "{stub} leaked into the layer");
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_exit_codes_and_no_op_steps() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let status = vm.run(&run_job(base.clone(), "exit 3", ""), None).unwrap().status.clone();
    assert_eq!((status.exit_code, status.layer), (3, None));
    let mut job = run_job(base.clone(), "", "");
    if let Job::Run(r) = &mut job {
        r.argv = vec!["/no/such/binary".into()];
    }
    assert_eq!(vm.run(&job, None).unwrap().status.exit_code, 127);
    let mut noop = run_job(base, "true", "");
    if let Job::Run(r) = &mut noop {
        r.workdir = "/".into();
    }
    let status = vm.run(&noop, None).unwrap().status.clone();
    assert_eq!((status.exit_code, status.layer), (0, None));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_drops_to_user() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let job = run_job(base.clone(), "id -u > /tmp/uid; echo $HOME > /tmp/home", "65534");
    let (finished, _) = run_step(&vm, &store, &base, &job);
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(entries["tmp/uid"].2, b"65534\n");
    assert_eq!(entries["tmp/home"].2, b"/home\n");
    let err = vm.run(&run_job(base, "true", "ghost"), None).err().unwrap();
    assert!(format!("{err:#}").contains("unable to find user ghost"), "{err:#}");
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_kills_leftover_processes() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let status = vm
        .run(&run_job(base, "sleep 1000 & echo started > /started", ""), None)
        .unwrap()
        .status
        .clone();
    assert_eq!(status.exit_code, 0);
    assert!(status.layer.is_some());
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn opaque_dir_hides_lower_contents() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let (_, stack) = run_step(&vm, &store, &base, &run_job(base.clone(), "mkdir /d && touch /d/a /d/b", ""));
    let (finished, stack) = run_step(&vm, &store, &stack, &run_job(stack.clone(), "rm -rf /d && mkdir /d && touch /d/c", ""));
    assert!(layer_entries(&finished.out_dir()).contains_key("d/.wh..wh..opq"));
    let (finished, _) = run_step(&vm, &store, &stack, &run_job(stack.clone(), "ls /d > /listing", ""));
    assert_eq!(layer_entries(&finished.out_dir())["listing"].2, b"c\n");
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_survives_resolv_conf_symlink() {
    // A RUN cannot replace /etc/resolv.conf (it is a bind mount during the
    // step), so the dangling link comes from a COPY layer, as from a base image.
    let (dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let base = busybox(&store);
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(ctx.join("root/etc")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", ctx.join("root/etc/resolv.conf")).unwrap();
    let copy = Job::Copy(CopyJob {
        lower: base.clone(),
        sources: vec!["root".into()],
        dest: "/".into(),
        workdir: "/".into(),
    });
    let finished = vm.run(&copy, Some(&ctx)).unwrap();
    let mut stack = base;
    let diff_id = finished.status.layer.clone().unwrap();
    let blobs = store.blobs();
    let mut writer = sandcastle::image::LayerWriter::new(&blobs).unwrap();
    std::io::copy(&mut std::fs::File::open(finished.out_dir().join(LAYER_FILE)).unwrap(), &mut writer).unwrap();
    let layer = writer.finish().unwrap();
    stack.push(LowerLayer {
        diff_id,
        blob: layer.descriptor.digest().to_string(),
        media_type: sandcastle_proto::LAYER_TAR_GZIP.into(),
    });
    let status = vm.run(&run_job(stack, "echo ok > /ok", ""), None).unwrap().status.clone();
    assert_eq!(status.exit_code, 0);
}
```

Make `sandcastle::image::LayerWriter` usable from tests. It is already `pub`, so check whether its `new` takes `&BlobStore`. The `run_step` helper above borrows `store.blobs()` as a temporary. If the borrow checker complains, bind `let blobs = store.blobs();` first.

- [ ] **Step 2: Run them to see them fail**

Run: `just it`
Expected: the new RUN tests FAIL with `guest helper failed: job not supported by this helper`.

- [ ] **Step 3: Implement `run.rs`**

```rust
//! `RUN`: the command runs chrooted in the overlay, in its own PID and mount
//! namespaces, as Docker's runtime would run it.

use std::convert::Infallible;
use std::ffi::{CStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Stdio};

use anyhow::{Context, Result};
use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
use rustix::mount::{MountFlags, MountPropagationFlags, mount, mount_bind, mount_change};
use rustix::process::{Gid, Uid, chroot};
use rustix::thread::{UnshareFlags, set_thread_groups, set_thread_res_gid, set_thread_res_uid};
use sandcastle_proto::{GUEST_HELPER_PATH, RunJob, Status};
use serde::{Deserialize, Serialize};

use crate::copy::resolve_in_root;
use crate::linux::commit_upper;
use crate::overlay::Overlay;
use crate::store::Store;
use crate::user;

/// Argument that makes the helper act as the step's exec child.
pub const EXEC_ARG: &str = "--exec";
const ETC_FILES: [&str; 3] = ["resolv.conf", "hosts", "hostname"];
/// tmpfs on the VM's `/tmp` holding the step's etc files.
const RUN_ETC: &str = "/tmp/sandcastle-etc";
const HOSTNAME: &str = "sandcastle";
const HOSTS: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\tsandcastle\n";
/// Exit codes of a shell for a command that could not start.
const EXIT_NOT_FOUND: i32 = 127;
const EXIT_CANNOT_EXEC: i32 = 126;
const EXIT_SETUP: i32 = 125;

/// What the exec child needs; passed as JSON in its argv.
#[derive(Serialize, Deserialize)]
struct ExecSpec {
    root: PathBuf,
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

pub fn run_job(store: &Store, job: &RunJob, out: &Path) -> Result<Status> {
    store.ensure_layers(&job.lower)?;
    let work = store.work("run")?;
    let mut lowers = store.lower_dirs(&job.lower)?;
    lowers.push(stub_layer(&work)?);
    write_etc(&job.resolv_conf)?;
    let overlay = Overlay::mount(&lowers, &work)?;
    let result = execute(overlay.merged(), job);
    let upper = overlay.unmount()?;
    let exit_code = result?;
    if exit_code != 0 {
        fs::remove_dir_all(&upper)?;
        return Ok(Status { exit_code, ..Default::default() });
    }
    commit_upper(store, &upper, out)
}

/// Bottom-most lower layer providing mount points, so binds never create
/// files in the upper dir.
fn stub_layer(work: &Path) -> Result<PathBuf> {
    let stubs = work.join("stubs");
    for dir in ["dev", "proc", "sys", "etc"] {
        fs::create_dir_all(stubs.join(dir))?;
    }
    for file in ETC_FILES {
        File::create(stubs.join("etc").join(file))?;
    }
    Ok(stubs)
}

fn write_etc(resolv_conf: &str) -> Result<()> {
    mount("tmpfs", "/tmp", "tmpfs", MountFlags::NOSUID | MountFlags::NODEV, None::<&CStr>)
        .context("mounting /tmp")?;
    let etc = Path::new(RUN_ETC);
    fs::create_dir(etc)?;
    fs::write(etc.join("resolv.conf"), resolv_conf)?;
    fs::write(etc.join("hosts"), HOSTS)?;
    fs::write(etc.join("hostname"), format!("{HOSTNAME}\n"))?;
    rustix::system::sethostname(HOSTNAME.as_bytes())?;
    Ok(())
}

fn execute(root: &Path, job: &RunJob) -> Result<i32> {
    let passwd = read_in_root(root, "/etc/passwd")?;
    let group = read_in_root(root, "/etc/group")?;
    let ids = user::resolve(&job.user, &passwd, &group)?;
    let mut env = job.env.clone();
    if !env.iter().any(|e| e.starts_with("HOME=")) {
        env.push(format!("HOME={}", ids.home));
    }
    let spec = ExecSpec {
        root: root.to_path_buf(),
        argv: job.argv.clone(),
        env,
        workdir: job.workdir.clone(),
        uid: ids.uid,
        gid: ids.gid,
        groups: ids.groups,
    };
    // SAFETY: CLONE_NEWPID only changes which namespace later children are
    // created in; it does not unshare the file table, the hazard
    // `unshare_unsafe` guards against.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWPID) }
        .context("creating the step's PID namespace")?;
    // The child is init of that namespace: when the command exits the kernel
    // kills whatever it left running, so the overlay can be unmounted.
    let status = Command::new(GUEST_HELPER_PATH)
        .arg(EXEC_ARG)
        .arg(serde_json::to_string(&spec)?)
        .stdin(Stdio::null())
        .status()
        .context("starting the step")?;
    Ok(exit_code(status))
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Reads a file of the image, resolving links inside the image root.
fn read_in_root(root: &Path, path: &str) -> Result<String> {
    match fs::read_to_string(resolve_in_root(root, Path::new(path))?) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {path}")),
    }
}

/// Entry point of `sandcastle-guest --exec <spec>`.
pub fn exec_main(spec: Option<OsString>) -> ! {
    let spec: ExecSpec = match spec
        .context("missing exec spec")
        .and_then(|s| Ok(serde_json::from_str(s.to_str().context("exec spec is not UTF-8")?)?))
    {
        Ok(spec) => spec,
        Err(e) => fail_setup(e),
    };
    if let Err(e) = setup(&spec) {
        fail_setup(e);
    }
    let err = Command::new(&spec.argv[0])
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().filter_map(|e| e.split_once('=')))
        .exec();
    eprintln!("sandcastle: exec {}: {err}", spec.argv[0]);
    process::exit(if err.kind() == io::ErrorKind::NotFound {
        EXIT_NOT_FOUND
    } else {
        EXIT_CANNOT_EXEC
    })
}

fn fail_setup(e: anyhow::Error) -> ! {
    eprintln!("sandcastle: preparing the step: {e:#}");
    process::exit(EXIT_SETUP)
}

fn setup(spec: &ExecSpec) -> Result<()> {
    anyhow::ensure!(!spec.argv.is_empty(), "empty command");
    // SAFETY: this process is single-threaded; CLONE_NEWNS does not touch
    // the file table.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }?;
    mount_change("/", MountPropagationFlags::REC | MountPropagationFlags::PRIVATE)?;
    let root = &spec.root;
    let hardened = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    mount("proc", root.join("proc"), "proc", hardened, None::<&CStr>).context("mounting /proc")?;
    mount("sysfs", root.join("sys"), "sysfs", hardened | MountFlags::RDONLY, None::<&CStr>)
        .context("mounting /sys")?;
    populate_dev(&root.join("dev")).context("populating /dev")?;
    for file in ETC_FILES {
        let target = root.join("etc").join(file);
        // A link here would be followed outside the image root; skip it.
        if fs::symlink_metadata(&target).is_ok_and(|m| m.is_file()) {
            mount_bind(Path::new(RUN_ETC).join(file), &target)
                .with_context(|| format!("binding /etc/{file}"))?;
        }
    }
    chroot(root)?;
    std::env::set_current_dir("/")?;
    rustix::process::umask(Mode::from_raw_mode(0o022));
    fs::create_dir_all(&spec.workdir)
        .with_context(|| format!("creating workdir {}", spec.workdir))?;
    std::env::set_current_dir(&spec.workdir)?;
    let groups: Vec<Gid> = spec.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    set_thread_groups(&groups)?;
    let gid = Gid::from_raw(spec.gid);
    set_thread_res_gid(gid, gid, gid)?;
    let uid = Uid::from_raw(spec.uid);
    set_thread_res_uid(uid, uid, uid)?;
    Ok(())
}

/// Docker's container `/dev`: a tmpfs with the standard character devices,
/// a private devpts and `/dev/shm`; never the VM's own devices.
fn populate_dev(dev: &Path) -> Result<()> {
    mount("tmpfs", dev, "tmpfs", MountFlags::NOSUID | MountFlags::NOEXEC, Some(c"mode=755,size=65536k"))?;
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let path = dev.join(name);
        mknodat(CWD, &path, FileType::CharacterDevice, Mode::from_raw_mode(0o666), makedev(major, minor))?;
        rustix::fs::chmod(&path, Mode::from_raw_mode(0o666))?;
    }
    fs::create_dir(dev.join("pts"))?;
    mount(
        "devpts",
        dev.join("pts"),
        "devpts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(c"newinstance,ptmxmode=0666,mode=0620"),
    )?;
    std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir(dev.join("shm"))?;
    mount(
        "shm",
        dev.join("shm"),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        Some(c"mode=1777,size=65536k"),
    )?;
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        std::os::unix::fs::symlink(target, dev.join(name))?;
    }
    Ok(())
}
```

The two `unsafe` blocks are the only `unsafe` in the guest (Global Constraints). rustix 1.1.5's `mount` takes its data as `Option<Data>`: pass `Some(c"…")` for options and `None::<&CStr>` for none, as `probe.rs` already does.

- [ ] **Step 4: Wire it up**

In `linux.rs`, `main` becomes:

```rust
pub fn main() -> ! {
    let mut args = std::env::args_os().skip(1);
    if let Some(arg) = args.next()
        && arg == crate::run::EXEC_ARG
    {
        crate::run::exec_main(args.next())
    }
    match job_main() {
        Ok(code) => process::exit(code),
        Err(e) => {
            // No status file: the host reports a setup failure.
            eprintln!("sandcastle-guest: {e:#}");
            process::exit(125)
        }
    }
}
```

The `Job::Run` arm becomes `Job::Run(job) => with_store(|store| crate::run::run_job(store, &job, out)),`. Add `#[cfg(target_os = "linux")] mod run;` to `main.rs`.

- [ ] **Step 5: Run the checks**

Run:
```bash
cargo clippy -p sandcastle-guest --target aarch64-unknown-linux-musl -- -D warnings
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
just it
```
Expected: everything PASSES. If the busybox image's `nobody` home differs from `/home`, read it from the image's `/etc/passwd` and fix the test's expected value. Do not change `user.rs`.

- [ ] **Step 6: Commit**

```bash
git add crates/sandcastle-guest tests/vm.rs
git commit -m "Run RUN steps in their own PID and mount namespaces

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Host Dockerfile adapter (v1 scope)

**Files:**
- Create: `src/dockerfile.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (add `sandcastle-dockerfile = { path = "crates/sandcastle-dockerfile" }`)

**Interfaces:**
- Consumes: `sandcastle_dockerfile::{parse, Instruction, Node}`
- Produces:
  - `dockerfile::Step { instruction: Instruction, text: String, line: usize }`
  - `dockerfile::Recipe { base: String, escape: char, steps: Vec<Step> }`
  - `dockerfile::check(src: &str) -> Result<Recipe>`
  - `dockerfile::load(path: &Path) -> Result<Recipe>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        format!("{:#}", check(src).unwrap_err())
    }

    #[test]
    fn accepts_every_v1_instruction_in_any_case() {
        let recipe = check(
            "# syntax=docker/dockerfile:1\nFROM alpine:3.20\nenv A=1\nWORKDIR /app\nCOPY a b/\nrun echo hi\nUSER 1000\nLABEL x=y\nEXPOSE 80\nCMD [\"sh\"]\nENTRYPOINT [\"/bin/sh\",\"-c\"]\n",
        )
        .unwrap();
        assert_eq!(recipe.base, "alpine:3.20");
        assert_eq!(recipe.escape, '\\');
        assert_eq!(recipe.steps.len(), 9);
        assert_eq!(recipe.steps[3].text, "run echo hi");
        assert_eq!(recipe.steps[3].line, 6);
    }

    #[test]
    fn rejects_out_of_scope_instructions_with_lines() {
        assert_eq!(err("FROM a\nARG X\n"), "line 2: ARG is not supported yet");
        assert_eq!(err("FROM a\nonbuild RUN x\n"), "line 2: ONBUILD is not supported yet");
        assert_eq!(err("FROM a\nRUN --mount=type=cache,target=/x true\n"), "line 2: RUN --mount is not supported yet");
        assert_eq!(err("FROM a\nCOPY --from=b x y\n"), "line 2: COPY --from is not supported yet");
        assert_eq!(err("FROM a AS b\n"), "line 1: multi-stage builds (FROM … AS) are not supported yet");
        assert_eq!(err("FROM --platform=linux/amd64 a\n"), "line 1: FROM --platform is not supported yet");
        assert_eq!(err("FROM a\nFROM b\n"), "line 2: multi-stage builds (a second FROM) are not supported yet");
        assert_eq!(err("FROM scratch\n"), "line 1: FROM scratch is not supported yet");
    }

    #[test]
    fn first_instruction_must_be_from() {
        assert_eq!(err("ENV A=1\nFROM a\n"), "line 1: the first instruction must be FROM");
        assert_eq!(err("# only a comment\n"), "the Dockerfile has no instructions");
    }

    #[test]
    fn parse_and_instruction_errors_keep_their_line() {
        assert!(err("FROM a\nENV\n").starts_with("line 2:"), "{}", err("FROM a\nENV\n"));
        assert!(err("FROM a\nBOGUS x\n").starts_with("line 2:"), "{}", err("FROM a\nBOGUS x\n"));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle --lib dockerfile`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
//! Reads a Dockerfile and checks it against what sandcastle can build
//! today, so unsupported instructions fail before anything is pulled.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sandcastle_dockerfile::{Instruction, Node, parse};

/// One instruction after `FROM`.
#[derive(Debug, Clone)]
pub struct Step {
    pub instruction: Instruction,
    /// The instruction as written, continuations joined (history, messages).
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub base: String,
    pub escape: char,
    pub steps: Vec<Step>,
}

pub fn load(path: &Path) -> Result<Recipe> {
    let src = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    check(&src).with_context(|| path.display().to_string())
}

pub fn check(src: &str) -> Result<Recipe> {
    let dockerfile = parse(src)?;
    let mut nodes = dockerfile.nodes.iter();
    let first = nodes.next().context("the Dockerfile has no instructions")?;
    let line = first.start_line;
    let base = match Instruction::try_from(first)? {
        Instruction::From { stage: Some(_), .. } => {
            bail!("line {line}: multi-stage builds (FROM … AS) are not supported yet")
        }
        Instruction::From { flags, .. } if !flags.is_empty() => {
            bail!("line {line}: FROM --{} is not supported yet", flags[0].name)
        }
        Instruction::From { image, .. } if image.eq_ignore_ascii_case("scratch") => {
            bail!("line {line}: FROM scratch is not supported yet")
        }
        Instruction::From { image, .. } => image,
        _ => bail!("line {line}: the first instruction must be FROM"),
    };
    let steps = nodes
        .map(|node| {
            let instruction = Instruction::try_from(node)?;
            supported(&instruction, node)?;
            Ok(Step {
                instruction,
                text: node.original.clone(),
                line: node.start_line,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Recipe {
        base,
        escape: dockerfile.escape,
        steps,
    })
}

fn supported(instruction: &Instruction, node: &Node) -> Result<()> {
    let line = node.start_line;
    match instruction {
        Instruction::From { .. } => {
            bail!("line {line}: multi-stage builds (a second FROM) are not supported yet")
        }
        Instruction::Run { flags, .. } | Instruction::Copy { flags, .. } => match flags.first() {
            Some(flag) => bail!(
                "line {line}: {} --{} is not supported yet",
                node.cmd.to_ascii_uppercase(),
                flag.name
            ),
            None => Ok(()),
        },
        Instruction::Other(other) => {
            bail!("line {line}: {} is not supported yet", other.cmd.to_ascii_uppercase())
        }
        _ => Ok(()),
    }
}
```

Add `pub mod dockerfile;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sandcastle --lib dockerfile && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dockerfile.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "Check Dockerfiles against the v1 instruction set

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Host instruction semantics and guest DNS (pure)

**Files:**
- Create: `src/build/config.rs`, `src/build/dns.rs`, `src/build/mod.rs` (module declarations only in this task)
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `dockerfile::Step` (Task 8), `image::ConfigState`, `sandcastle_dockerfile::{expand, Command, Env}`
- Produces (used by Task 10):
  - `build::config::Stage { pub state: ConfigState }` with:
    - `new(state: ConfigState, escape: char) -> Stage`
    - `apply(&mut self, step: &Step) -> Result<Action>`
    - `run_env(&self) -> Vec<String>`
    - `user(&self) -> String`
    - `workdir(&self) -> String`
  - `build::config::Action { Metadata, Run { argv: Vec<String> }, Copy { sources: Vec<String>, dest: String } }`
  - `build::config::DEFAULT_PATH`
  - `build::dns::resolv_conf() -> String` and `build::dns::guest_resolv_conf(host: Option<&str>, systemd: Option<&str>) -> String`

**Semantics (BuildKit's dispatchers):**
- `ENV`/`LABEL`: keys and values are expanded against the env *before* the instruction. A later key replaces an earlier one.
- `WORKDIR`: expanded. A relative path joins the previous workdir (default `/`). The result is cleaned lexically.
- `USER`: expanded.
- `EXPOSE`: each word is expanded and normalised to `<port>/<proto>`. Proto defaults to `tcp` and is lower-cased; it must be tcp, udp or sctp. A range `a-b` expands to every port.
- `CMD`/`ENTRYPOINT`: shell form becomes `["/bin/sh","-c",cmd]`. `ENTRYPOINT` clears an inherited `CMD` unless this Dockerfile already set `CMD`.
- `RUN`: never expanded. Same argv rule as `CMD`.
- `COPY`: each source and the dest are expanded. More than one source needs a dest ending in `/`.
- RUN env is the image env plus `PATH=` the default PATH if the image has none.

- [ ] **Step 1: Write the failing tests**

`src/build/config.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use oci_spec::image::{Arch, ImageConfigurationBuilder, Os, RootFsBuilder};

    use super::*;
    use crate::dockerfile::check;

    fn stage() -> Stage {
        let config = ImageConfigurationBuilder::default()
            .architecture(Arch::ARM64)
            .os(Os::Linux)
            .config(
                oci_spec::image::ConfigBuilder::default()
                    .env(vec!["PATH=/base/bin".to_string(), "HOME=/root".to_string()])
                    .cmd(vec!["sh".to_string()])
                    .build()
                    .unwrap(),
            )
            .rootfs(RootFsBuilder::default().typ("layers").diff_ids(Vec::<String>::new()).build().unwrap())
            .build()
            .unwrap();
        Stage::new(ConfigState::from_base(&config, vec![]).unwrap(), '\\')
    }

    /// Applies every step of `body` (after a FROM line) and returns the actions.
    fn apply(stage: &mut Stage, body: &str) -> Result<Vec<Action>> {
        let recipe = check(&format!("FROM base\n{body}")).unwrap();
        recipe.steps.iter().map(|s| stage.apply(s)).collect()
    }

    #[test]
    fn env_expands_against_the_env_before_the_instruction() {
        let mut s = stage();
        apply(&mut s, "ENV A=1\nENV A=2 B=$A PATH=$PATH:/x\n").unwrap();
        assert!(s.state.env.contains(&"A=2".to_string()));
        assert!(s.state.env.contains(&"B=1".to_string()));
        assert!(s.state.env.contains(&"PATH=/base/bin:/x".to_string()));
        assert_eq!(s.state.env.iter().filter(|e| e.starts_with("A=")).count(), 1);
    }

    #[test]
    fn workdir_joins_and_cleans() {
        let mut s = stage();
        apply(&mut s, "ENV D=app\nWORKDIR /srv\nWORKDIR $D/../web/./\n").unwrap();
        assert_eq!(s.workdir(), "/srv/web");
        apply(&mut s, "WORKDIR /abs\n").unwrap();
        assert_eq!(s.workdir(), "/abs");
    }

    #[test]
    fn expose_normalises_ports_and_ranges() {
        let mut s = stage();
        apply(&mut s, "ENV P=8080\nEXPOSE $P 53/UDP 7000-7002/tcp\n").unwrap();
        let ports: Vec<&str> = s.state.exposed_ports.iter().map(String::as_str).collect();
        assert_eq!(ports, ["53/udp", "7000/tcp", "7001/tcp", "7002/tcp", "8080/tcp"]);
        let err = apply(&mut s, "EXPOSE 80/quic\n").unwrap_err();
        assert!(format!("{err:#}").contains("line 2: invalid port 80/quic"), "{err:#}");
    }

    #[test]
    fn cmd_entrypoint_and_run_argv() {
        let mut s = stage();
        let actions = apply(&mut s, "ENTRYPOINT [\"/entry\"]\nRUN echo $HOME\nRUN [\"a\", \"b\"]\n").unwrap();
        assert_eq!(s.state.cmd, None, "inherited CMD is cleared by ENTRYPOINT");
        assert_eq!(actions[1], Action::Run { argv: vec!["/bin/sh".into(), "-c".into(), "echo $HOME".into()] });
        assert_eq!(actions[2], Action::Run { argv: vec!["a".into(), "b".into()] });

        let mut s = stage();
        apply(&mut s, "CMD echo hi\nENTRYPOINT [\"/entry\"]\n").unwrap();
        assert_eq!(s.state.cmd, Some(vec!["/bin/sh".into(), "-c".into(), "echo hi".into()]));
    }

    #[test]
    fn copy_expands_and_checks_multi_source_dest() {
        let mut s = stage();
        let actions = apply(&mut s, "ENV F=a.txt\nCOPY $F /dst\n").unwrap();
        assert_eq!(actions[1], Action::Copy { sources: vec!["a.txt".into()], dest: "/dst".into() });
        let err = apply(&mut s, "COPY a b /dst\n").unwrap_err();
        assert!(format!("{err:#}").contains("must be a directory and end with a /"), "{err:#}");
    }

    #[test]
    fn user_label_and_run_env() {
        let mut s = stage();
        apply(&mut s, "ENV U=app\nUSER $U:grp\nLABEL \"k $U\"=\"v $U\"\n").unwrap();
        assert_eq!(s.user(), "app:grp");
        assert_eq!(s.state.labels.get("k app").map(String::as_str), Some("v app"));
        let mut bare = stage();
        bare.state.env.retain(|e| !e.starts_with("PATH="));
        assert!(bare.run_env().contains(&format!("PATH={DEFAULT_PATH}")));
    }

    #[test]
    fn expansion_errors_name_the_line() {
        let mut s = stage();
        let err = apply(&mut s, "ENV A=1\nWORKDIR ${MISSING:?must be set}\n").unwrap_err();
        assert!(format!("{err:#}").starts_with("line 3:"), "{err:#}");
    }
}
```

`src/build/dns.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_ipv6_servers_are_dropped() {
        let got = guest_resolv_conf(
            Some("search corp.example\nnameserver 127.0.0.1\nnameserver fe80::1%en0\nnameserver 10.0.0.2\noptions ndots:2\n"),
            None,
        );
        assert_eq!(got, "search corp.example\nnameserver 10.0.0.2\noptions ndots:2\n");
    }

    #[test]
    fn systemd_stub_is_replaced_by_upstream_servers() {
        let got = guest_resolv_conf(
            Some("nameserver 127.0.0.53\nsearch lan\n"),
            Some("nameserver 192.168.1.1\nsearch lan\n"),
        );
        assert_eq!(got, "nameserver 192.168.1.1\nsearch lan\n");
    }

    #[test]
    fn falls_back_to_public_servers() {
        assert_eq!(guest_resolv_conf(None, None), "nameserver 8.8.8.8\nnameserver 8.8.4.4\n");
        assert_eq!(
            guest_resolv_conf(Some("nameserver 127.0.0.53\n"), None),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle --lib build::`
Expected: compile errors.

- [ ] **Step 3: Implement `config.rs`**

```rust
//! Applies Dockerfile instructions to the image config with Docker's
//! variable expansion rules, and turns RUN/COPY into job inputs.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use sandcastle_dockerfile::{Command, Instruction, expand};

use crate::dockerfile::Step;
use crate::image::ConfigState;

/// PATH for RUN when the image sets none (BuildKit's default).
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const PROTOCOLS: [&str; 3] = ["tcp", "udp", "sctp"];

/// The guest work a step needs, after expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Metadata,
    Run { argv: Vec<String> },
    Copy { sources: Vec<String>, dest: String },
}

pub struct Stage {
    pub state: ConfigState,
    escape: char,
    /// CMD was set in this Dockerfile, so ENTRYPOINT keeps it.
    cmd_set: bool,
}

impl Stage {
    pub fn new(state: ConfigState, escape: char) -> Self {
        Self { state, escape, cmd_set: false }
    }

    pub fn apply(&mut self, step: &Step) -> Result<Action> {
        self.apply_inner(step)
            .with_context(|| format!("line {}", step.line))
    }

    fn apply_inner(&mut self, step: &Step) -> Result<Action> {
        let env = self.env_map();
        let escape = self.escape;
        let x = |word: &str| expand(word, &env, escape).map_err(anyhow::Error::from);
        match &step.instruction {
            Instruction::Env(pairs) => {
                for kv in pairs {
                    let (key, value) = (x(&kv.key)?, x(&kv.value)?);
                    set_env(&mut self.state.env, &key, &value);
                }
            }
            Instruction::Label(pairs) => {
                for kv in pairs {
                    let (key, value) = (x(&kv.key)?, x(&kv.value)?);
                    self.state.labels.insert(key, value);
                }
            }
            Instruction::Workdir(dir) => {
                let dir = x(dir)?;
                self.state.working_dir = Some(join_workdir(self.state.working_dir.as_deref(), &dir));
            }
            Instruction::User(user) => self.state.user = Some(x(user)?),
            Instruction::Expose(ports) => {
                for port in ports {
                    self.state.exposed_ports.extend(parse_ports(&x(port)?)?);
                }
            }
            Instruction::Cmd(command) => {
                self.state.cmd = Some(argv(command));
                self.cmd_set = true;
            }
            Instruction::Entrypoint(command) => {
                self.state.entrypoint = Some(argv(command));
                if !self.cmd_set {
                    self.state.cmd = None;
                }
            }
            Instruction::Run { command, .. } => return Ok(Action::Run { argv: argv(command) }),
            Instruction::Copy { sources, dest, .. } => {
                let sources = sources.iter().map(|s| x(s)).collect::<Result<Vec<_>>>()?;
                let dest = x(dest)?;
                ensure!(
                    sources.len() == 1 || dest.ends_with('/'),
                    "When using COPY with more than one source file, the destination must be a directory and end with a /"
                );
                return Ok(Action::Copy { sources, dest });
            }
            Instruction::From { .. } | Instruction::Other(_) => {
                bail!("instruction is not supported here")
            }
        }
        Ok(Action::Metadata)
    }

    fn env_map(&self) -> HashMap<String, String> {
        self.state
            .env
            .iter()
            .filter_map(|e| e.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    pub fn run_env(&self) -> Vec<String> {
        let mut env = self.state.env.clone();
        if !env.iter().any(|e| e.starts_with("PATH=")) {
            env.push(format!("PATH={DEFAULT_PATH}"));
        }
        env
    }

    pub fn user(&self) -> String {
        self.state.user.clone().unwrap_or_default()
    }

    pub fn workdir(&self) -> String {
        self.state.working_dir.clone().unwrap_or_else(|| "/".into())
    }
}

fn argv(command: &Command) -> Vec<String> {
    match command {
        Command::Shell(cmd) => vec!["/bin/sh".into(), "-c".into(), cmd.clone()],
        Command::Exec(args) => args.clone(),
    }
}

fn set_env(env: &mut Vec<String>, key: &str, value: &str) {
    let entry = format!("{key}={value}");
    match env.iter_mut().find(|e| e.split_once('=').is_some_and(|(k, _)| k == key)) {
        Some(existing) => *existing = entry,
        None => env.push(entry),
    }
}

fn join_workdir(current: Option<&str>, dir: &str) -> String {
    if dir.starts_with('/') {
        clean(dir)
    } else {
        clean(&format!("{}/{dir}", current.unwrap_or("/")))
    }
}

/// Lexical clean of an absolute path, like Go's `path.Clean`.
fn clean(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

fn parse_ports(spec: &str) -> Result<Vec<String>> {
    let invalid = || anyhow::anyhow!("invalid port {spec}");
    let (ports, proto) = match spec.split_once('/') {
        Some((p, proto)) => (p, proto.to_ascii_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    ensure!(PROTOCOLS.contains(&proto.as_str()), invalid());
    let (start, end) = match ports.split_once('-') {
        Some((a, b)) => (a, b),
        None => (ports, ports),
    };
    let start: u16 = start.parse().map_err(|_| invalid())?;
    let end: u16 = end.parse().map_err(|_| invalid())?;
    ensure!(start <= end, invalid());
    Ok((start..=end).map(|p| format!("{p}/{proto}")).collect())
}
```

`ConfigState.exposed_ports` is a `BTreeSet<String>`. Sorting is lexical, which the test above accounts for.

- [ ] **Step 4: Implement `dns.rs` and module wiring**

```rust
//! The `/etc/resolv.conf` a RUN step sees. The guest's sockets are proxied
//! by the host (TSI), but loopback resolvers on the host are not reachable
//! from the guest's own loopback, so they are dropped as Docker does.

use std::fs;
use std::net::Ipv4Addr;

const HOST_RESOLV_CONF: &str = "/etc/resolv.conf";
/// The upstream servers behind systemd-resolved's 127.0.0.53 stub.
const SYSTEMD_RESOLV_CONF: &str = "/run/systemd/resolve/resolv.conf";
const FALLBACK: &str = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";

pub fn resolv_conf() -> String {
    guest_resolv_conf(
        fs::read_to_string(HOST_RESOLV_CONF).ok().as_deref(),
        fs::read_to_string(SYSTEMD_RESOLV_CONF).ok().as_deref(),
    )
}

pub fn guest_resolv_conf(host: Option<&str>, systemd: Option<&str>) -> String {
    let host = host.unwrap_or("");
    let servers: Vec<Ipv4Addr> = nameservers(host).collect();
    let source = if !servers.is_empty() && servers.iter().all(Ipv4Addr::is_loopback) {
        systemd.unwrap_or(host)
    } else {
        host
    };
    let mut out = String::new();
    let mut found = false;
    for line in source.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(ip) = words.next().and_then(|w| w.parse::<Ipv4Addr>().ok())
                    && !ip.is_loopback()
                {
                    out.push_str(line.trim());
                    out.push('\n');
                    found = true;
                }
            }
            Some("search" | "domain" | "options") => {
                out.push_str(line.trim());
                out.push('\n');
            }
            _ => {}
        }
    }
    if !found {
        out.push_str(FALLBACK);
    }
    out
}

fn nameservers(conf: &str) -> impl Iterator<Item = Ipv4Addr> + '_ {
    conf.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("nameserver"))
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}
```

Check the fallback test: `"nameserver 127.0.0.53\n"` with no systemd file. Every server is loopback and `systemd` is `None`, so `source` stays `host`. Every line is dropped, so the output is the fallback. That matches the test.

`src/build/mod.rs` for this task:

```rust
//! `sandcastle build`.

pub mod config;
pub mod dns;
```

Add `pub mod build;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p sandcastle --lib build:: && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/build src/lib.rs
git commit -m "Apply Dockerfile instructions to the image config

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: `sandcastle build`

**Files:**
- Modify: `src/build/mod.rs`, `src/main.rs`
- Modify: `tests/cli.rs`

**Interfaces:**
- Consumes:
  - `dockerfile::load` (Task 8)
  - `Stage`, `Action`, `dns::resolv_conf` (Task 9)
  - `Vm`, `Resources`, `Finished`, `open_guest_file` (Task 5)
  - `registry::pull`, `image::{ConfigState, LayerWriter, write_layout}`
- Produces:
  - `build::Options { dockerfile, context, output, tag, resources }`
  - `build::build(exe: &Path, opts: &Options) -> Result<Descriptor>`
  - CLI `sandcastle build [-f FILE] -t TAG -o DIR [--cpus N] [--memory MIB] CONTEXT`

- [ ] **Step 1: Write the failing CLI tests (no VM needed)**

Add to `tests/cli.rs`:

```rust
fn sandcastle() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sandcastle"))
}

#[test]
fn build_rejects_unsupported_instruction_before_pulling() {
    let ctx = tempfile::tempdir().unwrap();
    std::fs::write(ctx.path().join("Dockerfile"), "FROM alpine\nARG VERSION\n").unwrap();
    let output = sandcastle()
        .args(["build", "-t", "demo", "-o"])
        .arg(ctx.path().join("out"))
        .arg(ctx.path())
        .env("SANDCASTLE_LIBKRUN_DIR", "/nonexistent")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2: ARG is not supported yet"), "{stderr}");
}

#[test]
fn build_reports_missing_dockerfile() {
    let ctx = tempfile::tempdir().unwrap();
    let output = sandcastle()
        .args(["build", "-t", "demo", "-o", "out", "-f"])
        .arg(ctx.path().join("Nope"))
        .arg(ctx.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Nope"));
}
```

These tests are not `#[ignore]`: they must pass under plain `cargo test`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle --test cli`
Expected: FAIL (no `build` subcommand).

- [ ] **Step 3: Implement the orchestration**

`src/build/mod.rs`:

```rust
//! `sandcastle build`: pull the base, apply each instruction, run RUN and
//! COPY in microVMs, and write the image as an OCI layout.

pub mod config;
pub mod dns;

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use oci_spec::image::Descriptor;
use sandcastle_proto::{CopyJob, Job, LAYER_FILE, RunJob};

use crate::blobs::BlobStore;
use crate::dockerfile::{self, Step};
use crate::image::{self, ConfigState, Layer, LayerWriter};
use crate::install::{self, Install};
use crate::registry;
use crate::store::Store;
use crate::vm::{self, Resources, Vm};
use config::{Action, Stage};

/// Longest instruction text shown in progress and error messages.
const SHOWN_CHARS: usize = 60;

pub struct Options {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    pub output: PathBuf,
    pub tag: String,
    pub resources: Resources,
}

/// Builds the image and returns its manifest descriptor in the layout.
pub fn build(exe: &Path, opts: &Options) -> Result<Descriptor> {
    // Unsupported instructions fail here, before any download or VM.
    let recipe = dockerfile::load(&opts.dockerfile)?;
    let context = std::fs::canonicalize(&opts.context)
        .with_context(|| format!("build context {}", opts.context.display()))?;
    ensure!(context.is_dir(), "build context {} is not a directory", context.display());
    let install = Install::locate(exe)?;
    #[cfg(target_os = "linux")]
    crate::doctor::check_kvm(Path::new("/dev/kvm"))?;
    let store = Store::open(&install::store_root()?, &install)?;
    let blobs = store.blobs();

    let total = recipe.steps.len() + 1;
    eprintln!("[1/{total}] FROM {}", recipe.base);
    let image = registry::pull(&blobs, &recipe.base)?;
    let mut stage = Stage::new(ConfigState::from_base(&image.config, image.layers)?, recipe.escape);
    let vm = Vm { exe, install: &install, store: &store, resources: opts.resources };
    let resolv_conf = dns::resolv_conf();
    for (i, step) in recipe.steps.iter().enumerate() {
        let label = format!("step {}/{total} {}", i + 2, shown(&step.text));
        eprintln!("[{}/{total}] {}", i + 2, shown(&step.text));
        run_step(&vm, &blobs, &mut stage, step, &context, &resolv_conf).context(label)?;
    }
    image::write_layout(&blobs, &stage.state, &opts.output, &opts.tag)
}

fn run_step(
    vm: &Vm,
    blobs: &BlobStore<'_>,
    stage: &mut Stage,
    step: &Step,
    context: &Path,
    resolv_conf: &str,
) -> Result<()> {
    let (job, ctx) = match stage.apply(step)? {
        Action::Metadata => return stage.state.add_empty(&step.text),
        Action::Run { argv } => (
            Job::Run(RunJob {
                lower: stage.state.lower_layers()?,
                argv,
                env: stage.run_env(),
                user: stage.user(),
                workdir: stage.workdir(),
                resolv_conf: resolv_conf.to_string(),
            }),
            None,
        ),
        Action::Copy { sources, dest } => (
            Job::Copy(CopyJob {
                lower: stage.state.lower_layers()?,
                sources,
                dest,
                workdir: stage.workdir(),
            }),
            Some(context),
        ),
    };
    let finished = vm.run(&job, ctx)?;
    let status = &finished.status;
    ensure!(status.exit_code == 0, "exited with {}", status.exit_code);
    match &status.layer {
        None => stage.state.add_empty(&step.text),
        Some(diff_id) => {
            let layer = ingest(blobs, &finished.out_dir().join(LAYER_FILE), diff_id)?;
            stage.state.add_layer(layer, &step.text)
        }
    }
}

/// Gzips the guest's layer tar into the blob store and checks that the
/// host and the guest agree on its diff_id.
fn ingest(blobs: &BlobStore<'_>, path: &Path, diff_id: &str) -> Result<Layer> {
    let mut file = vm::open_guest_file(path)?
        .context("the guest reported a layer but wrote no layer.tar")?;
    let mut writer = LayerWriter::new(blobs)?;
    io::copy(&mut file, &mut writer).context("compressing the layer")?;
    let layer = writer.finish()?;
    ensure!(
        layer.diff_id.to_string() == diff_id,
        "layer digest mismatch: the guest reported {diff_id}, the host computed {}",
        layer.diff_id
    );
    Ok(layer)
}

fn shown(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > SHOWN_CHARS {
        format!("{}…", line.chars().take(SHOWN_CHARS).collect::<String>())
    } else {
        line.to_string()
    }
}
```

- [ ] **Step 4: CLI**

Add to the `Command` enum in `src/main.rs`:

```rust
    /// Build a Dockerfile into an OCI image layout directory.
    Build {
        /// Build context directory.
        context: PathBuf,
        /// Dockerfile path [default: <context>/Dockerfile].
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Name recorded in the layout index (use as `oci:<output>:<tag>`).
        #[arg(short, long)]
        tag: String,
        /// Output OCI layout directory (created or updated in place).
        #[arg(short, long)]
        output: PathBuf,
        /// vCPUs per build-step VM [default: all host CPUs].
        #[arg(long)]
        cpus: Option<u8>,
        /// Memory per build-step VM, in MiB.
        #[arg(long, default_value_t = 2048)]
        memory: u32,
    },
```

The match arm:

```rust
        Command::Build { context, file, tag, output, cpus, memory } => {
            build(context, file, tag, output, cpus, memory)
        }
```

and the function:

```rust
fn build(
    context: PathBuf,
    file: Option<PathBuf>,
    tag: String,
    output: PathBuf,
    cpus: Option<u8>,
    memory: u32,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut resources = Resources::default();
    if let Some(cpus) = cpus {
        resources.vcpus = cpus;
    }
    resources.ram_mib = memory;
    let opts = sandcastle::build::Options {
        dockerfile: file.unwrap_or_else(|| context.join("Dockerfile")),
        context,
        output,
        tag,
        resources,
    };
    let manifest = sandcastle::build::build(&exe, &opts)?;
    println!("{} {}:{}", manifest.digest(), opts.output.display(), opts.tag);
    Ok(())
}
```

Import `sandcastle::vm::Resources`.

- [ ] **Step 5: Run the tests**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS, including the two new CLI tests.

- [ ] **Step 6: Commit**

```bash
git add src tests/cli.rs
git commit -m "Add sandcastle build

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: End-to-end build tests, podman check, CI

**Files:**
- Create: `tests/build.rs`, `tests/podman.rs`
- Create: `tests/fixtures/build/alpine/{Dockerfile,hello.txt,one.conf,two.conf,tree/a.txt,tree/sub/b.txt,tree/run.sh}`
- Create: `tests/fixtures/build/debian/Dockerfile`
- Modify: `justfile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the `sandcastle build` CLI (Task 10). The tests inspect only the OCI layout on disk (`oci-spec`, `tar`, `flate2`, `sha2`), never sandcastle's own types.

- [ ] **Step 1: Fixtures**

`tests/fixtures/build/alpine/Dockerfile`:

```dockerfile
FROM mirror.gcr.io/library/alpine:3.20
ENV GREETING="hello world" APP=/app
WORKDIR $APP
COPY hello.txt ./
COPY tree/ data/
COPY *.conf /etc/demo/
RUN rm /etc/motd && adduser -D -u 1234 builder && echo "$GREETING" > greeting && ln -s greeting link
RUN apk add --no-cache tini
USER builder
RUN id -u > /tmp/uid
LABEL org.example.demo="yes"
EXPOSE 8080 53/udp
ENTRYPOINT ["/bin/sh", "-c"]
CMD ["cat /app/greeting"]
```

Context files:
- `hello.txt`: `hello from the context`
- `one.conf`: `1`
- `two.conf`: `2`
- `tree/a.txt`: `a`
- `tree/sub/b.txt`: `b`
- `tree/run.sh`: `#!/bin/sh` with mode 0755 (`git update-index --chmod=+x`)

Each file ends with a newline.

`tests/fixtures/build/debian/Dockerfile`:

```dockerfile
FROM mirror.gcr.io/library/debian:bookworm-slim
RUN groupadd -g 2000 grp && useradd -u 1500 -G grp app && rm /etc/debian_version
USER app
WORKDIR /home/app
RUN id > /tmp/id && pwd > /tmp/pwd && echo "$HOME" > /tmp/home
```

- [ ] **Step 2: Write the tests**

`tests/build.rs`:

```rust
//! End-to-end `sandcastle build` tests. Run with `just it` (needs lib/, a
//! hypervisor and network). They inspect only the OCI layout on disk.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::read::GzDecoder;
use oci_spec::image::{ImageConfiguration, ImageIndex, ImageManifest};
use sha2::{Digest, Sha256};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build").join(name)
}

/// Runs `sandcastle build` with a store shared by all build tests, so base
/// images are pulled once per `just it` run.
fn build(context: &Path, dockerfile: Option<&Path>, out: &Path) -> Output {
    let bin = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let store = bin.parent().unwrap().join("store");
    let mut cmd = Command::new(&bin);
    cmd.args(["build", "-t", "demo", "-o"]).arg(out);
    if let Some(f) = dockerfile {
        cmd.arg("-f").arg(f);
    }
    let output = cmd.arg(context).env("SANDCASTLE_ROOT", store).output().unwrap();
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    output
}

struct Layout {
    dir: PathBuf,
    manifest: ImageManifest,
    config: ImageConfiguration,
}

impl Layout {
    fn open(dir: &Path) -> Self {
        let index: ImageIndex = serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let desc = &index.manifests()[0];
        assert_eq!(
            desc.annotations().as_ref().unwrap()["org.opencontainers.image.ref.name"],
            "demo"
        );
        let manifest: ImageManifest = serde_json::from_slice(&blob(dir, desc.digest().digest())).unwrap();
        let config: ImageConfiguration =
            serde_json::from_slice(&blob(dir, manifest.config().digest().digest())).unwrap();
        Layout { dir: dir.to_path_buf(), manifest, config }
    }

    /// Entries of layer `i`: path → (entry type, uid, contents or link target).
    fn layer(&self, i: usize) -> BTreeMap<String, (tar::EntryType, u64, Vec<u8>)> {
        let gz = blob(&self.dir, self.manifest.layers()[i].digest().digest());
        let mut tar_bytes = Vec::new();
        GzDecoder::new(&gz[..]).read_to_end(&mut tar_bytes).unwrap();
        let mut map = BTreeMap::new();
        for entry in tar::Archive::new(&tar_bytes[..]).entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let (kind, uid) = (entry.header().entry_type(), entry.header().uid().unwrap());
            let body = match entry.link_name().unwrap() {
                Some(t) => t.to_string_lossy().into_owned().into_bytes(),
                None => {
                    let mut b = Vec::new();
                    entry.read_to_end(&mut b).unwrap();
                    b
                }
            };
            map.insert(path, (kind, uid, body));
        }
        map
    }
}

fn blob(dir: &Path, hex: &str) -> Vec<u8> {
    std::fs::read(dir.join("blobs/sha256").join(hex)).unwrap()
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn alpine_build_produces_expected_image() {
    let out = tempfile::tempdir().unwrap();
    let output = build(&fixture("alpine"), None, out.path());
    assert!(output.status.success());
    let layout = Layout::open(out.path());
    let config = layout.config.config().as_ref().unwrap();

    assert_eq!(config.user().as_deref(), Some("builder"));
    assert_eq!(config.working_dir().as_deref(), Some("/app"));
    let env = config.env().as_ref().unwrap();
    assert!(env.contains(&"GREETING=hello world".to_string()), "{env:?}");
    assert!(env.iter().any(|e| e.starts_with("PATH=")), "base PATH kept: {env:?}");
    assert_eq!(config.labels().as_ref().unwrap()["org.example.demo"], "yes");
    let mut ports = config.exposed_ports().clone().unwrap();
    ports.sort();
    assert_eq!(ports, ["53/udp", "8080/tcp"]);
    assert_eq!(config.entrypoint().as_deref(), Some(&["/bin/sh".to_string(), "-c".to_string()][..]));
    assert_eq!(config.cmd().as_deref(), Some(&["cat /app/greeting".to_string()][..]));

    // Every layer digest and diff_id recomputed from the bytes on disk.
    let diff_ids = layout.config.rootfs().diff_ids();
    assert_eq!(diff_ids.len(), layout.manifest.layers().len());
    for (desc, diff_id) in layout.manifest.layers().iter().zip(diff_ids) {
        let gz = blob(out.path(), desc.digest().digest());
        assert_eq!(hex_digest(&gz), desc.digest().to_string());
        let mut raw = Vec::new();
        GzDecoder::new(&gz[..]).read_to_end(&mut raw).unwrap();
        assert_eq!(&hex_digest(&raw), diff_id);
    }

    // 3 COPY + 3 RUN layers on top of the base; metadata steps add none.
    let base = layout.manifest.layers().len() - 6;
    let history = layout.config.history().as_ref().unwrap();
    assert_eq!(history.iter().filter(|h| !h.empty_layer().unwrap_or(false)).count(), base + 6);

    let hello = layout.layer(base);
    assert_eq!(hello["app/hello.txt"], (tar::EntryType::Regular, 0, b"hello from the context\n".to_vec()));
    let tree = layout.layer(base + 1);
    assert_eq!(tree["app/data/sub/b.txt"].2, b"b\n");
    let confs = layout.layer(base + 2);
    assert!(confs.contains_key("etc/demo/one.conf") && confs.contains_key("etc/demo/two.conf"));
    let run = layout.layer(base + 3);
    assert!(run.contains_key("etc/.wh.motd"), "{:?}", run.keys());
    assert_eq!(run["app/greeting"].2, b"hello world\n");
    assert_eq!(run["app/link"], (tar::EntryType::Symlink, 0, b"greeting".to_vec()));
    assert!(layout.layer(base + 4).contains_key("sbin/tini"));
    assert_eq!(layout.layer(base + 5)["tmp/uid"].2, b"1234\n");
    for i in base..base + 6 {
        for stub in ["etc/resolv.conf", "etc/hosts", "etc/hostname", "dev", "proc", "sys"] {
            assert!(!layout.layer(i).contains_key(stub), "{stub} in layer {i}");
        }
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn debian_build_resolves_users_and_groups() {
    let out = tempfile::tempdir().unwrap();
    let output = build(&fixture("debian"), None, out.path());
    assert!(output.status.success());
    let layout = Layout::open(out.path());
    let last = layout.layer(layout.manifest.layers().len() - 1);
    assert_eq!(last["tmp/id"].2, b"uid=1500(app) gid=1500(app) groups=1500(app),2000(grp)\n");
    assert_eq!(last["tmp/pwd"].2, b"/home/app\n");
    assert_eq!(last["tmp/home"].2, b"/home/app\n");
    let first = layout.layer(layout.manifest.layers().len() - 2);
    assert!(first.contains_key("etc/.wh.debian_version"));
}

fn failing_build(body: &str) -> String {
    let ctx = tempfile::tempdir().unwrap();
    let dockerfile = ctx.path().join("Dockerfile");
    std::fs::write(&dockerfile, format!("FROM mirror.gcr.io/library/alpine:3.20\n{body}")).unwrap();
    let output = build(ctx.path(), Some(&dockerfile), &ctx.path().join("out"));
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failing_steps_report_their_exit_code() {
    let stderr = failing_build("RUN exit 3\n");
    assert!(stderr.contains("step 2/2 RUN exit 3: exited with 3"), "{stderr}");
    assert!(failing_build("RUN definitely-not-a-command\n").contains("exited with 127"));
    assert!(failing_build("RUN [\"/no/such/binary\"]\n").contains("exited with 127"));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn unknown_user_fails_step() {
    let stderr = failing_build("USER nosuchuser\nRUN true\n");
    assert!(stderr.contains("unable to find user nosuchuser"), "{stderr}");
}
```

`tests/podman.rs`:

```rust
//! Runs a built image with podman. Linux CI only: `just it-podman`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap();
    assert!(output.status.success(), "{:?}: {}", cmd, String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "needs podman, skopeo, lib/, a hypervisor and network; run with `just it-podman`"]
fn podman_runs_built_image() {
    let bin = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let out = tempfile::tempdir().unwrap();
    let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build/alpine");
    run(Command::new(&bin)
        .args(["build", "-t", "demo", "-o"])
        .arg(out.path())
        .arg(&context)
        .env("SANDCASTLE_ROOT", bin.parent().unwrap().join("store")));
    let image = "localhost/sandcastle-it:latest";
    run(Command::new("skopeo")
        .arg("copy")
        .arg(format!("oci:{}:demo", out.path().display()))
        .arg(format!("containers-storage:{image}")));
    assert_eq!(run(Command::new("podman").args(["run", "--rm", image])), "hello world\n");
    assert_eq!(run(Command::new("podman").args(["run", "--rm", image, "id -u && pwd"])), "1234\n/app\n");
    run(Command::new("podman").args(["rmi", image]));
}
```

- [ ] **Step 3: justfile and CI**

In `justfile`, change the `it` recipe's test selection to `--test vm --test cli --test build`. Add:

```just
# Run a built image with podman (Linux CI): needs podman and skopeo.
it-podman: it
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_BIN={{it_bin}} cargo test --release -p sandcastle --test podman -- --ignored --test-threads=1
```

In `.github/workflows/ci.yml` `vm-linux`, replace `- run: just it` with:

```yaml
      - name: Install podman and skopeo
        run: sudo apt-get update && sudo apt-get install -y podman skopeo
      - run: just it-podman
```

(`it-podman` depends on `it`, so the VM, CLI and build tests run first.)

- [ ] **Step 4: Run them**

Run: `just it`
Expected: every test PASSES, including `alpine_build_produces_expected_image`, `debian_build_resolves_users_and_groups`, `failing_steps_report_their_exit_code` and `unknown_user_fails_step`. `just it-podman` needs Linux with podman, so it runs only in CI. Push the branch and check that `vm-linux` is green.

Adjust expected values only where the base image dictates them, and say so in the commit. For example, alpine's tini may live at `sbin/tini` or `usr/bin/tini` depending on the release. Never relax an assertion about sandcastle's own behaviour.

- [ ] **Step 5: Commit**

```bash
git add tests justfile .github/workflows/ci.yml
git commit -m "Add end-to-end build tests and run built images with podman in CI

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
