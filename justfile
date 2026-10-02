set shell := ["bash", "-euo", "pipefail", "-c"]

guest_target := arch() + "-unknown-linux-musl"
platform := os() + "-" + arch()
lib_dir := justfile_directory() / "lib"
bin := justfile_directory() / "target/release/sandcastle"
it_bin := justfile_directory() / "target/it/sandcastle"

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
# `cargo test` rewrites target/release/sandcastle and drops its signature, so the
# tests get a signed copy (with the guest helper next to it) in target/it.
it: build
    mkdir -p target/it
    cp target/release/sandcastle target/release/sandcastle-guest target/it/
    {{ if os() == "macos" { "codesign --force --sign - --entitlements sandcastle.entitlements " + it_bin } else { "true" } }}
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_BIN={{it_bin}} cargo test --release -p sandcastle --test vm --test cli --test build -- --ignored --test-threads=1

# Registry tests: need network, lib/ (store template) and skopeo.
it-registry:
    cargo build --release -p sandcastle-guest --target {{guest_target}}
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_GUEST={{justfile_directory()}}/target/{{guest_target}}/release/sandcastle-guest cargo test --release -p sandcastle --test registry -- --ignored --test-threads=1

# Build the libkrun/libkrunfw/store-template bundle into lib/.
build-libs:
    scripts/build-libs.sh {{lib_dir}}

# Download the bundle of the latest CI run into lib/ (needs a GitHub remote).
fetch-libs:
    rm -rf {{lib_dir}}
    gh run download --name sandcastle-libs-{{platform}} --dir {{lib_dir}}
    cd {{lib_dir}} && shasum -a 256 -c SHA256SUMS

# Run a built image with podman (Linux CI): needs podman and skopeo.
it-podman: it
    SANDCASTLE_LIBKRUN_DIR={{lib_dir}} SANDCASTLE_BIN={{it_bin}} cargo test --release -p sandcastle --test podman -- --ignored --test-threads=1
