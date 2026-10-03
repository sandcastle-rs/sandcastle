#!/usr/bin/env bash
# Local registry on localhost:$REGISTRY_PORT holding the benchmark base images.
set -euo pipefail
# shellcheck disable=SC1091
source "$(dirname "$0")/lib.sh"

case "${1:-}" in
start)
    if ! $ENGINE container inspect "$REGISTRY_NAME" >/dev/null 2>&1; then
        $ENGINE run -d --name "$REGISTRY_NAME" -p "127.0.0.1:$REGISTRY_PORT:5000" "$REGISTRY_IMAGE" >/dev/null
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
