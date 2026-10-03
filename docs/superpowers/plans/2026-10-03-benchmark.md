# Benchmark vs BuildKit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A manual, repeatable benchmark (`just bench …`) that times `sandcastle build` against BuildKit on the same machine and writes hyperfine tables to `bench/results/`.

**Architecture:** `bench/` holds five Dockerfile cases and shell scripts:
- **Registry:** base images come from a local `registry:2` on `localhost:5000`, seeded once with skopeo, so cold runs measure the builders rather than the internet.
- **BuildKit:** runs as a `moby/buildkit` container, rootful or rootless, and is driven by `buildctl` with `--output type=oci,tar=false`. Both tools write an OCI layout directory.
- **Timing:** `bench/run.sh` runs hyperfine per case and mode (cold/warm) with one command per tool, so every table compares the tools directly.
- **Where it runs:** on Linux, a Hetzner dedicated server with KVM, using Docker Engine for the containers. On macOS, the user's podman machine runs the registry and buildkitd.

**Tech Stack:** bash, hyperfine, buildctl/moby/buildkit, registry:2, skopeo, python3 (context generator), Rust (one registry change).

**Spec:** `docs/superpowers/specs/2026-10-02-sandcastle-v1-design.md` (section "Benchmark vs BuildKit (manual)")

## Rulings (where this plan departs from the spec's wording)

1. **macOS BuildKit runs in the user's podman machine, not Docker Desktop.** Docker Desktop isn't installed. Both tools then pay for a Linux VM on the Mac. — User decision.
2. **The Linux numbers come from a Hetzner dedicated server**, not a cloud VM. Hetzner Cloud VMs have no `/dev/kvm`, and nested-virt VMs would skew sandcastle. — User decision.
3. **BuildKit is driven with `buildctl` against a `moby/buildkit` container**, not `docker buildx`. The same driver then works for rootful and rootless and on both OSes, and the output is an OCI directory (`tar=false`), as with sandcastle.
4. **sandcastle pulls over plain HTTP when the registry host is `localhost`, `127.0.0.1` or `::1`** (any port), and over HTTPS otherwise. This is Docker's default rule, and the local mirror needs it.
5. **Cold vs warm.**
   - **sandcastle cold** uses a fresh `SANDCASTLE_ROOT`, so the store disk is created and the base is pulled and unpacked.
   - **sandcastle warm** reuses the store. Blobs and unpacked layers are cached, and every step still runs, because sandcastle has no build cache.
   - **BuildKit cold** starts a fresh buildkitd with an empty state volume.
   - **BuildKit warm** keeps buildkitd and passes `--no-cache`, so the base content is cached and every step still runs.

   This matches the spec's "warm comparisons disable the BuildKit cache".
6. **Package downloads (apk/apt) still go to the internet** for both tools. That is noise, but the same noise for both. `bench/README.md` says so.

## Global Constraints

