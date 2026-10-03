#!/usr/bin/env bash
# buildkitd in a container. `reset` gives a cold daemon (empty state volume).
set -euo pipefail
# shellcheck disable=SC1091
source "$(dirname "$0")/lib.sh"

mode="${2:-rootful}"
# One state volume per flavour: the two images mount state at different paths and users.
volume="$BUILDKIT_VOLUME-$mode"
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
        -v "$volume:$state" \
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
reset) stop; $ENGINE volume rm -f "$volume" >/dev/null 2>&1 || true; start ;;
stop) stop ;;
*) echo "usage: $0 start|reset|stop [rootful|rootless]" >&2; exit 2 ;;
esac
