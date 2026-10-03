#!/usr/bin/env bash
# Times sandcastle against BuildKit with hyperfine. Manual only; see bench/README.md.
set -euo pipefail
# shellcheck disable=SC1091
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
