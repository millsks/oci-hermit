#!/usr/bin/env bash
# Build the minimal, TCG-only QEMU that oci-hermit runs, as a self-contained bundle:
#
#   <out>/bin/qemu-system-{x86_64,aarch64}[.exe]
#   <out>/lib/            shared libraries it needs (Linux and macOS)
#   <out>/bin/*.dll       DLLs it needs (Windows)
#   <out>/share/qemu/     the firmware microvm boots with
#   <out>/BUNDLE.txt      what was built and from what
#
# Usage: qemu/build.sh <out-dir> [work-dir]
#
# Linux and macOS take their build dependencies from `pixi run -e qemu-build`; Windows builds in an MSYS2 UCRT64
# shell (see .github/workflows/qemu.yml). Written for bash 3.2, which macOS ships.
set -euo pipefail

QEMU_VERSION=11.1.3
QEMU_SHA256=5a8289c7940dc77d3873c81a6c29ba3afe1446fc9d36e77fa1658cdbecb84b22
# Bump when the configuration or bundling changes without a new QEMU release. The host tool checks this marker.
BUNDLE_REVISION=1
PKGVERSION="oci-hermit-qemu-${BUNDLE_REVISION}"

out=${1:?usage: qemu/build.sh <out-dir> [work-dir]}
work=${2:-$(pwd)/qemu/work}
mkdir -p "$out" "$work"
out=$(cd "$out" && pwd)
work=$(cd "$work" && pwd)

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | UCRT64* | CLANG64* | CYGWIN*) os=windows ;;
  *) echo "unsupported build host: $(uname -s)" >&2; exit 1 ;;
esac

