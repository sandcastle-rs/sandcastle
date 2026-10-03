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