- **The benchmark is manual only.** No CI job runs it, and neither `just test` nor `just it` touches it. Never run the README conformance suite.
- Scripts are `bash` with `set -euo pipefail` and must pass `shellcheck` and `bash -n`. They must work with macOS's bash 3.2: no associative arrays, no `mapfile`.
- The Dockerfiles use only v1 instructions (no `ARG`, no multi-stage). Bases are referenced as `localhost:5000/library/<name>:<tag>`.
- **Rust code:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass. Host `unsafe` stays in `src/vm/krun.rs`.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017321bxeGVNoZLQXWRDroYq
  ```
  (If a subagent's own harness names a different model, that model-accurate line is accepted.)
- No meta-design docs in user-facing docs. `bench/README.md` is how-to only.

## Review Focus

1. **Seeding the registry for the wrong architecture.** `registry.sh seed` must copy the host's `linux/<arch>`, or sandcastle's platform check rejects the image. Test: Task 2 smoke run on arm64 macOS.
2. **buildkitd inside a container can't reach `localhost:5000`.** It needs `--network host` plus the `http = true` registry config. Test: Task 2 smoke run.
3. **A "cold" run that is actually warm**, for example a state volume that is not removed, or an old sandcastle root. The `reset` paths must delete state. Test: Task 2 checks `buildkit.sh reset` drops the volume, and the run log shows the base pull.
4. **Output dirs from the previous run reused**, so a run times "nothing to write". Every hyperfine `--prepare` removes the output dir. Test: Task 2 command lines.
5. **A Linux setup script that silently half-installs.** It must be idempotent, stop on the first error, and end by printing `sandcastle doctor` output and `buildctl --version`. Test: Task 3 `shellcheck`/`bash -n`, plus the first real run by the user.

---

## File Structure

```
src/registry.rs                        protocol_for(): plain HTTP for localhost registries
bench/
  README.md                            how to run (macOS + Hetzner), what cold/warm mean, caveats
  images.txt                           base images to mirror (source → mirror path)
  lib.sh                               shared settings (ports, names, pinned images, arch)
  registry.sh                          start | seed | stop the local registry
  buildkit.sh                          start | reset | stop buildkitd (rootful | rootless)
  run.sh                               hyperfine matrix → bench/results/<stamp>/
  gen-context.sh                       generates cases/large-copy/data (gitignored)
  setup-ubuntu.sh                      provisions a Hetzner dedicated server (Ubuntu 24.04)
  buildkitd.toml                       registry config for buildkitd
  cases/alpine-apk/Dockerfile
  cases/debian-apt/Dockerfile
  cases/go-build/{Dockerfile,go.mod,main.go,internal/text/text.go}
  cases/many-runs/Dockerfile
  cases/large-copy/Dockerfile
  results/.gitkeep
