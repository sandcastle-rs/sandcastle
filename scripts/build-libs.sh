#!/usr/bin/env bash
# Build the bundled libkrun + libkrunfw + ext4 store template for this
# platform into the directory given as $1 (default: ./lib).
#
# macOS needs: brew install lld xz e2fsprogs zstd
# Linux needs: apt-get install patchelf libc6-dev e2fsprogs zstd binutils clang libclang-dev
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=libs.env
source "$here/libs.env"

out_arg="${1:-lib}"
rm -rf "$out_arg"
mkdir -p "$out_arg/licenses"
out="$(cd "$out_arg" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

os="$(uname -s)"
arch="$(uname -m)"
[ "$arch" = arm64 ] && arch=aarch64
fw_url="https://github.com/containers/libkrunfw/releases/download/$LIBKRUNFW_TAG"

fetch() { # url sha256 dest
    curl -fsSL "$1" -o "$3"
    echo "$2  $3" | shasum -a 256 -c - >/dev/null
}

ncpu() { getconf _NPROCESSORS_ONLN; }

# libkrunfw: the prebuilt tarball carries the kernel as kernel.c plus the
# GPL/LGPL license files; macOS compiles it, Linux uses the ready .so.
fetch "$fw_url/libkrunfw-prebuilt-aarch64.tgz" "$LIBKRUNFW_PREBUILT_AARCH64_SHA256" "$work/fw-prebuilt.tgz"
tar -xzf "$work/fw-prebuilt.tgz" -C "$work"
cp "$work/libkrunfw/LICENSE-GPL-2.0-only" "$work/libkrunfw/LICENSE-LGPL-2.1-only" "$out/licenses/"
case "$os" in
Darwin)
    make -C "$work/libkrunfw" -j"$(ncpu)"
    cp "$work/libkrunfw/libkrunfw.5.dylib" "$out/"
    ;;
Linux)
    case "$arch" in
    x86_64) fw_sha="$LIBKRUNFW_X86_64_SHA256" ;;
    aarch64) fw_sha="$LIBKRUNFW_AARCH64_SHA256" ;;
    *) echo "unsupported arch $arch" >&2; exit 1 ;;
    esac
    fetch "$fw_url/libkrunfw-$arch.tgz" "$fw_sha" "$work/fw.tgz"
    tar -xzf "$work/fw.tgz" -C "$work"
    cp -L "$work/lib/$arch-linux-gnu/libkrunfw.so.5" "$out/"
    ;;
*) echo "unsupported OS $os" >&2; exit 1 ;;
esac

# libkrun from the upstream source tag, with virtio-blk for the store disk.
fetch "https://github.com/containers/libkrun/archive/refs/tags/$LIBKRUN_TAG.tar.gz" "$LIBKRUN_SRC_SHA256" "$work/krun.tgz"
tar -xzf "$work/krun.tgz" -C "$work"
krun_src="$work/libkrun-${LIBKRUN_TAG#v}"
# krun-input's bindgen build script needs libclang; macOS gets it from the
# Xcode or the command line tools.
if [ "$os" = Darwin ]; then
    dev="$(xcode-select -p)"
    for d in "$dev/usr/lib" "$dev/Toolchains/XcodeDefault.xctoolchain/usr/lib"; do
        # The build scripts link libclang via @rpath, which cargo does not set.
        [ -e "$d/libclang.dylib" ] && export LIBCLANG_PATH="$d" RUSTFLAGS="-C link-arg=-Wl,-rpath,$d"
    done
fi
make -C "$krun_src" BLK=1 -j"$(ncpu)"
cp "$krun_src/LICENSE" "$out/licenses/LICENSE-libkrun"
case "$os" in
Darwin)
    cp "$krun_src/target/release/libkrun.${LIBKRUN_TAG#v}.dylib" "$out/libkrun.1.dylib"
    # Only system libraries may be referenced, or the bundle breaks off this machine.
    # (otool -L prints the file name, then the library's own install name.)
    if otool -L "$out/libkrun.1.dylib" | tail -n +3 | grep -vE '^\s+(/usr/lib/|/System/)'; then
        echo "libkrun.1.dylib links non-system libraries (listed above)" >&2; exit 1
    fi
    mke2fs="$(brew --prefix e2fsprogs)/sbin/mke2fs"
    ;;
Linux)
    cp "$krun_src/target/release/libkrun.so.${LIBKRUN_TAG#v}" "$out/libkrun.so.1"
    newest="$(objdump -T "$out/libkrun.so.1" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
    if [ "$(printf '%s\n%s\n' "$newest" "$GLIBC_MAX" | sort -V | tail -1)" != "$GLIBC_MAX" ]; then
        echo "libkrun.so.1 needs glibc $newest > $GLIBC_MAX" >&2; exit 1
    fi
    mke2fs=mke2fs
    ;;
esac

# Empty ext4 store template. Inode tables and journal are written as zeros
# (lazy_*_init=0) so the guest kernel never zeroes them later and inflates
# the sparse file; pack-sparse.py drops the zero chunks again.
dd if=/dev/zero of="$work/store.ext4" bs=1 count=0 seek="$STORE_DISK_BYTES" 2>/dev/null
"$mke2fs" -q -F -t ext4 -m 0 -E lazy_itable_init=0,lazy_journal_init=0,nodiscard "$work/store.ext4"
python3 "$here/pack-sparse.py" "$work/store.ext4" | zstd -q -19 -o "$out/store-template.ext4.zst"

{
    echo "libkrun $LIBKRUN_TAG source sha256 $LIBKRUN_SRC_SHA256 (https://github.com/containers/libkrun)"
    echo "libkrunfw $LIBKRUNFW_TAG (https://github.com/containers/libkrunfw/tree/$LIBKRUNFW_TAG)"
    echo "built on $os $arch with make BLK=1"
    echo "sha256 of every bundled file: see SHA256SUMS"
} >"$out/PROVENANCE"
(cd "$out" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | sort | xargs shasum -a 256 >SHA256SUMS)
ls -l "$out"
