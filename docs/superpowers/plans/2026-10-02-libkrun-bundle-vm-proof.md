# libkrun Bundle + VM Proof Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `sandcastle doctor` boots a libkrun microVM from bundled libraries on macOS arm64 and Linux, runs the static guest helper, which mounts the ext4 store disk and probes overlayfs, and reports back through a virtio-fs status file.

**Architecture:** Cargo workspace with the host binary (`sandcastle`), a shared wire-types crate (`sandcastle-proto`) and a static musl guest helper (`sandcastle-guest`). The host `dlopen`s libkrun from `lib/`, re-executes itself as a hidden `__vm` child per job (because `krun_start_enter` never returns), and treats the guest's `status.json` as the authoritative result. `scripts/build-libs.sh` builds the bundle (libkrun, libkrunfw, sparse ext4 store template) from pinned upstream tags, locally and in CI.

**Tech Stack:** Rust 2024 (rustc ≥ 1.89 for `File::try_lock`), libkrun v1.19.6 (`BLK=1`), libkrunfw v5.6.2, libloading 0.9, rustix 1.1, landlock 0.4, zstd 0.14, clap 4, serde/serde_json, anyhow, tempfile (dev), GitHub Actions, just.

**Spec:** `docs/superpowers/specs/2026-10-02-sandcastle-v1-design.md` (sections: Decisions, Architecture → VM setup, Exit code handling, Landlock, Build and platform notes).

## Global Constraints

- Upstream libkrun **stable v1.19.6** only, built with `BLK=1`; no smolvm fork APIs.
- libkrunfw **v5.6.2** release assets; macOS uses `libkrunfw-prebuilt-aarch64.tgz`.
- libkrun is loaded at runtime with `dlopen` from `lib/` next to the executable, or from `$SANDCASTLE_LIBKRUN_DIR`. No build-time linking.
- Bundled file names: Linux `libkrun.so.1`, `libkrunfw.so.5`; macOS `libkrun.1.dylib`, `libkrunfw.5.dylib`; both: `store-template.ext4.zst`, `PROVENANCE`, `SHA256SUMS`, `licenses/`.
- Store root: `$SANDCASTLE_ROOT`, default `~/.local/share/sandcastle`. Store disk: 64 GiB sparse ext4.
- Root is never required. Linux needs read-write access to `/dev/kvm` only.
- `status.json` is authoritative for exit codes; libkrun's init reserves 125/126/127.
- All `unsafe` lives in `src/vm/krun.rs`, and each `unsafe` block has a `// SAFETY:` comment.
- Guest arch = host arch: `aarch64-unknown-linux-musl` on Apple Silicon, `<arch>-unknown-linux-musl` on Linux.
- macOS binary is ad-hoc signed with `com.apple.security.hypervisor`.
- Tests: no tautological tests. VM tests are `#[ignore]` and run only via `just it`. Never run the README conformance suite.

## Review Focus

1. A second `sandcastle` started while one is running must fail at once with "another sandcastle process is using the store", not hang or corrupt the ext4 disk (Task 4 test `second_open_fails_while_locked`).
2. After a crash or Ctrl-C, leftover `jobs/*` directories must not leak into the next run (Task 4 test `open_removes_stale_job_dirs`).
3. After upgrading sandcastle, the guest helper copied into `guest-root/` must be replaced, otherwise host and guest disagree on the wire format (Task 4 test `open_refreshes_changed_guest_helper`).
4. If the VM or helper dies before writing `status.json` (libkrun setup error, signal, helper panic), the step must fail with "setup failed" even when the child exit code looks like a command result such as 127 (Task 5 test `missing_status_is_setup_failure_even_with_exit_127`).
5. A store root path containing spaces (common on macOS) must still boot, because paths reach libkrun as C strings (Task 5 integration tests use a `store root` directory).

---

## File Structure

```
Cargo.toml                         workspace + host package
.cargo/config.toml                 rust-lld linker for musl targets
justfile                           build, sign, test, it, build-libs, fetch-libs
sandcastle.entitlements            macOS hypervisor entitlement
NOTICE                             bundled components and licenses
.gitignore                         + lib/
.github/workflows/ci.yml           libs bundle, fast tests, Linux VM tests
scripts/libs.env                   pinned upstream tags + sha256
scripts/build-libs.sh              builds lib/ bundle for the current platform
scripts/pack-sparse.py             ext4 image -> non-zero extent stream
crates/sandcastle-proto/           Job / Status / ProbeReport wire types
crates/sandcastle-guest/
  src/main.rs                      non-Linux stub + dispatch
  src/linux.rs                     mount /out, read job, write status, exit
  src/probe.rs                     store disk + overlay lowerdir+ probe
src/lib.rs                         module list
src/main.rs                        clap CLI: doctor, hidden __vm
src/install.rs                     locate lib dir, guest helper, store root
src/store.rs                       store lock, guest-root, sparse disk expansion, job dirs
src/vm/mod.rs                      VmSpec, run_job, outcome
src/vm/krun.rs                     dlopen bindings (only unsafe in the crate)
src/vm/child.rs                    __vm: configure context, sandbox, start_enter
src/vm/landlock.rs                 Linux host-side Landlock rules
src/doctor.rs                      doctor report
tests/vm.rs                        ignored VM integration tests
tests/cli.rs                       ignored `sandcastle doctor --json` test
```

---

### Task 1: Workspace, wire types, guest skeleton, build recipe

**Files:**
- Modify: `Cargo.toml`, `src/main.rs`, `.gitignore`
- Create: `.cargo/config.toml`, `justfile`, `sandcastle.entitlements`, `crates/sandcastle-proto/Cargo.toml`, `crates/sandcastle-proto/src/lib.rs`, `crates/sandcastle-guest/Cargo.toml`, `crates/sandcastle-guest/src/main.rs`, `crates/sandcastle-guest/src/linux.rs`

**Interfaces:**
- Produces: `sandcastle_proto::{Job, Status, ProbeReport, JOB_FILE, STATUS_FILE}`; `Job::Probe { exit_code: i32 }`; `Status { exit_code: i32, probe: Option<ProbeReport> }`; `ProbeReport { kernel_release: String, overlay_lowerdir_plus: bool }`; `just build` → `target/release/sandcastle` (signed on macOS) + `target/release/sandcastle-guest` (static musl).

- [ ] **Step 1: Install the musl target**

Run: `rustup target add "$(uname -m | sed s/arm64/aarch64/)-unknown-linux-musl"`
Expected: target installed (or "up to date").

- [ ] **Step 2: Convert to a workspace**

`Cargo.toml`:

```toml
[workspace]
members = [".", "crates/sandcastle-proto", "crates/sandcastle-guest"]
resolver = "3"

[workspace.package]
edition = "2024"
rust-version = "1.89"

[package]
name = "sandcastle"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
anyhow = "1.0.104"
clap = { version = "4.6.7", features = ["derive"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
sandcastle-proto = { path = "crates/sandcastle-proto" }
```

`.gitignore` (append):

```
/lib
```

`.cargo/config.toml`:

```toml
# The guest helper is a static musl binary; rust-lld links it on any host,
# including macOS, without a Linux cross toolchain.
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"

[target.x86_64-unknown-linux-musl]
linker = "rust-lld"
```

- [ ] **Step 3: Write the failing wire-format test**

`crates/sandcastle-proto/Cargo.toml`:

