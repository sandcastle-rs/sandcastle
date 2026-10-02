# Sandcastle v1 — walking skeleton

## Goal

Build a single-stage Dockerfile into an OCI image layout directory, in Rust, on
macOS (Apple Silicon) and Linux, without root and without a container daemon.
v1 proves the whole path end to end:

```
Dockerfile → parse → pull base → unpack in guest → RUN/COPY in microVM → layer → config + manifest → OCI layout
```

Success criteria:

1. `sandcastle build -t demo -o ./out ./ctx` builds a Dockerfile using every v1
   instruction on both platforms.
2. The resulting `./out` loads and runs with `podman run oci:./out:demo` (or
   `skopeo copy oci:./out:demo docker-daemon:demo:latest` + `docker run`) and the
   container behaves as `docker build` would produce.
3. No step requires root. Linux requires only access to `/dev/kvm`.

## Scope

In scope instructions: `FROM` (registry pull), `RUN` (shell and exec form),
`COPY` (from build context), `ENV`, `WORKDIR`, `USER`, `CMD`, `ENTRYPOINT`,
`LABEL`, `EXPOSE`.

Out of scope for v1 (each is a later spec): multi-stage (`FROM … AS`,
`COPY --from`), `ARG`, build cache across builds, `ADD`, `SHELL`, `VOLUME`,
`ONBUILD`, `HEALTHCHECK`, `STOPSIGNAL`, `.dockerignore`, `COPY --chown`/`--chmod`,
heredocs, registry push, registry authentication (anonymous pulls only),
multi-arch and cross-arch builds, reproducible timestamps, Rust rewrite of the
imagebuilder conformance harness.

## Decisions

| Topic | Decision |
|---|---|
| Registry access | `oci-client` (oras-project), tokio runtime |
| Image/config types | `oci-spec` |
| Step execution | Upstream **libkrun stable v1.19.x** + libkrunfw. No smolvm fork features. |
| Distribution | Self-contained release tarball: `sandcastle`, `sandcastle-guest`, bundled `lib/libkrun*` + `lib/libkrunfw*`, ext4 store template. Nothing to install (same model as smolvm). |
| Isolation | Each `RUN`/`COPY` runs in its own microVM. Linux host side additionally hardened with Landlock. |
| Rootfs | Guest helper mounts overlayfs inside the VM over layers stored on an ext4 **store disk**. Host never mounts ext4. |
| Dockerfile parsing | In-house parser, a port of BuildKit's `frontend/dockerfile/parser` (see below). No third-party Dockerfile crate. |
| Architecture | Guest arch = host arch = image arch. No emulation. |
| Root | Never required |

## Architecture

Cargo workspace with four crates:

```
sandcastle/              (root package, host binary)
crates/sandcastle-guest/ (static musl binary, runs inside the VM)
crates/sandcastle-proto/ (serde job/status types shared by host and guest)
crates/sandcastle-dockerfile/ (Dockerfile parser + typed instructions, no I/O)
```

### Host binary (`sandcastle`)

| Module | Responsibility | Depends on |
|---|---|---|
| `cli` | `sandcastle build [-f Dockerfile] -t <name> -o <oci-dir> <context>`; hidden `__vm <job.json>` subcommand used for the VM child process | clap |
| `dockerfile` | Thin adapter: read the Dockerfile, call `sandcastle-dockerfile`, reject instructions outside the v1 scope with a line-numbered error | sandcastle-dockerfile |
| `registry` | Resolve reference, select `linux/<host arch>` from an index, fetch manifest + config, stream layer blobs into the blob store (skip existing digests) | oci-client, oci-spec |
| `store` | Owns the store root (`$SANDCASTLE_ROOT`, default `~/.local/share/sandcastle`): content-addressed `blobs/sha256/`, `store.ext4`, `guest-root/`, `jobs/`. Creates `store.ext4` once by expanding the bundled zstd-compressed empty ext4 template into a sparse file (64 GiB, zero runs become holes). Exclusive `flock` per build — ext4 cannot be mounted by two VMs. | zstd |
| `vm` | `ffi.rs`: hand-written `extern "C"` declarations for the ~10 libkrun calls used, loaded at runtime with `dlopen` (libloading) from `lib/` next to the executable, or `$SANDCASTLE_LIBKRUN_DIR`. No build-time link, so `cargo build`/`cargo test` work without libkrun. `run.rs`: spawns `sandcastle __vm`, inherits stdio, waits, reads `status.json`. The child configures the context, applies Landlock (Linux), calls `krun_start_enter`. All `unsafe` lives in `vm/ffi.rs` + the child setup function. | libloading, landlock (Linux) |
| `image` | Build `ImageConfiguration` (env, cmd, entrypoint, workdir, user, labels, exposed ports, history, `rootfs.diff_ids`), gzip layer tars while hashing both uncompressed (diff_id) and compressed (digest) in one pass, write manifest, `index.json` (with `org.opencontainers.image.ref.name`), `oci-layout` | oci-spec, flate2, sha2 |
| `build` | Orchestrates: iterate instructions, mutate config state, dispatch `RUN`/`COPY` jobs to `vm`, collect layers | all above |