justfile                               `bench` recipe
.gitignore                             bench/results/*/ , bench/cases/large-copy/data/
```

---

### Task 1: Plain HTTP for localhost registries

**Files:**
- Modify: `src/registry.rs`

**Interfaces:**
- Produces: `registry::protocol_for(registry: &str) -> oci_client::client::ClientProtocol` (pure function, used by `pull_async`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/registry.rs`:

```rust
    #[test]
    fn localhost_registries_use_plain_http() {
        for host in ["localhost:5000", "localhost", "127.0.0.1:5000", "[::1]:5000"] {
            assert!(
                matches!(protocol_for(host), ClientProtocol::HttpsExcept(ref list) if list == &[host.to_string()]),
                "{host}"
            );
        }
    }

    #[test]
    fn other_registries_use_https() {
        for host in ["mirror.gcr.io", "localhost.example.com:5000", "10.0.0.5:5000", "registry-1.docker.io"] {
            assert!(matches!(protocol_for(host), ClientProtocol::Https), "{host}");
        }
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sandcastle --lib registry::tests::localhost`
Expected: compile error (`protocol_for` undefined).

- [ ] **Step 3: Implement**

In `src/registry.rs`, change the import to `use oci_client::client::{ClientConfig, ClientProtocol};`. Then add:

```rust
/// Docker's default: registries on the loopback interface speak plain HTTP,
/// everything else HTTPS. `registry` is a reference's resolved host[:port].
pub fn protocol_for(registry: &str) -> ClientProtocol {
    let host = match registry.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => registry.split(':').next().unwrap_or(registry),
    };
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        ClientProtocol::HttpsExcept(vec![registry.to_string()])
    } else {
        ClientProtocol::Https
    }
}
```

In `pull_async`, set the protocol on the client config:

```rust
    let client = Client::new(ClientConfig {
        protocol: protocol_for(reference.resolve_registry()),
        // The default resolver matches the host OS, which is `darwin` on macOS.
        platform_resolver: Some(Box::new(move |entries: &[ImageIndexEntry]| {
            select_platform(entries, &resolver_arch)
        })),
        ..Default::default()
    });
```

`127.0.0.1:5000` splits to `127.0.0.1`, and `starts_with("127.")` covers all of 127/8. `10.0.0.5` stays on HTTPS.

- [ ] **Step 4: Run the tests**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry.rs
git commit -m "Pull from localhost registries over plain HTTP

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017321bxeGVNoZLQXWRDroYq"
```

---

### Task 2: Bench cases and harness (macOS smoke run)

**Files:**
- Create: everything under `bench/` except `setup-ubuntu.sh`
- Modify: `justfile`, `.gitignore`

**Interfaces:**
- Consumes: `sandcastle build -t TAG -o DIR CONTEXT` with `SANDCASTLE_ROOT` and `SANDCASTLE_LIBKRUN_DIR`, and Task 1's localhost HTTP pulls.
- Produces:
  - `bench/run.sh [--cases LIST] [--modes LIST] [--tools LIST] [--runs N]`. Tools are `sandcastle`, `buildkit` (rootful) and `buildkit-rootless`.
  - `bench/registry.sh start|seed|stop`
  - `bench/buildkit.sh start|reset|stop rootful|rootless`
  - `ENGINE` env (`docker` | `podman`). If unset: `docker` on Linux, `podman` on macOS.

- [ ] **Step 1: Cases**

`bench/cases/alpine-apk/Dockerfile`:

```dockerfile
FROM localhost:5000/library/alpine:3.20
RUN apk add --no-cache curl git jq
```

`bench/cases/debian-apt/Dockerfile`:

```dockerfile
FROM localhost:5000/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git && rm -rf /var/lib/apt/lists/*
```

`bench/cases/go-build/Dockerfile`:

```dockerfile
FROM localhost:5000/library/golang:1.25
ENV CGO_ENABLED=0
WORKDIR /src
COPY . .
RUN go build -o /out/app .
```

`bench/cases/go-build/go.mod`:

```
module example.com/benchapp

go 1.25
```

`bench/cases/go-build/main.go`:

```go
package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"

	"example.com/benchapp/internal/text"
)

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]string{"greeting": text.Greeting(r.URL.Path)})
	})
	if len(os.Args) > 1 && os.Args[1] == "serve" {
		fmt.Println(http.ListenAndServe(":8080", nil))
	}
	fmt.Println(text.Greeting("bench"))
}
```

`bench/cases/go-build/internal/text/text.go`:

```go
package text

import "strings"

// Greeting builds a friendly greeting for name.
func Greeting(name string) string {
	return "hello, " + strings.Trim(name, "/")
}
```

`bench/cases/many-runs/Dockerfile` has the FROM line followed by exactly 50 RUN lines, `RUN echo 1 > /step-1` through `RUN echo 50 > /step-50`. Write the file out; do not generate it at run time:

```dockerfile
FROM localhost:5000/library/alpine:3.20
RUN echo 1 > /step-1
RUN echo 2 > /step-2
…
RUN echo 50 > /step-50
```

`bench/cases/large-copy/Dockerfile`:

```dockerfile
FROM localhost:5000/library/alpine:3.20
COPY data/ /data/
```

`bench/gen-context.sh` generates `bench/cases/large-copy/data`:
- 100 dirs × 100 files of 4 KiB, plus 20 files of 5 MiB: 10,020 files, about 140 MiB.
- The bytes are deterministic (seeded).
- It does nothing if the data already exists. `--force` regenerates.

```bash
#!/usr/bin/env bash
# Generates the build context for the large-copy case (deterministic bytes).
set -euo pipefail
dest="$(cd "$(dirname "$0")" && pwd)/cases/large-copy/data"
if [[ -d "$dest" && "${1:-}" != "--force" ]]; then
    echo "large-copy context exists: $dest (use --force to regenerate)"
    exit 0
fi
rm -rf "$dest"
python3 - "$dest" <<'PY'
import os, random, sys
root = sys.argv[1]
rng = random.Random(42)
for d in range(100):
    path = os.path.join(root, f"dir{d:03}")
    os.makedirs(path)
    for f in range(100):
        with open(os.path.join(path, f"file{f:03}.bin"), "wb") as out:
            out.write(rng.randbytes(4096))
big = os.path.join(root, "big")
os.makedirs(big)
for f in range(20):
    with open(os.path.join(big, f"blob{f:02}.bin"), "wb") as out:
        out.write(rng.randbytes(5 << 20))
PY
echo "generated $(find "$dest" -type f | wc -l | tr -d ' ') files in $dest"
```

- [ ] **Step 2: Shared settings, registry, buildkitd**

`bench/lib.sh` (sourced by the other scripts):

```bash
# Shared benchmark settings. Sourced, not executed.
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
REGISTRY_PORT=5000
REGISTRY_NAME=sandcastle-bench-registry
REGISTRY_IMAGE=docker.io/library/registry:2
BUILDKIT_NAME=sandcastle-bench-buildkitd
BUILDKIT_VOLUME=sandcastle-bench-buildkit-state
# Pin a release; check it exists with `skopeo inspect docker://$BUILDKIT_IMAGE`.
BUILDKIT_VERSION=v0.25.1
BUILDKIT_IMAGE=docker.io/moby/buildkit:$BUILDKIT_VERSION
if [[ -z "${ENGINE:-}" ]]; then
    if [[ "$(uname -s)" == Darwin ]]; then ENGINE=podman; else ENGINE=docker; fi
fi
case "$(uname -m)" in
    arm64 | aarch64) ARCH=arm64 ;;
    x86_64 | amd64) ARCH=amd64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
```

When implementing, set `BUILDKIT_VERSION` to the latest stable `moby/buildkit` release tag that `skopeo inspect` confirms exists. Use the same version for `buildctl` (Homebrew `buildkit`, or the release tarball in Task 3), and record it in the commit message.

`bench/images.txt`, one per line, as `<source> <mirror path>`:

```
docker://mirror.gcr.io/library/alpine:3.20 library/alpine:3.20
docker://mirror.gcr.io/library/debian:bookworm-slim library/debian:bookworm-slim
docker://mirror.gcr.io/library/golang:1.25 library/golang:1.25
```

`bench/registry.sh`:

```bash
#!/usr/bin/env bash
# Local registry on localhost:$REGISTRY_PORT holding the benchmark base images.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

case "${1:-}" in
start)
    if ! $ENGINE container inspect "$REGISTRY_NAME" >/dev/null 2>&1; then
        $ENGINE run -d --name "$REGISTRY_NAME" -p "$REGISTRY_PORT:5000" "$REGISTRY_IMAGE" >/dev/null
    fi
    $ENGINE start "$REGISTRY_NAME" >/dev/null
    for _ in $(seq 1 30); do
        curl -fsS "http://localhost:$REGISTRY_PORT/v2/" >/dev/null 2>&1 && exit 0
        sleep 1
    done
    echo "registry did not come up on localhost:$REGISTRY_PORT" >&2
    exit 1
    ;;
seed)
    while read -r src dst; do
        [[ -z "$src" ]] && continue
        echo "seeding $dst (linux/$ARCH)"
        skopeo copy --override-os linux --override-arch "$ARCH" --dest-tls-verify=false \
            "$src" "docker://localhost:$REGISTRY_PORT/$dst"
    done <"$BENCH_DIR/images.txt"
    ;;
stop)
    $ENGINE rm -f "$REGISTRY_NAME" >/dev/null 2>&1 || true
    ;;
*)
    echo "usage: $0 start|seed|stop" >&2
    exit 2
    ;;
esac
```

`bench/buildkitd.toml`:

```toml
[registry."localhost:5000"]
  http = true
  insecure = true
```

`bench/buildkit.sh`:

```bash
#!/usr/bin/env bash
# buildkitd in a container. `reset` gives a cold daemon (empty state volume).
set -euo pipefail
source "$(dirname "$0")/lib.sh"

mode="${2:-rootful}"
start() {
    local image="$BUILDKIT_IMAGE" args=(--privileged) extra=()
    local state=/var/lib/buildkit config=/etc/buildkit/buildkitd.toml
    if [[ "$mode" == rootless ]]; then
        # The rootless image runs as uid 1000 with per-user paths.
        image="$BUILDKIT_IMAGE-rootless"
        args=(--security-opt seccomp=unconfined --security-opt apparmor=unconfined --device /dev/fuse)
        extra=(--oci-worker-no-process-sandbox)
        state=/home/user/.local/share/buildkit
        config=/home/user/.config/buildkit/buildkitd.toml
    fi
    $ENGINE run -d --name "$BUILDKIT_NAME" --network host "${args[@]}" \
        -v "$BUILDKIT_VOLUME:$state" \
        -v "$BENCH_DIR/buildkitd.toml:$config:ro" \
        "$image" ${extra[@]+"${extra[@]}"} >/dev/null
    for _ in $(seq 1 60); do
        buildctl --addr "$ENGINE-container://$BUILDKIT_NAME" debug workers >/dev/null 2>&1 && return 0
        sleep 1
    done
    echo "buildkitd did not become ready" >&2
    return 1
}
stop() {
    $ENGINE rm -f "$BUILDKIT_NAME" >/dev/null 2>&1 || true
}

case "${1:-}" in
start) stop; start ;;
reset) stop; $ENGINE volume rm -f "$BUILDKIT_VOLUME" >/dev/null 2>&1 || true; start ;;
stop) stop ;;
*) echo "usage: $0 start|reset|stop [rootful|rootless]" >&2; exit 2 ;;
esac
```

- [ ] **Step 3: The runner**

`bench/run.sh`:

```bash
#!/usr/bin/env bash
# Times sandcastle against BuildKit with hyperfine. Manual only; see bench/README.md.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

cases="alpine-apk,debian-apt,go-build,many-runs,large-copy"
modes="cold,warm"
tools="sandcastle,buildkit"
runs=5
while [[ $# -gt 0 ]]; do
    case "$1" in
    --cases) cases="$2"; shift 2 ;;
    --modes) modes="$2"; shift 2 ;;
    --tools) tools="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    *) echo "usage: $0 [--cases LIST] [--modes cold,warm] [--tools sandcastle,buildkit,buildkit-rootless] [--runs N]" >&2; exit 2 ;;
    esac
done

sandcastle="$REPO_DIR/target/release/sandcastle"
[[ -x "$sandcastle" ]] || { echo "run \`just build\` first" >&2; exit 1; }
export SANDCASTLE_LIBKRUN_DIR="$REPO_DIR/lib"
work="$(mktemp -d "${TMPDIR:-/tmp}/sandcastle-bench.XXXXXX")"
trap 'rm -rf "$work"' EXIT
stamp="$(date +%Y%m%d-%H%M)-$(hostname -s)"
results="$BENCH_DIR/results/$stamp"
mkdir -p "$results"

"$BENCH_DIR/registry.sh" start
"$BENCH_DIR/gen-context.sh"

{
    echo "# Benchmark environment"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%MZ)"
    echo "- host: $(uname -srm)"
    echo "- cpu: $(if [[ "$(uname -s)" == Darwin ]]; then sysctl -n machdep.cpu.brand_string; else grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-; fi)"
    echo "- sandcastle: $(git -C "$REPO_DIR" rev-parse --short HEAD)"
    echo "- buildkit: $BUILDKIT_IMAGE via $ENGINE"
    echo "- hyperfine: $(hyperfine --version)"
    echo "- runs: $runs; modes: $modes; tools: $tools"
} >"$results/environment.md"

IFS=, read -r -a case_list <<<"$cases"
IFS=, read -r -a mode_list <<<"$modes"
IFS=, read -r -a tool_list <<<"$tools"

for case in "${case_list[@]}"; do
    context="$BENCH_DIR/cases/$case"
    for mode in "${mode_list[@]}"; do
        args=(--runs "$runs" --export-markdown "$results/$case-$mode.md" --export-json "$results/$case-$mode.json")
        [[ "$mode" == warm ]] && args+=(--warmup 1)
        for tool in "${tool_list[@]}"; do
            out="$work/$tool-$case"
            case "$tool" in
            sandcastle)
                root="$work/sandcastle-root"
                prepare="rm -rf '$out'"
                [[ "$mode" == cold ]] && prepare="rm -rf '$out' '$root'"
                cmd="SANDCASTLE_ROOT='$root' '$sandcastle' build -t bench -o '$out' '$context'"
                ;;
            buildkit | buildkit-rootless)
                flavour=rootful
                [[ "$tool" == buildkit-rootless ]] && flavour=rootless
                # Each run (re)starts its own flavour, so rootful and rootless can
                # share one container name; `reset` also drops the state volume.
                daemon=start
                [[ "$mode" == cold ]] && daemon=reset
                prepare="rm -rf '$out' && '$BENCH_DIR/buildkit.sh' $daemon $flavour"
                cmd="buildctl --addr $ENGINE-container://$BUILDKIT_NAME build --no-cache --frontend dockerfile.v0 --local context='$context' --local dockerfile='$context' --output type=oci,dest='$out',tar=false"
                ;;
            *) echo "unknown tool $tool" >&2; exit 2 ;;
            esac
            args+=(--command-name "$tool" --prepare "$prepare" "$cmd")
        done
        echo "== $case ($mode)"
        hyperfine "${args[@]}"
    done
done
"$BENCH_DIR/buildkit.sh" stop
echo "results: $results"
```

hyperfine pairs the N-th `--prepare` with the N-th command and requires as many prepares as commands. The script appends one `--command-name`, `--prepare` and command per tool, so that holds. In warm mode, restarting buildkitd in the prepare keeps its state volume (content stays cached), and the restart itself is not timed.

- [ ] **Step 4: justfile, gitignore, README**

`justfile`:

```just
# Benchmark against BuildKit (manual; see bench/README.md). Example: just bench --cases many-runs --runs 3
bench *args: build
    bench/run.sh {{args}}
```

`.gitignore` additions:

```
/bench/results/*/
/bench/cases/large-copy/data/
```

Add `bench/results/.gitkeep`. Results you want to keep are committed explicitly with `git add -f`.

`bench/README.md` is how-to only. Sections:
1. What it measures:
   - the five cases (one line each);
   - cold vs warm for each tool, as in Ruling 5;
   - package downloads hit the internet.
2. **macOS setup:**
   - `brew install hyperfine buildkit skopeo`;
   - a podman machine running (`podman machine start`), rootful if the rootful BuildKit mode is used (`podman machine set --rootful`);
   - `bench/registry.sh start && bench/registry.sh seed`;
   - `just bench`.
3. **Linux (Hetzner dedicated) setup:** `bench/setup-ubuntu.sh`, then the same seed and run steps. Rootful and rootless with `--tools sandcastle,buildkit,buildkit-rootless`.
4. **Reading results:** `bench/results/<stamp>/<case>-<mode>.md` plus `environment.md`. Commit a run with `git add -f`.
5. **Cleanup:** `bench/buildkit.sh stop; bench/registry.sh stop`.

- [ ] **Step 5: Lint and smoke run on this Mac**

Run:

```bash
shellcheck bench/*.sh && bash -n bench/*.sh
brew install hyperfine buildkit   # if missing; skopeo is already installed
podman machine start              # if not running
bench/registry.sh start && bench/registry.sh seed
just bench --cases many-runs,large-copy --runs 1
```

Expected:
- Both tools finish both modes.
- `bench/results/<stamp>/many-runs-cold.md` has a sandcastle row and a buildkit row.
- Both OCI outputs are valid (spot-check one with `skopeo inspect oci:<out>:bench`, or check `index.json` exists before the trap removes it).

If BuildKit rootful cannot run in the podman machine, use `buildkit-rootless` on macOS, document it in the README, and say so in the report. If something fails, fix the script; never drop a tool or case to make the run pass.

- [ ] **Step 6: Commit**

```bash
git add bench justfile .gitignore
git commit -m "Add a manual benchmark against BuildKit

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017321bxeGVNoZLQXWRDroYq"
```

---

### Task 3: Hetzner dedicated server setup

**Files:**
- Create: `bench/setup-ubuntu.sh`
- Modify: `bench/README.md` (Linux section points at it)

**Interfaces:**
- Consumes: `just build-libs`, `just build`, `sandcastle doctor`, `bench/registry.sh`, `bench/run.sh`.

- [ ] **Step 1: Write the script**

`bench/setup-ubuntu.sh` provisions a fresh Ubuntu 24.04 server as a non-root sudo user, inside a clone of this repo. It is idempotent and stops on the first error.

```bash
#!/usr/bin/env bash
# Provisions an Ubuntu 24.04 machine with KVM for `just bench`. Run as a sudo
# user from the repo root. Safe to re-run.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

[[ -e /dev/kvm ]] || { echo "/dev/kvm is missing: use a dedicated server, not a cloud VM" >&2; exit 1; }

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git python3 skopeo hyperfine docker.io \
    patchelf libc6-dev e2fsprogs zstd binutils clang libclang-dev lld fuse-overlayfs uidmap
sudo usermod -aG kvm,docker "$USER"

if ! command -v rustup >/dev/null; then
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup target add "$(uname -m)-unknown-linux-musl"
command -v just >/dev/null || cargo install --locked just

if ! command -v buildctl >/dev/null || [[ "$(buildctl --version)" != *"$BUILDKIT_VERSION"* ]]; then
    url="https://github.com/moby/buildkit/releases/download/$BUILDKIT_VERSION/buildkit-$BUILDKIT_VERSION.linux-$ARCH.tar.gz"
    curl -fsSL "$url" | sudo tar -xz -C /usr/local bin/buildctl
fi

cd "$REPO_DIR"
[[ -f lib/PROVENANCE ]] || just build-libs
just build
# New group membership applies to new logins; use sg for this check.
sg kvm -c "target/release/sandcastle doctor"
buildctl --version
echo "ready: log out and back in (kvm/docker groups), then: bench/registry.sh start && bench/registry.sh seed && just bench --tools sandcastle,buildkit,buildkit-rootless"
```

Compare the apt package list with the `libs` job in `.github/workflows/ci.yml` (Linux) and add anything the build needs that is missing. `sandcastle doctor` must run with `SANDCASTLE_LIBKRUN_DIR` unset: `just build` copies the guest next to the binary, and the libs are found through `lib/` next to the repo. If `Install::locate` cannot find `lib/` from `target/release/`, export `SANDCASTLE_LIBKRUN_DIR="$REPO_DIR/lib"` before `doctor`, as `run.sh` does.

- [ ] **Step 2: Lint**

Run: `shellcheck bench/setup-ubuntu.sh && bash -n bench/setup-ubuntu.sh`
Expected: clean. The script cannot run on macOS. The user runs it on the server, and that first run is the real test (Review Focus 5).

- [ ] **Step 3: Commit**

```bash
git add bench/setup-ubuntu.sh bench/README.md
git commit -m "Add Ubuntu setup script for running the benchmark on a dedicated server

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017321bxeGVNoZLQXWRDroYq"
```