sha256() {
  if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

tarball="$work/qemu-${QEMU_VERSION}.tar.xz"
if [ ! -f "$tarball" ] || [ "$(sha256 "$tarball")" != "$QEMU_SHA256" ]; then
  curl -fsSL -o "$tarball" "https://download.qemu.org/qemu-${QEMU_VERSION}.tar.xz"
fi
actual=$(sha256 "$tarball")
if [ "$actual" != "$QEMU_SHA256" ]; then
  echo "qemu-${QEMU_VERSION}.tar.xz: sha256 $actual, expected $QEMU_SHA256" >&2
  exit 1
fi

src="$work/qemu-${QEMU_VERSION}"
build="$work/build"
stamp="$build/.oci-hermit-stamp"

# Everything off except what a sealed, headless, software-emulated sandbox needs: TCG (built in), user-mode
# networking (slirp), and the device tree the aarch64 `virt` machine boots with (libfdt from the build
# environment: the release tarball does not include the dtc subproject). bindir and datadir are explicit
# because QEMU's Windows defaults put the executables at the top of the prefix. No hypervisor accelerators, display,
# audio, USB, tools, docs or guest agent. --disable-download keeps meson from fetching anything at build time.
configure_flags="--prefix=/ --bindir=bin --datadir=share --target-list=x86_64-softmmu,aarch64-softmmu \
--without-default-features --enable-slirp --enable-fdt=system --disable-download --disable-docs --disable-tools \
--disable-guest-agent --disable-werror"

# A build directory configured with exactly these sources and flags is reused, so iterating on bundling does not
# recompile QEMU. Anything else starts from a clean tree.
want="$QEMU_VERSION $PKGVERSION $configure_flags"
fresh=1
[ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ] && fresh=0
if [ "$fresh" = 1 ]; then
  rm -rf "$src" "$build"
  # roms/ (firmware sources) and tests/lcitool/libvirt-ci/ (upstream CI) are never built here, and their
  # symlinks cannot be created on Windows without developer mode. tests/lcitool/ itself stays: the Makefile
  # includes it.
  tar -xJf "$tarball" -C "$work" \
    --exclude="qemu-${QEMU_VERSION}/roms" --exclude="qemu-${QEMU_VERSION}/tests/lcitool/libvirt-ci"
  mkdir -p "$build"
  cd "$build"
  # shellcheck disable=SC2086 # the flags are meant to split into words
  "$src/configure" $configure_flags --with-pkgversion="$PKGVERSION"
  echo "$want" > "$stamp"
fi
cd "$build"

make -j"$(getconf _NPROCESSORS_ONLN 2> /dev/null || echo 4)"

exe=""
[ "$os" = windows ] && exe=".exe"

rm -rf "$out"
mkdir -p "$out"
make install DESTDIR="$out" > /dev/null

# On Windows the `/` prefix resolves to the MSYS2 root, so DESTDIR nests the install under that path
# (out/a/_temp/msys64/...). Wherever it landed, the directory holding bin/ becomes the bundle.
installed=$(find "$out" -path "*/bin/qemu-system-x86_64$exe" | head -n 1)
if [ -z "$installed" ]; then
  echo "make install produced no bin/qemu-system-x86_64$exe under $out:" >&2
  find "$out" -maxdepth 6 >&2
  exit 1
fi
root=$(dirname "$(dirname "$installed")")
if [ "$root" != "$out" ]; then
  stage="$work/stage"
  rm -rf "$stage"
  mv "$root" "$stage"
  rm -rf "$out"
  mv "$stage" "$out"
fi
echo "installed:"
find "$out" -maxdepth 2 | sed "s|^$out|  .|"

# Keep only the firmware oci-hermit boots: qboot for x86 microvm. aarch64 `virt` boots the kernel directly.
# Linux and macOS install firmware to share/qemu/, Windows to share/ itself; either way it ends up in share/qemu/.
keep="bios-microvm.bin"
found=$(find "$out/share" -name "$keep" | head -n 1)
if [ -z "$found" ]; then
  echo "make install produced no $keep under $out/share" >&2
  exit 1
fi
mv "$found" "$work/$keep"
rm -rf "$out/share"
mkdir -p "$out/share/qemu"
mv "$work/$keep" "$out/share/qemu/$keep"
rm -rf "$out/include" "$out/libexec" "$out/var"
# Nothing QEMU installs under lib/ is needed at run time; bundle_libs fills it with what is.
rm -rf "$out/lib"

binaries="$out/bin/qemu-system-x86_64$exe $out/bin/qemu-system-aarch64$exe"

# Copy every shared library the binaries load from the build environment into the bundle, so it runs anywhere.
bundle_libs() {
  case "$os" in
    linux)
      prefix=${CONDA_PREFIX:?run under pixi run -e qemu-build}
      mkdir -p "$out/lib"
      for bin in $binaries; do
        ldd "$bin" | awk '/=> \// {print $3}' | while read -r lib; do
          case "$lib" in "$prefix"/*) cp -L "$lib" "$out/lib/" ;; esac
        done
      done
      for lib in "$out"/lib/*; do patchelf --set-rpath '$ORIGIN' "$lib"; done
      for bin in $binaries; do patchelf --set-rpath '$ORIGIN/../lib' "$bin"; done
      ;;
    macos)
      prefix=${CONDA_PREFIX:?run under pixi run -e qemu-build}
      mkdir -p "$out/lib"
      # Walk the dependency graph until no new library from the environment turns up.
      pending="$binaries"
      while [ -n "$pending" ]; do
        next=""
        for file in $pending; do
          for dep in $(otool -L "$file" | awk 'NR > 1 {print $1}'); do
            case "$dep" in
              @rpath/*) path="$prefix/lib/${dep#@rpath/}" ;;
              "$prefix"/*) path="$dep" ;;
              *) continue ;;
            esac
            name=$(basename "$dep")
            if [ ! -f "$out/lib/$name" ]; then
              cp -L "$path" "$out/lib/$name"
              chmod u+w "$out/lib/$name"
              next="$next $out/lib/$name"
            fi
          done
        done
        pending=$next
      done
      relink() {
        for dep in $(otool -L "$1" | awk 'NR > 1 {print $1}'); do
          case "$dep" in
            @rpath/* | "$prefix"/*) install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$1" 2> /dev/null ;;
          esac
        done
        for rpath in $(otool -l "$1" | awk '/LC_RPATH/ {getline; getline; print $2}'); do
          install_name_tool -delete_rpath "$rpath" "$1" 2> /dev/null || true
        done
        install_name_tool -add_rpath "$2" "$1"
      }
      for lib in "$out"/lib/*; do
        install_name_tool -id "@rpath/$(basename "$lib")" "$lib"
        relink "$lib" "@loader_path"
      done
      for bin in $binaries; do relink "$bin" "@executable_path/../lib"; done
      # install_name_tool invalidates signatures; Apple silicon refuses to run unsigned code. Ad-hoc sign again.
      for file in "$out"/lib/* $binaries; do codesign --force --sign - "$file"; done
      ;;
    windows)
      prefix=${MINGW_PREFIX:?run in an MSYS2 MINGW/UCRT shell}
      for bin in $binaries; do
        ldd "$bin" | awk '/=> \// {print $3}' | while read -r dll; do
          case "$dll" in "$prefix"/*) cp -u "$dll" "$out/bin/" ;; esac
        done
      done
      ;;
  esac
}
bundle_libs

{
  echo "qemu_version=$QEMU_VERSION"
  echo "qemu_sha256=$QEMU_SHA256"
  echo "pkgversion=$PKGVERSION"
  echo "os=$os"
  echo "arch=$(uname -m)"
  echo "targets=x86_64-softmmu,aarch64-softmmu"
} > "$out/BUNDLE.txt"

for bin in $binaries; do "$bin" --version | head -1; done
