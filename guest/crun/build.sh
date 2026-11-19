#!/usr/bin/env bash
# Build a static crun for one guest architecture, with the json-c, libseccomp and libcap it links, all from pinned
# release tarballs:
#
#   <out>/crun                    the static binary
#   <out>/licenses/<name>/...     each component's license files
#   <out>/licenses/SOURCES.json   name, version, license and source tarball (URL, sha256) of each
#   <out>/CRUN.txt                what was built and from what
#
# Usage: guest/crun/build.sh [x86_64|aarch64] [out-dir] [work-dir]
#
# Defaults: the build host's architecture, guest/out/crun-<arch>, guest/work. Builds natively only, on Linux, with
# the toolchain and 2.39 sysroot from `pixi run -e crun-build`.
set -euo pipefail

CRUN_VERSION=1.30.1
# From the release's CHECKSUMS (https://github.com/containers/crun/releases/tag/1.30.1).
CRUN_SHA256=99d036d95113552b6df66c7c879432835504bf0e525fc0372745071df129527b
LIBSECCOMP_VERSION=2.6.1
# From the release's libseccomp-2.6.1.tar.gz.SHA256SUM (https://github.com/seccomp/libseccomp/releases).
LIBSECCOMP_SHA256=501f66c667225d53791b97e1d7cf85ab764c297d04881f60f38f451c4b0ee1be
JSON_C_VERSION=0.18
# json-c publishes no checksum file; this is the tarball's sha256 as distributions pin it (Arch, Alpine, Fedora).
JSON_C_SHA256=876ab046479166b869afc6896d288183bbc0e5843f141200c677b3e8dfb11724
LIBCAP_VERSION=2.78
# From https://cdn.kernel.org/pub/linux/libs/security/linux-privs/libcap2/sha256sums.asc
LIBCAP_SHA256=0d621e562fd932ccf67b9660fb018e468a683d7b827541df27813228c996bb11

CRUN_URL="https://github.com/containers/crun/releases/download/$CRUN_VERSION/crun-$CRUN_VERSION.tar.gz"
LIBSECCOMP_URL="https://github.com/seccomp/libseccomp/releases/download/v$LIBSECCOMP_VERSION/libseccomp-$LIBSECCOMP_VERSION.tar.gz"
JSON_C_URL="https://s3.amazonaws.com/json-c_releases/releases/json-c-$JSON_C_VERSION.tar.gz"
LIBCAP_URL="https://cdn.kernel.org/pub/linux/libs/security/linux-privs/libcap2/libcap-$LIBCAP_VERSION.tar.xz"

arch=${1:-$(uname -m)}
out=${2:-$(pwd)/guest/out/crun-$arch}
work=${3:-$(pwd)/guest/work}

case "$arch" in
  x86_64 | aarch64) ;;
  *) echo "unsupported architecture: $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != "$arch" ]; then
  echo "crun for $arch builds natively on $arch Linux only (this is $(uname -s) $(uname -m))" >&2
  exit 1
fi
prefix=${CONDA_PREFIX:?run under pixi run -e crun-build}
host="$arch-conda-linux-gnu"
if [ ! -x "$prefix/bin/$host-gcc" ]; then
  echo "no $host-gcc in $prefix/bin: run under pixi run -e crun-build" >&2
  exit 1
fi

mkdir -p "$out" "$work"
out=$(cd "$out" && pwd)
work=$(cd "$work" && pwd)
build="$work/crun-$arch"
deps="$build/deps"
rm -rf "$build"
mkdir -p "$build" "$deps"

# Downloads $1 to $work (unless it is there already with the right sha256), checks it against $2, unpacks it into
# $build and prints the folder it made.
fetch() {
  local url=$1 sha=$2 file
  file="$work/$(basename "$url")"
  if [ ! -f "$file" ] || [ "$(sha256sum "$file" | cut -d' ' -f1)" != "$sha" ]; then
    curl -fsSL --retry 5 -o "$file" "$url"
  fi
  local actual
  actual=$(sha256sum "$file" | cut -d' ' -f1)
  if [ "$actual" != "$sha" ]; then
    echo "$(basename "$file"): sha256 $actual, expected $sha" >&2
    exit 1
  fi
  tar -xf "$file" -C "$build"
  local name
  name=$(basename "$file")
  name=${name%.tar.*}
  echo "$build/$name"
}

# conda-forge's gcc against the 2.39 sysroot, nothing from the build host. The environment's flags are for shared
# conda packages (-fPIC, rpaths); a static binary needs its own.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
export CC="$host-gcc" AR="$host-ar" RANLIB="$host-ranlib"
export CFLAGS="-O2 -g0 -ffile-prefix-map=$build=."
export PKG_CONFIG_PATH="$deps/lib/pkgconfig" PKG_CONFIG_LIBDIR="$deps/lib/pkgconfig"
jobs=$(nproc)

libseccomp=$(fetch "$LIBSECCOMP_URL" "$LIBSECCOMP_SHA256")
(
  cd "$libseccomp"
  ./configure --host="$host" --prefix="$deps" --libdir="$deps/lib" --enable-static --disable-shared \
    --disable-python
  make -j"$jobs"
  make install
)

