#!/usr/bin/env bash
# Provisions an Ubuntu 24.04 machine with KVM for `just bench`. Run as a sudo
# user from inside a clone of the repo. Safe to re-run.
set -euo pipefail
# shellcheck disable=SC1091
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
sg kvm -c "SANDCASTLE_LIBKRUN_DIR='$REPO_DIR/lib' target/release/sandcastle doctor"
buildctl --version
echo "ready: log out and back in (kvm/docker groups), then: bench/registry.sh start && bench/registry.sh seed && just bench --tools sandcastle,buildkit,buildkit-rootless"
