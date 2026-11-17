#!/usr/bin/env bash
# Build the minimal guest kernel for one architecture:
#
#   <out>/bzImage (x86_64) or <out>/Image (aarch64)
#   <out>/config        the final .config
#   <out>/BUNDLE.txt    what was built and from what
#
# Usage: guest/kernel/build.sh [x86_64|aarch64] [out-dir] [work-dir]
#
# Defaults: the build host's architecture, guest/out/<arch>, guest/work. Builds natively only, on Linux, with the
# toolchain from `pixi run -e kernel-build`. The configuration is `make tinyconfig` with config/common.config and
# config/<arch>.config merged over it.
set -euo pipefail

KERNEL_VERSION=6.18.63
# From https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc
KERNEL_SHA256=81764a545072d160748beff4d9de2e7d95aa9f16e8261110288f067a2cc61d71
# Bump when the configuration or build changes without a new kernel release. It ends up in the kernel release
# string (uname -r) as -oci-hermit-<revision>.
KERNEL_REVISION=2

here=$(cd "$(dirname "$0")" && pwd)
arch=${1:-$(uname -m)}
out=${2:-$(pwd)/guest/out/$arch}
work=${3:-$(pwd)/guest/work}

case "$arch" in
  x86_64) karch=x86_64 image=arch/x86/boot/bzImage ;;
  aarch64) karch=arm64 image=arch/arm64/boot/Image ;;
  *) echo "unsupported architecture: $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != "$arch" ]; then
  echo "the $arch kernel builds natively on $arch Linux only (this is $(uname -s) $(uname -m))" >&2
  exit 1
fi
prefix=${CONDA_PREFIX:?run under pixi run -e kernel-build}
# conda-forge's gcc and binutils, under their target-prefixed names.
host="$arch-conda-linux-gnu"
if [ ! -x "$prefix/bin/$host-gcc" ] || [ ! -x "$prefix/bin/$host-ld" ]; then
  echo "no $host-gcc or $host-ld in $prefix/bin: run under pixi run -e kernel-build" >&2
  exit 1
fi

mkdir -p "$out" "$work"
out=$(cd "$out" && pwd)
work=$(cd "$work" && pwd)

tarball="$work/linux-${KERNEL_VERSION}.tar.xz"
if [ ! -f "$tarball" ] || [ "$(sha256sum "$tarball" | cut -d' ' -f1)" != "$KERNEL_SHA256" ]; then
  curl -fsSL -o "$tarball" "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_VERSION%%.*}.x/linux-${KERNEL_VERSION}.tar.xz"
fi
actual=$(sha256sum "$tarball" | cut -d' ' -f1)
if [ "$actual" != "$KERNEL_SHA256" ]; then
  echo "linux-${KERNEL_VERSION}.tar.xz: sha256 $actual, expected $KERNEL_SHA256" >&2
  exit 1
fi

src="$work/linux-${KERNEL_VERSION}"
if [ ! -f "$src/.oci-hermit-extracted" ]; then
  rm -rf "$src"
  tar -xJf "$tarball" -C "$work"
  touch "$src/.oci-hermit-extracted"
fi
build="$work/build-$arch"
mkdir -p "$build"

# The compilers come from conda-forge under their target-prefixed names (x86_64-conda-linux-gnu-gcc, ...). The
# kernel has its own flags; the environment's CFLAGS and LDFLAGS are for userspace and must not leak in.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
# Fixed build metadata, so the same sources and configuration give the same image. The timestamp is the release's,
# from the tarball's file times.
KBUILD_BUILD_TIMESTAMP=$(date -u -d "@$(stat -c %Y "$src/Makefile")" '+%Y-%m-%d %H:%M:%S UTC')
export KBUILD_BUILD_TIMESTAMP KBUILD_BUILD_USER=oci-hermit KBUILD_BUILD_HOST=oci-hermit KBUILD_BUILD_VERSION=1
kmake() {
  make -C "$src" O="$build" ARCH="$karch" CROSS_COMPILE="$host-" HOSTCC="$host-gcc" HOSTCXX="$host-g++" \
    HOSTCFLAGS="-I$prefix/include" HOSTLDFLAGS="-L$prefix/lib -Wl,-rpath,$prefix/lib" \
    LOCALVERSION="-oci-hermit-$KERNEL_REVISION" "$@"
}

fragments="$here/config/common.config $here/config/$arch.config"
kmake tinyconfig
# shellcheck disable=SC2086 # the fragment list is meant to split into words
"$src/scripts/kconfig/merge_config.sh" -m -O "$build" "$build/.config" $fragments
kmake olddefconfig

# Every option the fragments ask for must have survived: Kconfig silently drops one whose dependencies are unmet.
missing=0
for fragment in $fragments; do
  while IFS= read -r line; do
    case "$line" in
      CONFIG_*=*) grep -qxF "$line" "$build/.config" || { echo "$(basename "$fragment"): $line did not stick" >&2; missing=1; } ;;
      "# CONFIG_"*" is not set")
        option=${line#\# }
        option=${option%% *}
        if grep -q "^$option=" "$build/.config"; then
          echo "$(basename "$fragment"): $option is set, wanted it off" >&2
          missing=1
        fi
        ;;
    esac
  done < "$fragment"
done
[ "$missing" = 0 ] || exit 1

kmake -j"$(nproc)" "$(basename "$image")"

release=$(cat "$build/include/config/kernel.release")
rm -rf "$out"
mkdir -p "$out"
cp "$build/$image" "$out/"
cp "$build/.config" "$out/config"
name=$(basename "$image")

# The kernel's license (COPYING points at LICENSES/preferred/GPL-2.0 and the syscall note) and where its source is,
# for the release's THIRD-PARTY-LICENSES and source archives (see packaging/licenses.py for the format).
mkdir -p "$out/licenses/linux/LICENSES/preferred" "$out/licenses/linux/LICENSES/exceptions"
cp "$src/COPYING" "$out/licenses/linux/COPYING"
cp "$src/LICENSES/preferred/GPL-2.0" "$out/licenses/linux/LICENSES/preferred/GPL-2.0"
cp "$src/LICENSES/exceptions/Linux-syscall-note" "$out/licenses/linux/LICENSES/exceptions/Linux-syscall-note"
cat > "$out/licenses/SOURCES.json" << EOF
[
  {
    "name": "linux",
    "version": "$KERNEL_VERSION",
    "license": "GPL-2.0-only WITH Linux-syscall-note",
    "from": "https://kernel.org, built with this kernel's config (config, beside the image)",
    "sources": [
      {
        "url": "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_VERSION%%.*}.x/linux-${KERNEL_VERSION}.tar.xz",
        "sha256": "$KERNEL_SHA256"
      }
    ],
    "patches": []
  }
]
EOF
{
  echo "kernel_version=$KERNEL_VERSION"
  echo "kernel_sha256=$KERNEL_SHA256"
  echo "kernel_release=$release"
  echo "revision=$KERNEL_REVISION"
  echo "arch=$arch"
  echo "image=$name"
  echo "image_sha256=$(sha256sum "$out/$name" | cut -d' ' -f1)"
  echo "config_sha256=$(sha256sum "$out/config" | cut -d' ' -f1)"
  echo "compiler=$("$host-gcc" --version | head -1)"
} > "$out/BUNDLE.txt"

cat "$out/BUNDLE.txt"
ls -l "$out/$name"