Async is confined to `registry`; everything else is synchronous.

### Dockerfile parser (`sandcastle-dockerfile`)

Separate library crate: `&str` in, typed instructions out. No filesystem,
network, VM or OCI dependencies, so it builds and tests on any OS and can be
published on its own later. Public API:

- `parse(&str) -> Result<Dockerfile, ParseError>` — port of BuildKit's line
  parser; `Dockerfile { escape: char, nodes: Vec<Node> }`,
  `Node { cmd, args, flags, original, start_line, end_line }`.
- `Instruction::try_from(&Node)` — typed variants for the v1 set (`From`,
  `Run`, `Copy`, `Env`, `Workdir`, `User`, `Cmd`, `Entrypoint`, `Label`,
  `Expose`); any other known instruction becomes `Instruction::Other(Node)`
  so the scope decision stays in the host.
- `expand(&str, &Env, escape: char) -> Result<String, ExpandError>` — `$VAR`,
  `${VAR}`, `${VAR:-w}`, `${VAR:+w}`, honouring the escape char. Callers apply it to
  `ENV`, `WORKDIR`, `COPY`, `USER`, `LABEL`, `EXPOSE`, never `RUN`.

Dependencies: serde_json (exec-form arrays), thiserror (public error types).

Existing crates were probed against the Dockerfile reference:

| Case | `dockerfile-parser-rs` 3.3 | `dockerfile-parser` (HPE) 0.9 |
|---|---|---|
| Lowercase instructions (`run …`) | syntax error | ok |
| Single-char argument (`USER 0`) | syntax error | ok (untyped) |
| Shell-form `RUN echo "a    b"` | quotes dropped, split to `["echo","a","b"]` | verbatim |
| Exec-form `RUN ["echo","a  b"]` | `"a b"` (whitespace collapsed) | correct |
| `# escape=` directive | error | error |
| Heredoc | `EOF` delimiter only | error |
| Maintained | yes | last release 2024 |

Neither covers the reference; the first changes `RUN` semantics. We port
BuildKit's parser instead (~1,350 lines of Go, excluding tests): parser
directives (`escape`; `syntax` and `check` read and ignored), line
continuations with the active escape char, comments and blank lines inside
continuations, case-insensitive instruction names, `--flag=value` extraction,
JSON vs shell form with BuildKit's fallback rules, key/value parsing for
`ENV`/`LABEL` (including legacy `ENV key value`), and line ranges for errors.
Heredocs are parsed in a later spec.

BuildKit's `testfiles/` (33 Dockerfile + expected `result` pairs) and
`testfiles-negative/` are vendored (Apache-2.0, attributed in `NOTICE`) and
run as table tests against a dump of our `Node` tree in BuildKit's `result`
format. They are an external oracle, not self-written expectations.

### Guest helper (`sandcastle-guest`)

Static `*-unknown-linux-musl` binary. libkrun's `init.krun` is PID 1 and execs
the helper with the job path. Dependencies: rustix (mount API, chroot,
setuid), tar, sha2, serde_json.

Modes (one per job):

- **`unpack`** — for each base layer diff_id missing from `/store/layers/`,
  read the blob from `/blobs` (virtio-fs, read-only), decompress, extract into
  `/store/layers/<diff_id>.tmp/`, converting OCI `.wh.<name>` to overlay
  whiteouts (char device 0/0) and `.wh..wh..opq` to
  `trusted.overlay.opaque=y`, then rename to `/store/layers/<diff_id>/`. The
  rename is the completion marker.
- **`run`** — mount overlay (below), bind `/proc`, `/sys`, `/dev`, and
  `/etc/resolv.conf`, `/etc/hosts`, `/etc/hostname` from `guest-root` into the
  merged root, `mkdir -p` the workdir, chroot, set groups/gid/uid resolved from
  the merged `/etc/passwd` + `/etc/group` (numeric `USER` accepted directly),
  exec `/bin/sh -c "<cmd>"` or the exec-form argv with the image env, wait.
  Then unmount binds, remove any mount-point stub files it created, commit.