```toml
[package]
name = "sandcastle-proto"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { version = "1.0.229", features = ["derive"] }

[dev-dependencies]
serde_json = "1.0.151"
```

`crates/sandcastle-proto/src/lib.rs` (test only first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Host and guest are built for different targets and copied separately,
    // so the JSON on the virtio-fs share is a contract; pin its exact shape.
    #[test]
    fn job_wire_format() {
        let job: Job = serde_json::from_str(r#"{"mode":"probe","exit_code":127}"#).unwrap();
        assert_eq!(job, Job::Probe { exit_code: 127 });
    }

    #[test]
    fn status_wire_format() {
        let status: Status = serde_json::from_str(
            r#"{"exit_code":0,"probe":{"kernel_release":"6.12.0","overlay_lowerdir_plus":true}}"#,
        )
        .unwrap();
        assert_eq!(status.probe.unwrap().kernel_release, "6.12.0");
        let bare: Status = serde_json::from_str(r#"{"exit_code":3}"#).unwrap();
        assert_eq!(bare, Status { exit_code: 3, probe: None });
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p sandcastle-proto`
Expected: FAIL to compile with "cannot find type `Job`".

- [ ] **Step 5: Implement the wire types**

Prepend to `crates/sandcastle-proto/src/lib.rs`:

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

/// Work the guest helper performs in one VM boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Job {
    /// Self-test used by `sandcastle doctor`. The helper exits with `exit_code`.
    Probe { exit_code: i32 },
}

/// Result written by the guest helper. Authoritative over the VM process
/// exit code, because libkrun's init reserves 125, 126 and 127.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
}

/// Guest capabilities found by [`Job::Probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub kernel_release: String,
    /// Overlayfs accepts `lowerdir+` (kernel ≥ 6.8), needed for long layer stacks.
    pub overlay_lowerdir_plus: bool,
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p sandcastle-proto`
Expected: PASS (2 tests).

- [ ] **Step 7: Create the guest helper skeleton**

`crates/sandcastle-guest/Cargo.toml`:

```toml
[package]
name = "sandcastle-guest"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
anyhow = "1.0.104"
serde_json = "1.0.151"
sandcastle-proto = { path = "../sandcastle-proto" }

[target.'cfg(target_os = "linux")'.dependencies]
rustix = { version = "1.1.5", features = ["mount", "system"] }
```

`crates/sandcastle-guest/src/main.rs`:

```rust
//! Static helper executed by libkrun's init inside every sandcastle microVM.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandcastle-guest only runs inside a sandcastle microVM");
    std::process::exit(125);
}
```

`crates/sandcastle-guest/src/linux.rs`:

```rust
use std::ffi::CStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::{fs, process};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount};
use sandcastle_proto::{JOB_FILE, Job, STATUS_FILE, Status};

const OUT: &str = "/out";

pub fn main() -> ! {
    match run() {
        Ok(code) => process::exit(code),
        Err(e) => {
            // No status file: the host reports a setup failure.
            eprintln!("sandcastle-guest: {e:#}");
            process::exit(125)
        }
    }
}

fn run() -> Result<i32> {
    mount("out", OUT, "virtiofs", MountFlags::empty(), None::<&CStr>)
        .context("mounting the out share")?;
    let out = Path::new(OUT);
    let job: Job = serde_json::from_slice(&fs::read(out.join(JOB_FILE)).context("reading job")?)
        .context("parsing job")?;
    let status = match job {
        Job::Probe { exit_code } => Status { exit_code, probe: None },
    };
    let mut file = File::create(out.join(STATUS_FILE)).context("creating status file")?;
    file.write_all(&serde_json::to_vec(&status)?)?;
    // The VM is torn down right after exit; make sure the host sees the bytes.
    file.sync_all()?;
    Ok(status.exit_code)
}
```

- [ ] **Step 8: Replace the host placeholder**

`src/main.rs`:

```rust
fn main() {
    eprintln!("sandcastle: no commands yet");
    std::process::exit(2);
}
```

- [ ] **Step 9: Add the entitlement and build recipe**

`sandcastle.entitlements`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
```

`justfile`:

```just
set shell := ["bash", "-euo", "pipefail", "-c"]

guest_target := arch() + "-unknown-linux-musl"
platform := os() + "-" + arch()
lib_dir := justfile_directory() / "lib"
bin := justfile_directory() / "target/release/sandcastle"

# Build host + guest helper (release); signs the host on macOS.
build:
    cargo build --release -p sandcastle-guest --target {{guest_target}}
    cargo build --release -p sandcastle
    cp target/{{guest_target}}/release/sandcastle-guest target/release/sandcastle-guest
    {{ if os() == "macos" { "codesign --force --sign - --entitlements sandcastle.entitlements " + bin } else { "true" } }}

# Fast tests (no VM).
test:
    cargo test --workspace

# VM integration tests: need lib/ (see build-libs) and a hypervisor.
it: build
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_BIN={{bin}} cargo test --release --workspace -- --ignored --test-threads=1

# Build the libkrun/libkrunfw/store-template bundle into lib/.
build-libs:
    scripts/build-libs.sh {{lib_dir}}

# Download the bundle of the latest CI run into lib/ (needs a GitHub remote).
fetch-libs:
    rm -rf {{lib_dir}}
    gh run download --name sandcastle-libs-{{platform}} --dir {{lib_dir}}
    cd {{lib_dir}} && shasum -a 256 -c SHA256SUMS
```

- [ ] **Step 10: Verify the build**

Run: `just build && file target/release/sandcastle-guest`
Expected: `ELF 64-bit LSB executable, ARM aarch64 ... statically linked` (x86-64 on x86 Linux). On macOS, `codesign -d --entitlements - target/release/sandcastle` lists `com.apple.security.hypervisor`.

Run: `cargo test --workspace`
Expected: PASS (proto tests). Guest compiles to the stub on macOS.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore .cargo justfile sandcastle.entitlements src/main.rs crates
git commit -m "Add workspace with wire types, guest helper skeleton and build recipe" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: libkrun bundle script and CI bundle job

**Files:**
- Create: `scripts/libs.env`, `scripts/build-libs.sh`, `scripts/pack-sparse.py`, `NOTICE`, `.github/workflows/ci.yml`

**Interfaces:**
- Produces: `lib/` containing `libkrun.so.1` + `libkrunfw.so.5` (Linux) or `libkrun.1.dylib` + `libkrunfw.5.dylib` (macOS), `store-template.ext4.zst`, `PROVENANCE`, `SHA256SUMS`, `licenses/`. Template stream format (before zstd): magic `SCX1`, `u64` LE disk size, then records `u64` LE offset, `u32` LE length, `length` bytes; records cover every non-zero 64 KiB chunk. CI artifact `sandcastle-libs-<os>-<arch>` (`macos-aarch64`, `linux-x86_64`, `linux-aarch64`).

- [ ] **Step 1: Pin upstream inputs**

`scripts/libs.env`:

```bash
# Upstream inputs of the bundled VM libraries. Changing anything here
# invalidates the CI cache and rebuilds lib/.
LIBKRUN_TAG=v1.19.6
LIBKRUN_SRC_SHA256=7025d72208172dc06f791ad5af7ccaafeabdac9869f74ccb45fb6d4f6e5991cd
LIBKRUNFW_TAG=v5.6.2
LIBKRUNFW_PREBUILT_AARCH64_SHA256=adef6739cc3bda2a1b57f04c08ccbfdf206a6c831d76b32de1beddc0e7984e86
LIBKRUNFW_X86_64_SHA256=016c32ddb2a28aa300382cab33352d63a41a901eb36ee8d1ca24b389791c8b91
LIBKRUNFW_AARCH64_SHA256=dea7905a167eee17d482200ea2fe15871aceb293b0674ac825ed7ae69759399f
# Newest glibc symbol version libkrun.so may require (Ubuntu 22.04).
GLIBC_MAX=2.35
STORE_DISK_BYTES=68719476736
```

- [ ] **Step 2: Write the sparse packer**

`scripts/pack-sparse.py`:

```python
#!/usr/bin/env python3
"""Write the non-zero 64 KiB chunks of a sparse disk image to stdout.

Format: b"SCX1", u64 LE size, then records of (u64 LE offset, u32 LE length,
bytes). sandcastle expands it with holes for everything not listed.
"""
import errno
import os
import struct
import sys

CHUNK = 1 << 16

def main(path: str) -> None:
    out = sys.stdout.buffer
    fd = os.open(path, os.O_RDONLY)
    size = os.fstat(fd).st_size
    out.write(b"SCX1" + struct.pack("<Q", size))
    pos = 0
    while pos < size:
        try:
            start = os.lseek(fd, pos, os.SEEK_DATA)
        except OSError as e:
            if e.errno == errno.ENXIO:  # no data after pos
                break
            raise
        end = os.lseek(fd, start, os.SEEK_HOLE)
        off = start
        while off < end:
            n = min(CHUNK, end - off)
            buf = os.pread(fd, n, off)
            if buf.count(0) != len(buf):
                out.write(struct.pack("<QI", off, len(buf)))
                out.write(buf)
            off += n
        pos = end

if __name__ == "__main__":
    main(sys.argv[1])
```

- [ ] **Step 3: Write the bundle script**

`scripts/build-libs.sh` (then `chmod +x scripts/build-libs.sh scripts/pack-sparse.py`):

```bash
#!/usr/bin/env bash
# Build the bundled libkrun + libkrunfw + ext4 store template for this
# platform into the directory given as $1 (default: ./lib).
#
# macOS needs: brew install lld xz e2fsprogs zstd
# Linux needs: apt-get install patchelf libc6-dev e2fsprogs zstd binutils
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=libs.env
source "$here/libs.env"

out_arg="${1:-lib}"
rm -rf "$out_arg"
mkdir -p "$out_arg/licenses"
out="$(cd "$out_arg" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

os="$(uname -s)"
arch="$(uname -m)"
[ "$arch" = arm64 ] && arch=aarch64
fw_url="https://github.com/containers/libkrunfw/releases/download/$LIBKRUNFW_TAG"

fetch() { # url sha256 dest
    curl -fsSL "$1" -o "$3"
    echo "$2  $3" | shasum -a 256 -c - >/dev/null
}

ncpu() { getconf _NPROCESSORS_ONLN; }

# libkrunfw: the prebuilt tarball carries the kernel as kernel.c plus the
# GPL/LGPL license files; macOS compiles it, Linux uses the ready .so.
fetch "$fw_url/libkrunfw-prebuilt-aarch64.tgz" "$LIBKRUNFW_PREBUILT_AARCH64_SHA256" "$work/fw-prebuilt.tgz"
tar -xzf "$work/fw-prebuilt.tgz" -C "$work"
cp "$work/libkrunfw/LICENSE-GPL-2.0-only" "$work/libkrunfw/LICENSE-LGPL-2.1-only" "$out/licenses/"
case "$os" in
Darwin)
    make -C "$work/libkrunfw" -j"$(ncpu)"
    cp "$work/libkrunfw/libkrunfw.5.dylib" "$out/"
    ;;
Linux)
    case "$arch" in
    x86_64) fw_sha="$LIBKRUNFW_X86_64_SHA256" ;;
    aarch64) fw_sha="$LIBKRUNFW_AARCH64_SHA256" ;;
    *) echo "unsupported arch $arch" >&2; exit 1 ;;
    esac
    fetch "$fw_url/libkrunfw-$arch.tgz" "$fw_sha" "$work/fw.tgz"
    tar -xzf "$work/fw.tgz" -C "$work"
    cp -L "$work/lib/$arch-linux-gnu/libkrunfw.so.5" "$out/"
    ;;
*) echo "unsupported OS $os" >&2; exit 1 ;;
esac

# libkrun from the upstream source tag, with virtio-blk for the store disk.
fetch "https://github.com/containers/libkrun/archive/refs/tags/$LIBKRUN_TAG.tar.gz" "$LIBKRUN_SRC_SHA256" "$work/krun.tgz"
tar -xzf "$work/krun.tgz" -C "$work"
krun_src="$work/libkrun-${LIBKRUN_TAG#v}"
make -C "$krun_src" BLK=1 -j"$(ncpu)"
cp "$krun_src/LICENSE" "$out/licenses/LICENSE-libkrun"
case "$os" in
Darwin)
    cp "$krun_src/target/release/libkrun.${LIBKRUN_TAG#v}.dylib" "$out/libkrun.1.dylib"
    # Only system libraries may be referenced, or the bundle breaks off this machine.
    if otool -L "$out/libkrun.1.dylib" | tail -n +2 | grep -vE '^\s+(/usr/lib/|/System/)'; then
        echo "libkrun.1.dylib links non-system libraries (listed above)" >&2; exit 1
    fi
    mke2fs="$(brew --prefix e2fsprogs)/sbin/mke2fs"
    ;;
Linux)
    cp "$krun_src/target/release/libkrun.so.${LIBKRUN_TAG#v}" "$out/libkrun.so.1"
    newest="$(objdump -T "$out/libkrun.so.1" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
    if [ "$(printf '%s\n%s\n' "$newest" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
        echo "libkrun.so.1 needs glibc $newest > $GLIBC_MAX" >&2; exit 1
    fi
    mke2fs=mke2fs
    ;;
esac

# Empty ext4 store template. Inode tables and journal are written as zeros
# (lazy_*_init=0) so the guest kernel never zeroes them later and inflates
# the sparse file; pack-sparse.py drops the zero chunks again.
dd if=/dev/zero of="$work/store.ext4" bs=1 count=0 seek="$STORE_DISK_BYTES" 2>/dev/null
"$mke2fs" -q -F -t ext4 -m 0 -E lazy_itable_init=0,lazy_journal_init=0,nodiscard "$work/store.ext4"
python3 "$here/pack-sparse.py" "$work/store.ext4" | zstd -q -19 -o "$out/store-template.ext4.zst"

{
    echo "libkrun $LIBKRUN_TAG source sha256 $LIBKRUN_SRC_SHA256 (https://github.com/containers/libkrun)"
    echo "libkrunfw $LIBKRUNFW_TAG (https://github.com/containers/libkrunfw/tree/$LIBKRUNFW_TAG)"
    echo "built on $os $arch with make BLK=1"
} >"$out/PROVENANCE"
(cd "$out" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | sort | xargs shasum -a 256 >SHA256SUMS)
ls -l "$out"
```

- [ ] **Step 4: Run it locally**

Run: `brew install lld xz e2fsprogs zstd && just build-libs` (macOS) or install the apt packages from the script header and run `just build-libs` (Linux).
Expected: `lib/` lists the two libraries, `store-template.ext4.zst` (expect under 5 MB), `PROVENANCE`, `SHA256SUMS`, `licenses/`. Run `cd lib && shasum -a 256 -c SHA256SUMS`: all `OK`.

If the macOS libkrunfw `make` tries to compile a kernel instead of `kernel.c`, check `ls $work/libkrunfw` (add `echo "$work"` and drop the trap temporarily). The prebuilt tarball must be used as-is: run `make` in the directory that contains `kernel.c`.

- [ ] **Step 5: Add NOTICE**

`NOTICE`:

```
sandcastle release archives bundle third-party components in lib/:

- libkrun (https://github.com/containers/libkrun), Apache-2.0,
  lib/licenses/LICENSE-libkrun.
- libkrunfw (https://github.com/containers/libkrunfw), library code
  LGPL-2.1-only; embedded Linux kernel GPL-2.0-only. Licenses in
  lib/licenses/. The exact source tag is recorded in lib/PROVENANCE;
  kernel source and patches are available from that libkrunfw tag.
```

- [ ] **Step 6: Add the CI bundle job**

`.github/workflows/ci.yml`:

```yaml
name: ci
on:
  push:
  pull_request:

jobs:
  libs:
    strategy:
      matrix:
        include:
          - { runner: macos-14, platform: macos-aarch64 }
          - { runner: ubuntu-22.04, platform: linux-x86_64 }
          - { runner: ubuntu-22.04-arm, platform: linux-aarch64 }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - id: cache
        uses: actions/cache@v4
        with:
          path: lib
          key: libs-${{ matrix.platform }}-${{ hashFiles('scripts/build-libs.sh', 'scripts/libs.env', 'scripts/pack-sparse.py') }}
      - if: steps.cache.outputs.cache-hit != 'true' && runner.os == 'macOS'
        run: brew install lld xz e2fsprogs zstd
      - if: steps.cache.outputs.cache-hit != 'true' && runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y patchelf libc6-dev e2fsprogs zstd binutils
      - if: steps.cache.outputs.cache-hit != 'true'
        run: scripts/build-libs.sh lib
      - uses: actions/upload-artifact@v4
        with:
          name: sandcastle-libs-${{ matrix.platform }}
          path: lib
```

- [ ] **Step 7: Commit**

```bash
git add scripts NOTICE .github
git commit -m "Add libkrun/libkrunfw bundle script and CI bundle job" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Install paths and runtime libkrun bindings

**Files:**
- Create: `src/lib.rs`, `src/install.rs`, `src/vm/mod.rs`, `src/vm/krun.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: bundle file names from Task 2.
- Produces:
  - `install::{Install, store_root, LIB_DIR_ENV, GUEST_ENV, ROOT_ENV}`; `Install { lib_dir: PathBuf, guest_bin: PathBuf }`; `Install::locate(exe: &Path) -> Result<Install>`; `Install::store_template(&self) -> PathBuf`; `store_root() -> Result<PathBuf>`.
  - `vm::krun::{Krun, Ctx, LIBKRUN_FILE}`; `Krun::load(lib_dir: &Path) -> Result<Krun>`; `Krun::set_log_level_error(&self) -> Result<()>`; `Krun::create_ctx(&self) -> Result<Ctx<'_>>`; `Ctx::{set_vm_config(&mut self, u8, u32), set_root(&mut self, &Path), set_workdir(&mut self, &str), add_disk(&mut self, &str, &Path, bool), add_virtiofs(&mut self, &str, &Path), set_exec(&mut self, &str, &[&str], &[&str])} -> Result<()>`; `Ctx::start_enter(self) -> anyhow::Error`.

- [ ] **Step 1: Add dependencies**

`Cargo.toml` `[dependencies]` additions:

```toml
libloading = "0.9.0"
```

and:

```toml
[dev-dependencies]
tempfile = "3.27.0"
```

- [ ] **Step 2: Write the failing test**

`src/lib.rs`:

```rust
//! sandcastle: Dockerfile image builder running build steps in libkrun microVMs.

pub mod install;
pub mod vm;
```

`src/vm/mod.rs`:

```rust
//! Running guest-helper jobs in libkrun microVMs.

pub mod krun;
```

`src/vm/krun.rs` (test only first):

```rust
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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p sandcastle load_error`
Expected: FAIL to compile with "cannot find type `Krun`".

- [ ] **Step 4: Implement install paths**

`src/install.rs`:

```rust
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
        let exe_dir = exe.parent().context("executable path has no parent directory")?;
        let lib_dir = env::var_os(LIB_DIR_ENV).map_or_else(|| exe_dir.join("lib"), PathBuf::from);
        let guest_bin = env::var_os(GUEST_ENV).map_or_else(|| exe_dir.join(GUEST_BIN), PathBuf::from);
        Ok(Self {
            lib_dir: fs::canonicalize(&lib_dir).with_context(|| {
                format!("library directory {} not found (set {LIB_DIR_ENV} or run `just build-libs`)", lib_dir.display())
            })?,
            guest_bin: fs::canonicalize(&guest_bin).with_context(|| {
                format!("guest helper {} not found (set {GUEST_ENV} or run `just build`)", guest_bin.display())
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
```

- [ ] **Step 5: Implement the bindings**

Prepend to `src/vm/krun.rs`:

```rust
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
            format!("cannot load {} (set {LIB_DIR_ENV} or run `just build-libs`)", path.display())
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
        check(unsafe { (self.set_log_level)(LOG_LEVEL_ERROR) }, "krun_set_log_level").map(drop)
    }

    pub fn create_ctx(&self) -> Result<Ctx<'_>> {
        // SAFETY: no arguments.
        let id = check(unsafe { (self.create_ctx)() }, "krun_create_ctx")?;
        Ok(Ctx { krun: self, id: id as u32 })
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
        check(unsafe { (self.krun.set_vm_config)(self.id, vcpus, ram_mib) }, "krun_set_vm_config")
            .map(drop)
    }

    pub fn set_root(&mut self, dir: &Path) -> Result<()> {
        let dir = path_cstring(dir)?;
        // SAFETY: `dir` is NUL-terminated and outlives the call; libkrun copies it.
        check(unsafe { (self.krun.set_root)(self.id, dir.as_ptr()) }, "krun_set_root").map(drop)
    }

    pub fn set_workdir(&mut self, dir: &str) -> Result<()> {
        let dir = CString::new(dir)?;
        // SAFETY: as in `set_root`.
        check(unsafe { (self.krun.set_workdir)(self.id, dir.as_ptr()) }, "krun_set_workdir").map(drop)
    }

    pub fn add_disk(&mut self, block_id: &str, image: &Path, read_only: bool) -> Result<()> {
        let block_id = CString::new(block_id)?;
        let image = path_cstring(image)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; libkrun copies them.
        let rc = unsafe { (self.krun.add_disk)(self.id, block_id.as_ptr(), image.as_ptr(), read_only) };
        check(rc, "krun_add_disk").map(drop)
    }

    pub fn add_virtiofs(&mut self, tag: &str, dir: &Path) -> Result<()> {
        let tag = CString::new(tag)?;
        let dir = path_cstring(dir)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; libkrun copies them.
        check(unsafe { (self.krun.add_virtiofs)(self.id, tag.as_ptr(), dir.as_ptr()) }, "krun_add_virtiofs")
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
        let rc = unsafe { (self.krun.set_exec)(self.id, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
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
    items.iter().map(|s| CString::new(*s).map_err(Into::into)).collect()
}

fn padded_ptrs(items: &[CString]) -> Result<Vec<*const c_char>> {
    anyhow::ensure!(items.len() < KRUN_MAX_ARGS, "too many arguments for libkrun");
    let mut ptrs: Vec<*const c_char> = items.iter().map(|s| s.as_ptr()).collect();
    ptrs.resize(KRUN_MAX_ARGS, std::ptr::null());
    Ok(ptrs)
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p sandcastle load_error`
Expected: PASS.

- [ ] **Step 7: Lint**

Run: `cargo test -p sandcastle && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, no warnings (the items are `pub` in the lib crate, so they are not dead code before Task 5 uses them).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src
git commit -m "Add install paths and runtime-loaded libkrun bindings" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Store: lock, guest root, sparse disk, job dirs

**Files:**
- Create: `src/store.rs`
- Modify: `src/lib.rs`, `Cargo.toml`

**Interfaces:**
- Consumes: `Install { lib_dir, guest_bin }`, `Install::store_template()` (Task 3); template stream format (Task 2).
- Produces: `store::{Store, GUEST_HELPER_PATH}`; `Store::open(root: &Path, install: &Install) -> Result<Store>`; `Store::root(&self) -> &Path`; `Store::disk(&self) -> PathBuf`; `Store::guest_root(&self) -> PathBuf`; `Store::new_job_dir(&self) -> Result<PathBuf>` (creates `<dir>/out/`); `store::expand_sparse(src: impl Read, dest: &Path) -> Result<()>`.

- [ ] **Step 1: Add the dependency**

`Cargo.toml` `[dependencies]`:

```toml
zstd = "0.14.0"
```

- [ ] **Step 2: Write the failing tests**

`src/lib.rs`: add `pub mod store;`.

`src/store.rs` (tests only first):

```rust
#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn template(size: u64, extents: &[(u64, &[u8])]) -> Vec<u8> {
        let mut raw = b"SCX1".to_vec();
        raw.extend_from_slice(&size.to_le_bytes());
        for (off, data) in extents {
            raw.extend_from_slice(&off.to_le_bytes());
            raw.extend_from_slice(&(data.len() as u32).to_le_bytes());
            raw.extend_from_slice(data);
        }
        zstd::encode_all(&raw[..], 3).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, Install) {
        let dir = tempfile::tempdir().unwrap();
        let lib_dir = dir.path().join("lib");
        fs::create_dir(&lib_dir).unwrap();
        fs::write(lib_dir.join("store-template.ext4.zst"), template(1 << 20, &[(0, b"superblock")])).unwrap();
        let guest_bin = dir.path().join("sandcastle-guest");
        fs::write(&guest_bin, b"helper v1").unwrap();
        (dir, Install { lib_dir, guest_bin })
    }

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
        assert!(allocated < size / 4, "disk is not sparse: {allocated} bytes allocated");
    }

    #[test]
    fn expand_sparse_rejects_extent_past_end() {
        let dir = tempfile::tempdir().unwrap();
        let src = template(4096, &[(4000, &[1; 200])]);
        let err = expand_sparse(zstd::Decoder::new(&src[..]).unwrap(), &dir.path().join("d")).unwrap_err();
        assert!(format!("{err:#}").contains("past the end"), "{err:#}");
        assert!(!dir.path().join("d").exists());
    }

    #[test]
    fn second_open_fails_while_locked() {
        let (dir, install) = fixture();
        let root = dir.path().join("store");
        let _first = Store::open(&root, &install).unwrap();
        let err = Store::open(&root, &install).err().unwrap();
        assert!(format!("{err:#}").contains("another sandcastle process"), "{err:#}");
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
        let copied = store.guest_root().join(GUEST_HELPER_PATH.trim_start_matches('/'));
        assert_eq!(fs::read(copied).unwrap(), b"helper v2");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p sandcastle store::`
Expected: FAIL to compile with "cannot find type `Store`" / "cannot find function `expand_sparse`".

- [ ] **Step 4: Implement the store**

Prepend to `src/store.rs`:

```rust
//! On-disk state: the ext4 store disk, the guest root shared as `/`, and
//! per-job directories. One process at a time: the ext4 disk cannot be
//! mounted by two VMs.

use std::fs::{self, File, Permissions, TryLockError};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail, ensure};

