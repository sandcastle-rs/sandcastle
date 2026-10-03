# shellcheck shell=bash disable=SC2034
# Shared benchmark settings. Sourced, not executed.
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
# The port also appears in bench/buildkitd.toml and in the FROM line of every
# bench/cases/*/Dockerfile (v1 Dockerfiles have no ARG before FROM); change all together.
REGISTRY_PORT=5001
REGISTRY_NAME=sandcastle-bench-registry
REGISTRY_IMAGE=docker.io/library/registry:2
BUILDKIT_NAME=sandcastle-bench-buildkitd
BUILDKIT_VOLUME=sandcastle-bench-buildkit-state
# Pinned release (matches the Homebrew buildctl); check it exists with
# `skopeo inspect --override-os linux docker://$BUILDKIT_IMAGE`.
BUILDKIT_VERSION=v0.33.1
BUILDKIT_IMAGE=docker.io/moby/buildkit:$BUILDKIT_VERSION
# All images are fully qualified, so skopeo uses a minimal v2 registries.conf
# instead of a stale system one (e.g. a v1 file from an old Homebrew install).
export CONTAINERS_REGISTRIES_CONF="${CONTAINERS_REGISTRIES_CONF:-$BENCH_DIR/registries.conf}"
# Only public images are pulled, anonymously; an empty auth file keeps a broken
# credential helper in the user's own config from failing pulls.
export REGISTRY_AUTH_FILE="${REGISTRY_AUTH_FILE:-$BENCH_DIR/auth.json}"
if [[ -z "${ENGINE:-}" ]]; then
    if [[ "$(uname -s)" == Darwin ]]; then ENGINE=podman; else ENGINE=docker; fi
fi
case "$(uname -m)" in
    arm64 | aarch64) ARCH=arm64 ;;
    x86_64 | amd64) ARCH=amd64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