json_c=$(fetch "$JSON_C_URL" "$JSON_C_SHA256")
(
  cmake -S "$json_c" -B "$json_c/build" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$deps" \
    -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=OFF -DBUILD_STATIC_LIBS=ON -DBUILD_TESTING=OFF \
    -DDISABLE_EXTRA_LIBS=ON -DBUILD_APPS=OFF -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_C_COMPILER="$CC"
  cmake --build "$json_c/build" -j"$jobs"
  cmake --install "$json_c/build"
)

libcap=$(fetch "$LIBCAP_URL" "$LIBCAP_SHA256")
(
  cd "$libcap"
  # Only the static library and its headers: no Go bindings, no PAM module, no tools.
  make -C libcap -j"$jobs" CC="$CC" BUILD_CC="$CC" AR="$AR" RANLIB="$RANLIB" SHARED=no GOLANG=no PAM_CAP=no \
    prefix="$deps" lib=lib DESTDIR= install-static
)

crun=$(fetch "$CRUN_URL" "$CRUN_SHA256")
(
  cd "$crun"
  # No systemd (cgroups are managed with cgroupfs), CRIU, dlopen'd handlers or shared libcrun: the guest has none of
  # them.
  ./configure --host="$host" --prefix="$build/install" --enable-static --disable-shared --disable-systemd \
    --disable-criu --disable-dl \
    CPPFLAGS="-I$deps/include" LDFLAGS="-L$deps/lib"
  # The default target, not just `crun`: it also makes git-version.h, a built source crun.c includes. libtool links
  # fully statically only with its own -all-static; plain -static still leaves the C library shared.
  make -j"$jobs" LDFLAGS="-L$deps/lib -all-static -s"
)

# A static binary has no program interpreter (dynamic loader).
if "$host-readelf" -l "$crun/crun" | grep -q INTERP; then
  echo "crun is not statically linked" >&2
  exit 1
fi
features=$("$crun/crun" --version)
for wanted in +CAP +SECCOMP; do
  if ! grep -q -- "$wanted" <<< "$features"; then
    echo "crun was built without $wanted:" >&2
    echo "$features" >&2
    exit 1
  fi
done

rm -rf "$out"
mkdir -p "$out/licenses/crun" "$out/licenses/json-c" "$out/licenses/libseccomp" "$out/licenses/libcap"
cp "$crun/crun" "$out/crun"
cp "$crun/COPYING" "$crun/COPYING.libcrun" "$out/licenses/crun/"
cp "$json_c/COPYING" "$json_c/AUTHORS" "$out/licenses/json-c/"
cp "$libseccomp/LICENSE" "$out/licenses/libseccomp/"
cp "$libcap/License" "$out/licenses/libcap/"
cat > "$out/licenses/SOURCES.json" << EOF
[
  {
    "name": "crun",
    "version": "$CRUN_VERSION",
    "license": "GPL-2.0-or-later (libcrun: LGPL-2.1-or-later)",
    "from": "https://github.com/containers/crun, built statically by guest/crun/build.sh",
    "sources": [{ "url": "$CRUN_URL", "sha256": "$CRUN_SHA256" }],
    "patches": []
  },
  {
    "name": "json-c",
    "version": "$JSON_C_VERSION",
    "license": "MIT (statically linked into crun)",
    "from": "https://github.com/json-c/json-c, built by guest/crun/build.sh",
    "sources": [{ "url": "$JSON_C_URL", "sha256": "$JSON_C_SHA256" }],
    "patches": []
  },
  {
    "name": "libcap",
    "version": "$LIBCAP_VERSION",
    "license": "BSD-3-Clause OR GPL-2.0-only (statically linked into crun)",
    "from": "https://sites.google.com/site/fullycapable/, built by guest/crun/build.sh",
    "sources": [{ "url": "$LIBCAP_URL", "sha256": "$LIBCAP_SHA256" }],
    "patches": []
  },
  {
    "name": "libseccomp",
    "version": "$LIBSECCOMP_VERSION",
    "license": "LGPL-2.1-only (statically linked into crun; relink from this repository's source)",
    "from": "https://github.com/seccomp/libseccomp, built by guest/crun/build.sh",
    "sources": [{ "url": "$LIBSECCOMP_URL", "sha256": "$LIBSECCOMP_SHA256" }],
    "patches": []
  }
]
EOF
{
  echo "crun_version=$CRUN_VERSION"
  echo "crun_sha256=$CRUN_SHA256"
  echo "json_c_version=$JSON_C_VERSION"
  echo "libseccomp_version=$LIBSECCOMP_VERSION"
  echo "libcap_version=$LIBCAP_VERSION"
  echo "arch=$arch"
  echo "binary_sha256=$(sha256sum "$out/crun" | cut -d' ' -f1)"
  echo "compiler=$("$CC" --version | head -1)"
  echo "features=$(tr '\n' ' ' <<< "$features")"
} > "$out/CRUN.txt"

cat "$out/CRUN.txt"
ls -l "$out/crun"