use crate::install::Install;

/// Path of the guest helper inside the VM (relative to the guest root).
pub const GUEST_HELPER_PATH: &str = "/sandcastle-guest";
/// Mount points libkrun's init and the guest helper expect on the root share.
const GUEST_DIRS: &[&str] = &["dev", "proc", "sys", "tmp", "out", "store"];
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
        let lock = File::options().create(true).truncate(false).write(true).open(root.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!("another sandcastle process is using the store at {}", root.display())
            }
            Err(TryLockError::Error(e)) => return Err(e).context("locking the store"),
        }

        let jobs = root.join("jobs");
        if jobs.exists() {
            fs::remove_dir_all(&jobs).context("removing job directories left by an earlier run")?;
        }
        fs::create_dir_all(&jobs)?;

        let store = Self { root, next_job: AtomicU32::new(0), _lock: lock };
        store.prepare_guest_root(&install.guest_bin)?;
        let disk = store.disk();
        if !disk.exists() {
            let template = install.store_template();
            let file = File::open(&template).with_context(|| format!("opening {}", template.display()))?;
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

    fn prepare_guest_root(&self, helper: &Path) -> Result<()> {
        let guest_root = self.guest_root();
        for dir in GUEST_DIRS {
            fs::create_dir_all(guest_root.join(dir))?;
        }
        let want = fs::read(helper).with_context(|| format!("reading {}", helper.display()))?;
        let dest = guest_root.join(GUEST_HELPER_PATH.trim_start_matches('/'));
        if fs::read(&dest).ok().as_deref() != Some(&want[..]) {
            let tmp = guest_root.join(".sandcastle-guest.tmp");
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
    src.read_exact(&mut magic).context("reading template header")?;
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
            Ok(_) => src.read_exact(&mut header[1..]).context("truncated template record")?,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let offset = u64::from_le_bytes(header[..8].try_into()?);
        let len = u32::from_le_bytes(header[8..].try_into()?);
        ensure!(
            offset.checked_add(u64::from(len)).is_some_and(|end| end <= size),
            "template extent at {offset} (+{len}) is past the end of the {size}-byte disk"
        );
        buf.resize(len as usize, 0);
        src.read_exact(&mut buf).context("truncated template extent")?;
        out.write_all_at(&buf, offset)?;
    }
    out.sync_all()?;
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p sandcastle store::`
Expected: PASS (5 tests).

- [ ] **Step 6: Lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings. The real template is exercised end to end by Task 5's integration tests.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src
git commit -m "Add store with lock, guest root and sparse disk expansion" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: VM runner, `__vm` child and guest probe

**Files:**
- Create: `src/vm/child.rs`, `crates/sandcastle-guest/src/probe.rs`, `tests/vm.rs`
- Modify: `src/vm/mod.rs`, `src/main.rs`, `crates/sandcastle-guest/src/linux.rs`, `crates/sandcastle-guest/src/main.rs`

**Interfaces:**
- Consumes: `Krun`/`Ctx` (Task 3), `Store` (Task 4), `Job`/`Status`/`ProbeReport` (Task 1).
- Produces: `vm::{VmSpec, run_job, outcome, SPEC_FILE}`; `run_job(exe: &Path, store: &Store, install: &Install, job: &Job) -> Result<Status>`; `outcome(child_exit: Option<i32>, status: Option<Status>) -> Result<Status>`; `vm::child::enter(job_dir: &Path) -> anyhow::Error`; CLI `sandcastle __vm <job_dir>`.

- [ ] **Step 1: Write the failing unit tests**

Append to `src/vm/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_file_wins_over_child_exit_code() {
        let status = outcome(Some(127), Some(Status { exit_code: 127, probe: None })).unwrap();
        assert_eq!(status.exit_code, 127);
    }

    #[test]
    fn missing_status_is_setup_failure_even_with_exit_127() {
        let err = format!("{:#}", outcome(Some(127), None).unwrap_err());
        assert!(err.contains("setup failed"), "{err}");
        assert!(err.contains("127"), "{err}");
    }

    #[test]
    fn missing_status_after_signal_is_setup_failure() {
        let err = format!("{:#}", outcome(None, None).unwrap_err());
        assert!(err.contains("killed by a signal"), "{err}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle vm::tests`
Expected: FAIL to compile with "cannot find function `outcome`".

- [ ] **Step 3: Implement the runner**

Replace the top of `src/vm/mod.rs` (keep the tests module):

```rust
//! Running guest-helper jobs in libkrun microVMs.
//!
//! `krun_start_enter` takes over the calling process and exits with the
//! guest's exit code, so every job runs in a re-executed `sandcastle __vm`
//! child. The guest's `status.json` is authoritative; the child's exit code
//! is only used to describe failures before the helper ran.

pub mod child;
pub mod krun;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sandcastle_proto::{JOB_FILE, Job, STATUS_FILE, Status};
use serde::{Deserialize, Serialize};

use crate::install::Install;
use crate::store::Store;

/// VM description the parent writes into the job dir for the `__vm` child.
pub const SPEC_FILE: &str = "vm.json";
const RAM_MIB: u32 = 2048;

#[cfg(target_os = "linux")]
const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";
#[cfg(target_os = "macos")]
const LIB_PATH_ENV: &str = "DYLD_LIBRARY_PATH";

#[derive(Debug, Serialize, Deserialize)]
pub struct VmSpec {
    pub lib_dir: PathBuf,
    pub guest_root: PathBuf,
    pub disk: PathBuf,
    pub out_dir: PathBuf,
    pub vcpus: u8,
    pub ram_mib: u32,
}

/// Runs `job` in a fresh microVM and returns the guest's status.
/// `exe` is the signed `sandcastle` binary used for the `__vm` child.
pub fn run_job(exe: &Path, store: &Store, install: &Install, job: &Job) -> Result<Status> {
    let job_dir = store.new_job_dir()?;
    let out_dir = job_dir.join("out");
    fs::write(out_dir.join(JOB_FILE), serde_json::to_vec(job)?)?;
    let vcpus = std::thread::available_parallelism().map_or(1, |n| n.get()).min(usize::from(u8::MAX));
    let spec = VmSpec {
        lib_dir: install.lib_dir.clone(),
        guest_root: store.guest_root(),
        disk: store.disk(),
        out_dir: out_dir.clone(),
        vcpus: vcpus as u8,
        ram_mib: RAM_MIB,
    };
    fs::write(job_dir.join(SPEC_FILE), serde_json::to_vec(&spec)?)?;

    // libkrun dlopens libkrunfw by bare file name; point the loader at the bundle.
    let exit = Command::new(exe)
        .arg("__vm")
        .arg(&job_dir)
        .env(LIB_PATH_ENV, &install.lib_dir)
        .status()
        .with_context(|| format!("starting {}", exe.display()))?;
    let status = read_status(&out_dir.join(STATUS_FILE))?;
    fs::remove_dir_all(&job_dir)?;
    outcome(exit.code(), status)
}

fn read_status(path: &Path) -> Result<Option<Status>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context("parsing guest status")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Decides a job's result. A missing status file means the helper never
/// finished, whatever the child exit code looks like (libkrun's init itself
/// uses 125/126/127).
pub fn outcome(child_exit: Option<i32>, status: Option<Status>) -> Result<Status> {
    match (status, child_exit) {
        (Some(status), _) => Ok(status),
        (None, Some(code)) => bail!("VM or guest helper setup failed before the step ran (VM process exited with {code})"),
        (None, None) => bail!("VM or guest helper setup failed before the step ran (VM process was killed by a signal)"),
    }
}
```

`src/vm/child.rs`:

```rust
//! The `sandcastle __vm <job_dir>` process: configures libkrun and enters the VM.

use std::convert::Infallible;
use std::fs;
use std::path::Path;

use anyhow::Result;

use super::krun::Krun;
use super::{SPEC_FILE, VmSpec};
use crate::store::GUEST_HELPER_PATH;

/// Never returns on success (libkrun exits the process with the guest's code).
pub fn enter(job_dir: &Path) -> anyhow::Error {
    match try_enter(job_dir) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

fn try_enter(job_dir: &Path) -> Result<Infallible> {
    let spec: VmSpec = serde_json::from_slice(&fs::read(job_dir.join(SPEC_FILE))?)?;
    let krun = Krun::load(&spec.lib_dir)?;
    krun.set_log_level_error()?;
    let mut ctx = krun.create_ctx()?;
    ctx.set_vm_config(spec.vcpus, spec.ram_mib)?;
    ctx.set_root(&spec.guest_root)?;
    ctx.add_disk("store", &spec.disk, false)?;
    ctx.add_virtiofs("out", &spec.out_dir)?;
    ctx.set_workdir("/")?;
    ctx.set_exec(GUEST_HELPER_PATH, &[], &["HOME=/"])?;
    Err(ctx.start_enter())
}
```

`src/main.rs`:

```rust
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::vm;

#[derive(Parser)]
#[command(name = "sandcastle", version, about = "Build OCI images from Dockerfiles in microVMs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Internal: configure and enter a microVM for one job.
    #[command(name = "__vm", hide = true)]
    Vm { job_dir: PathBuf },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Vm { job_dir } => {
            let err = vm::child::enter(&job_dir);
            eprintln!("sandcastle: {err:#}");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 4: Run unit tests to verify they pass**

Run: `cargo test -p sandcastle vm::tests`
Expected: PASS (3 tests).

- [ ] **Step 5: Implement the guest probe**

`crates/sandcastle-guest/src/probe.rs`:

```rust
//! `Job::Probe`: checks the store disk and overlayfs inside the guest.

use std::ffi::CStr;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::mount::{
    FsOpenFlags, MountFlags, UnmountFlags, fsconfig_create, fsconfig_set_string, fsopen, mount, unmount,
};
use sandcastle_proto::ProbeReport;

/// First virtio-blk device: the store disk added by the host.
const STORE_DEV: &str = "/dev/vda";
const STORE: &str = "/store";

pub fn run() -> Result<ProbeReport> {
    mount(STORE_DEV, STORE, "ext4", MountFlags::NOATIME, None::<&CStr>).context("mounting the store disk")?;
    let dir = Path::new(STORE).join("probe");
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    for sub in ["l1", "l2", "upper", "work"] {
        fs::create_dir_all(dir.join(sub)).context("writing to the store disk")?;
    }
    let overlay_lowerdir_plus = overlay_lowerdir_plus(&dir).is_ok();
    fs::remove_dir_all(&dir)?;
    unmount(STORE, UnmountFlags::empty()).context("unmounting the store disk")?;
    Ok(ProbeReport {
        kernel_release: rustix::system::uname().release().to_string_lossy().into_owned(),
        overlay_lowerdir_plus,
    })
}

/// Creates (without mounting) an overlay superblock using `lowerdir+`.
fn overlay_lowerdir_plus(dir: &Path) -> rustix::io::Result<()> {
    let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
    fsconfig_set_string(&fs, "lowerdir+", dir.join("l1"))?;
    fsconfig_set_string(&fs, "lowerdir+", dir.join("l2"))?;
    fsconfig_set_string(&fs, "upperdir", dir.join("upper"))?;
    fsconfig_set_string(&fs, "workdir", dir.join("work"))?;
    fsconfig_create(&fs)
}
```

In `crates/sandcastle-guest/src/main.rs` add below `mod linux;`:

```rust
#[cfg(target_os = "linux")]
mod probe;
```

In `crates/sandcastle-guest/src/linux.rs` replace the `Job::Probe` arm:

```rust
        Job::Probe { exit_code } => Status { exit_code, probe: Some(crate::probe::run()?) },
```

- [ ] **Step 6: Write the VM integration tests**

`tests/vm.rs`:

```rust
//! VM integration tests. Run with `just it` (needs lib/ and a hypervisor).

use std::path::PathBuf;

use sandcastle::install::Install;
use sandcastle::store::Store;
use sandcastle::vm::run_job;
use sandcastle_proto::Job;

fn sandcastle_bin() -> PathBuf {
    std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN must point at the signed sandcastle binary").into()
}

/// A store under a directory with a space, as on many macOS setups.
fn store() -> (tempfile::TempDir, PathBuf, Install, Store) {
    let exe = sandcastle_bin();
    let install = Install::locate(&exe).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("store root"), &install).unwrap();
    (dir, exe, install, store)
}

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn probe_mounts_store_and_supports_overlay_lowerdir_plus() {
    let (_dir, exe, install, store) = store();
    let status = run_job(&exe, &store, &install, &Job::Probe { exit_code: 0 }).unwrap();
    assert_eq!(status.exit_code, 0);
    let probe = status.probe.expect("probe report");
    assert!(!probe.kernel_release.is_empty());
    assert!(probe.overlay_lowerdir_plus, "guest kernel {} lacks overlay lowerdir+", probe.kernel_release);
}

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn guest_exit_127_comes_from_status_file() {
    let (_dir, exe, install, store) = store();
    let status = run_job(&exe, &store, &install, &Job::Probe { exit_code: 127 }).unwrap();
    assert_eq!(status.exit_code, 127);
    assert!(status.probe.is_some(), "status file must be present, not inferred from the exit code");
}
```

- [ ] **Step 7: Run the VM tests**

Run: `just build-libs` (if `lib/` is missing), then `just it`
Expected: PASS (2 ignored tests run). On macOS this is the first real HVF boot.

If it fails:
- `cannot load .../libkrun...`: check `ls lib/`.
- `krun_add_disk failed: Operation not supported`: libkrun was built without `BLK=1`.
- Exit with "setup failed" and guest stderr `mounting the out share`: virtio-fs tag mismatch; tag must be `out`.
- macOS `hv_vm_create` / `HV_DENIED`: binary not signed; `codesign -d --entitlements - target/release/sandcastle`.
- `overlay_lowerdir_plus` false: record `kernel_release` and stop. This triggers the spec fallback (short relative lowerdir paths) for plan 4; report to the user before continuing.

- [ ] **Step 8: Commit**

```bash
git add src crates tests
git commit -m "Run guest probe jobs in libkrun microVMs via a __vm child" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: `sandcastle doctor`

**Files:**
- Create: `src/doctor.rs`, `tests/cli.rs`
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `Install::locate`, `install::store_root`, `Store::open`, `vm::run_job`.
- Produces: `doctor::{run, Report, check_kvm}`; `run(exe: &Path, install: &Install, store_root: &Path) -> Result<Report>`; `Report { lib_dir: PathBuf, store_root: PathBuf, kernel_release: String, overlay_lowerdir_plus: bool }` (Serialize + Display); `check_kvm(path: &Path) -> Result<()>`; CLI `sandcastle doctor [--json]`.

- [ ] **Step 1: Write the failing tests**

`src/lib.rs`: add `pub mod doctor;`.

`src/doctor.rs` (tests only first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_error_explains_the_fix() {
        let err = format!("{:#}", check_kvm(Path::new("/nonexistent/kvm")).unwrap_err());
        assert!(err.contains("kvm"), "{err}");
        assert!(err.contains("group"), "{err}");
    }
}
```

`tests/cli.rs`:

```rust
//! End-to-end CLI test. Run with `just it`.

use std::process::Command;

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn doctor_json_reports_guest_kernel() {
    let bin = std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN");
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .args(["doctor", "--json"])
        .env("SANDCASTLE_ROOT", root.path().join("store"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["kernel_release"].as_str().is_some_and(|k| !k.is_empty()), "{report}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sandcastle doctor::`
Expected: FAIL to compile with "cannot find function `check_kvm`".

- [ ] **Step 3: Implement doctor**

Prepend to `src/doctor.rs`:

```rust
//! `sandcastle doctor`: boots one probe VM and reports what the guest supports.

use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sandcastle_proto::Job;
use serde::Serialize;

use crate::install::Install;
use crate::store::Store;
use crate::vm::run_job;

#[derive(Debug, Serialize)]
pub struct Report {
    pub lib_dir: PathBuf,
    pub store_root: PathBuf,
    pub kernel_release: String,
    pub overlay_lowerdir_plus: bool,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "libraries:       {}", self.lib_dir.display())?;
        writeln!(f, "store:           {}", self.store_root.display())?;
        writeln!(f, "guest kernel:    {}", self.kernel_release)?;
        writeln!(f, "overlay lowerdir+: {}", if self.overlay_lowerdir_plus { "yes" } else { "no" })
    }
}

pub fn run(exe: &Path, install: &Install, store_root: &Path) -> Result<Report> {
    #[cfg(target_os = "linux")]
    check_kvm(Path::new("/dev/kvm"))?;
    let store = Store::open(store_root, install)?;
    let status = run_job(exe, &store, install, &Job::Probe { exit_code: 0 })?;
    if status.exit_code != 0 {
        bail!("guest probe exited with {}", status.exit_code);
    }
    let probe = status.probe.context("guest returned no probe report")?;
    Ok(Report {
        lib_dir: install.lib_dir.clone(),
        store_root: store.root().to_path_buf(),
        kernel_release: probe.kernel_release,
        overlay_lowerdir_plus: probe.overlay_lowerdir_plus,
    })
}

/// KVM needs read-write access to the device; that is the only privilege sandcastle uses.
pub fn check_kvm(path: &Path) -> Result<()> {
    OpenOptions::new().read(true).write(true).open(path).map(drop).with_context(|| {
        format!(
            "cannot open {} read-write: enable virtualization and add your user to the `kvm` group",
            path.display()
        )
    })
}
```

On macOS `check_kvm` is only used by the test; add `#[cfg_attr(not(target_os = "linux"), allow(dead_code))]` above it if clippy warns.

In `src/main.rs`, extend `Command` and `main`:

```rust
#[derive(Subcommand)]
enum Command {
    /// Check that this host can run build steps in a microVM.
    Doctor {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Internal: configure and enter a microVM for one job.
    #[command(name = "__vm", hide = true)]
    Vm { job_dir: PathBuf },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Doctor { json } => doctor(json),
        Command::Vm { job_dir } => Err(vm::child::enter(&job_dir)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sandcastle: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn doctor(json: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let install = Install::locate(&exe)?;
    let report = sandcastle::doctor::run(&exe, &install, &install::store_root()?)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{report}");
    }
    Ok(())
}
```

and change the imports at the top of `src/main.rs` to:

```rust
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcastle::install::{self, Install};
use sandcastle::vm;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle doctor:: && just it`
Expected: PASS (unit test plus 3 ignored VM/CLI tests).

Run: `just build && SANDCASTLE_LIBKRUN_DIR=$PWD/lib target/release/sandcastle doctor`
Expected: four report lines, `overlay lowerdir+: yes`.

- [ ] **Step 5: Commit**

```bash
git add src tests
git commit -m "Add sandcastle doctor" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Landlock for the VM child and Linux CI

**Files:**
- Create: `src/vm/landlock.rs`
- Modify: `src/vm/mod.rs`, `src/vm/child.rs`, `Cargo.toml`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `VmSpec` (Task 5).
- Produces: `vm::landlock::{restrict, restrict_paths, Rules}` (Linux only); `Rules<'a> { rw_dirs: &'a [&'a Path], ro_dirs: &'a [&'a Path], rw_files: &'a [&'a Path], devices: &'a [&'a Path] }`; `restrict(spec: &VmSpec) -> Result<()>`; `restrict_paths(rules: &Rules) -> Result<bool>` (`false` = kernel without Landlock).

- [ ] **Step 1: Add the dependency**

`Cargo.toml`:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4.7"
```

- [ ] **Step 2: Write the failing test**

`src/vm/mod.rs`: add

```rust
#[cfg(target_os = "linux")]
pub mod landlock;
```

`src/vm/landlock.rs` (test only first):

```rust
#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    // Landlock applies to the calling thread and its future children only,
    // so the restriction stays inside this spawned thread.
    #[test]
    fn only_allowed_paths_stay_accessible() {
        let allowed = tempfile::tempdir().unwrap();
        let denied = tempfile::tempdir().unwrap();
        fs::write(denied.path().join("secret"), b"x").unwrap();
        let (a, d) = (allowed.path().to_path_buf(), denied.path().to_path_buf());
        std::thread::spawn(move || {
            let rules = Rules { rw_dirs: &[a.as_path()], ro_dirs: &[], rw_files: &[], devices: &[] };
            let enforced = restrict_paths(&rules).unwrap();
            if !enforced {
                eprintln!("kernel without Landlock; skipping");
                return;
            }
            fs::write(a.join("ok"), b"y").expect("allowed dir must stay writable");
            let err = fs::read(d.join("secret")).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        })
        .join()
        .unwrap();
    }
}
```

- [ ] **Step 3: Run test to verify it fails (Linux)**

Run (Linux host or CI): `cargo test -p sandcastle landlock`
Expected: FAIL to compile with "cannot find function `restrict_paths`".

- [ ] **Step 4: Implement**

Prepend to `src/vm/landlock.rs`:

```rust
//! Host-side Landlock sandbox for the `__vm` child (Linux only). Applied
//! after libkrun is configured and right before `krun_start_enter`, so
//! only the VM's own files stay reachable while the guest runs.

use std::path::Path;

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};

use super::VmSpec;

const ABI_LEVEL: ABI = ABI::V5;

pub struct Rules<'a> {
    pub rw_dirs: &'a [&'a Path],
    pub ro_dirs: &'a [&'a Path],
    pub rw_files: &'a [&'a Path],
    pub devices: &'a [&'a Path],
}

pub fn restrict(spec: &VmSpec) -> Result<()> {
    let enforced = restrict_paths(&Rules {
        // guest-root is the VM's `/` and is written by init (mount points).
        rw_dirs: &[spec.out_dir.as_path(), spec.guest_root.as_path()],
        // libkrun dlopens libkrunfw from here inside krun_start_enter.
        ro_dirs: &[spec.lib_dir.as_path()],
        rw_files: &[spec.disk.as_path()],
        devices: &[Path::new("/dev/kvm")],
    })?;
    if !enforced {
        eprintln!("sandcastle: warning: kernel lacks Landlock; VM process runs without a host sandbox");
    }
    Ok(())
}

/// Returns `false` if the kernel does not support Landlock at all.
pub fn restrict_paths(rules: &Rules) -> Result<bool> {
    let abi = ABI_LEVEL;
    let file_rw: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
    let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi))?.create()?;
    for dir in rules.rw_dirs {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, AccessFs::from_all(abi)))?;
    }
    for dir in rules.ro_dirs {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, AccessFs::from_read(abi)))?;
    }
    for file in rules.rw_files {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(file)?, file_rw))?;
    }
    for dev in rules.devices {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dev)?, file_rw | AccessFs::IoctlDev))?;
    }
    // restrict_self also sets PR_SET_NO_NEW_PRIVS.
    let status = ruleset.restrict_self()?;
    Ok(status.ruleset != RulesetStatus::NotEnforced)
}
```

In `src/vm/child.rs`, insert before `Err(ctx.start_enter())`:

```rust
    #[cfg(target_os = "linux")]
    super::landlock::restrict(&spec)?;