- **`copy`** — mount overlay, `mkdir -p` the workdir, copy sources from `/ctx`
  (build context, virtio-fs, read-only) into the merged root with Docker
  `COPY` semantics (directory → its contents; wildcards; trailing-`/` dest;
  files owned `0:0`, mode and mtime preserved), commit.

Overlay mount: `lowerdir` = previous step layers then base layers (top-most
first), `upperdir`/`workdir` under `/store/work/<job>/`, options
`index=off,metacopy=off,redirect_dir=off,xino=off` so the upper dir is a
self-contained diff. Lower layers are added with the new mount API
(`fsconfig` `lowerdir+`) to avoid the single-option page-size limit on long
layer stacks.

Commit: walk the upper dir in sorted order and write an uncompressed tar to
`/out/layer.tar`, converting overlay whiteouts back to OCI `.wh.` entries,
hashing as it writes. Rename the upper dir to `/store/layers/<diff_id>/` (drop
it if that already exists) so later steps use it as a lower layer. Write
`/out/status.json` = `{ exit_code, diff_id }`. Exit with the command's code.

On boot the helper deletes `/store/work/*` left by any interrupted run.

### VM setup (per job)

| Item | Value |
|---|---|
| Root | `krun_set_root(guest-root/)` — tiny host dir: helper binary, `etc/resolv.conf`, `etc/hosts`, `etc/hostname`, mount points. Case-insensitive APFS is irrelevant here. |
| Disk | `krun_add_disk(store.ext4, raw, rw)` → mounted at `/store` |
| virtio-fs | `blobs` (ro), `ctx` (ro, `copy` jobs only), `out` (rw, per-job dir under `jobs/<id>/out`) |
| Network | TSI (libkrun default) |
| Resources | vCPUs = host CPUs, RAM 2 GiB default; both overridable by flags |
| Exec | `/sandcastle-guest /out/job.json` |

`guest-root/sandcastle-guest` is refreshed from the installed helper (next to
the `sandcastle` executable, or `$SANDCASTLE_GUEST`) when its hash differs.

### Exit code handling

libkrun's init reserves 125/126/127, which collide with real shell exit codes
(127 = command not found). The host treats `status.json` as authoritative. If
it is absent, the helper never committed and the step fails with the child
process exit code and "VM or helper setup failed".

### Landlock (Linux host only)

In the `__vm` child, after all `krun_*` configuration calls and immediately
before `krun_start_enter`: set `PR_SET_NO_NEW_PRIVS`, then restrict the
filesystem to read-write `/dev/kvm`, `store.ext4`, the job `out` dir; read-only
`guest-root/`, `blobs/`, the build context, the libkrun/libkrunfw libraries.
Network stays open (TSI proxies guest connects from the host process). Use the
best ABI available; if the kernel lacks Landlock, log a warning and continue.
Paths libkrun opens lazily inside `krun_start_enter` must be found during
implementation and added to the allow list — the integration test catches
omissions. macOS has no equivalent and gets no host-side sandbox in v1.

## Data flow

1. Parse Dockerfile → `Vec<Instruction>`; first must be `FROM`.
2. Pull base: manifest, config, layer blobs → `blobs/`. Seed config state
   (env, cmd, entrypoint, workdir, user, labels, ports) and `diff_ids` from the
   base config.
3. `unpack` job for the base diff_ids (skipped when all are present).
4. For each instruction:
   - `ENV`, `WORKDIR`, `USER`, `CMD`, `ENTRYPOINT`, `LABEL`, `EXPOSE`: update
     config state, add a history entry with `empty_layer: true`.
   - `RUN`, `COPY`: write job (lower stack, env, user, workdir, command or
     copy spec), run VM, read `status.json`, gzip + hash `layer.tar` into
     `blobs/`, verify the host diff_id equals the guest's, append diff_id and
     history.
5. Write config blob, manifest blob, `index.json`, `oci-layout` into `-o`.

## Error handling

`anyhow` in both binaries; typed `thiserror` errors in the `sandcastle-dockerfile` library, with context on each step (`step 4/9 RUN apt-get …:
exited with 100`). A failed step discards its work dir and aborts the build;
blobs and committed layers stay in the store. Unsupported instructions fail at
parse time before any pull. Missing bundled libraries, `/dev/kvm` access or
hypervisor entitlement are detected upfront with a message naming the fix.

