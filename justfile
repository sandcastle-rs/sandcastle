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