```

- [ ] **Step 5: Run tests (Linux)**

Run: `cargo test -p sandcastle landlock && just it`
Expected: PASS. If the VM tests now fail with a "setup failed" error that passed before Landlock, find the denied path:

```bash
strace -f -e trace=openat,open,mkdirat -o /tmp/vm.trace target/release/sandcastle doctor; grep EACCES /tmp/vm.trace
```

Add exactly those paths to `restrict` (narrowest rule that works), each with a comment explaining why libkrun needs it, and re-run.

- [ ] **Step 6: Add fast tests and the Linux VM job to CI**

Append to `.github/workflows/ci.yml` `jobs:`:

```yaml
  test:
    strategy:
      matrix:
        runner: [macos-14, ubuntu-22.04]
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace

  vm-linux:
    needs: libs
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          name: sandcastle-libs-linux-x86_64
          path: lib
      - name: Allow the runner user to use /dev/kvm
        run: |
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules
          sudo udevadm trigger --name-match=kvm
      - uses: taiki-e/install-action@just
      - run: rustup target add x86_64-unknown-linux-musl
      - run: just it
```

GitHub's macOS arm64 runners do not offer Hypervisor.framework to jobs, so macOS VM tests stay local (`just it`).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src .github
git commit -m "Sandbox the VM child with Landlock on Linux; add CI test jobs" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Out of scope for this plan

Dockerfile parsing (plan 1), registry pulls and OCI output (plan 3), guest `unpack`/`run`/`copy` and `sandcastle build` (plan 4), benchmark and README (plan 5). Release archive packaging (tarball with `sandcastle`, `sandcastle-guest`, `lib/`) is added once there is something to release.