## Build and platform notes

### Bundled libraries

Upstream publishes no libkrun binaries and only Linux libkrunfw binaries, so
CI builds both from pinned upstream tags and stores them as release inputs:

| Platform | libkrunfw | libkrun |
|---|---|---|
| macOS arm64 | built from the `libkrunfw-prebuilt-aarch64.tgz` release asset (pre-generated kernel bundle, no kernel compile) | built from tag on a macOS runner |
| Linux x86_64 / aarch64 | `libkrunfw-<arch>.tgz` release asset | built from tag on a native runner of that arch, on an old-glibc base so the `.so` runs on older distros |

A `lib/PROVENANCE` file records the upstream tags and sha256 of every bundled
file; CI checks it. libkrunfw embeds a Linux kernel (GPL-2.0): the release
notes link the exact libkrunfw source tag, and `NOTICE` lists all bundled
licenses.

The ext4 store template is produced in Linux CI with
`mke2fs -t ext4 -E lazy_itable_init=1,lazy_journal_init=1` on a 64 GiB sparse
file, then zstd-compressed (expected well under a few MB, since
uninitialised tables are zeros).

For local development, `just fetch-libs` downloads the bundled libraries of
the latest CI build into `lib/`.

### Toolchain

- Guest helper: `cargo build -p sandcastle-guest --target
  <arch>-unknown-linux-musl` with `rust-lld` as linker (pure-Rust deps only,
  no C) so it cross-builds from macOS.
- macOS: Hypervisor.framework requires the `com.apple.security.hypervisor`
  entitlement; the `sandcastle` binary is ad-hoc signed with it after build.
  If it is later signed with the hardened runtime, it also needs
  `com.apple.security.cs.disable-library-validation` to load the bundled
  dylibs.
- A `justfile` wraps: build helper + host + sign, unit tests, integration
  tests, benchmark, conformance.

## Testing

- **Unit (fast, macOS + Linux, `cargo test`)**: BuildKit parser fixtures
  (positive + negative); `Node` → `Instruction` conversion and errors; variable expansion; image config/history/manifest
  assembly from a given state; whiteout conversion both directions and
  overlay ↔ OCI tar round-trip on fixture trees; `COPY` source resolution
  rules. Guest pure logic (whiteouts, copy rules) is kept free of
  Linux-only APIs so it tests on macOS.
- **Integration (`just it`, needs libkrun + hypervisor + network; run
  explicitly)**: build fixture Dockerfiles from `alpine` and `debian`
  exercising every v1 instruction, including a file deleted in a `RUN` (whiteout),
  `USER` non-root, `RUN` exit 127, and a failing command. Assert by running the
  result with podman / inspecting the OCI layout with `oci-spec`, not by
  re-reading our own data structures.
- **Conformance (imagebuilder) and benchmark: manual only**, documented in
  README, never run automatically.

## Benchmark vs BuildKit (manual)

`bench/` with `hyperfine`: Dockerfiles for alpine `apk add`, debian `apt-get`,
a Go build, 50 trivial `RUN`s, a large `COPY` context. Cold and warm runs.
Base images served from a local registry mirror. Both tools output an OCI
layout (`docker buildx build --output type=oci`). BuildKit native on Linux
(rootful and rootless), via Docker Desktop on macOS — noted in results.
Single-stage v1 has no cross-build cache, so warm comparisons disable the
BuildKit cache (`--no-cache`) and report pull-cache effects separately.

## Risks

| Risk | Mitigation |
|---|---|
| Parser port drifts from BuildKit over time | Fixtures are vendored at a pinned BuildKit commit; refresh them when adding instructions |
| libkrun lazily opens files that Landlock blocks | Integration test on Linux; extend allow list |
| `krun_add_disk` / virtio-fs behaviour differs on macOS HVF | First integration milestone runs on both platforms before building further |
| Overlay on ext4 inside libkrunfw kernel lacks `lowerdir+` (needs ≥ 6.8) | Check kernel version at helper start; fall back to relative short lowerdir paths |
| Building libkrun for macOS in CI is fiddly (guest `init` cross-build, libclang) | First plan milestone produces the bundle on all three platforms before other VM work; smolvm's `scripts/build-dist.sh` is the reference |
| Linux glibc floor of bundled `libkrun.so` | Build on an old-glibc image; CI checks the max `GLIBC_` symbol version |
