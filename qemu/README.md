# Bundled QEMU

oci-hermit runs its own build of QEMU, never one already on the machine. conda-forge has no system-emulation QEMU
for macOS or Windows (its Linux packages are user-mode `qemu-execve-*` only), and distribution builds carry
hypervisor accelerators, displays, USB and more that the sandbox does not want.

## What is built

`build.sh` builds QEMU **11.1.3** from the release tarball on download.qemu.org, checked against a pinned sha256,
with:

- targets `x86_64-softmmu` and `aarch64-softmmu` only
- `--without-default-features`: no KVM, HVF, WHPX or Xen, no display, audio, USB, tools, docs or guest agent
- no debug info, which with GCC (Linux, MinGW) tripled the bundle's size
- user-mode networking (libslirp) and libfdt from the build environment, with `--disable-download` so nothing is
  fetched at build time
- the version marker `--with-pkgversion=oci-hermit-qemu-<revision>`

The host tool refuses any QEMU whose `--version` does not report exactly `11.1.3 (oci-hermit-qemu-<revision>)`, or
that has no TCG accelerator (`crates/oci-hermit/src/qemu.rs`). Changing the configuration means bumping
`BUNDLE_REVISION` in `build.sh` and `BUNDLE_MARKER` in `qemu.rs` together.

## Where each platform's build comes from

| Platform | Built on | Build dependencies from |
| --- | --- | --- |
| linux-64, linux-aarch64 | GitHub-hosted Ubuntu runners | conda-forge, via `pixi run -e qemu-build` |
| osx-arm64, osx-64 | GitHub-hosted macOS runners | conda-forge, via `pixi run -e qemu-build` |
| win-64 | GitHub-hosted Windows runner | MSYS2 UCRT64 (conda-forge has no libslirp for win-64, and QEMU does not build with MSVC), plus `qemu.qmp==0.0.6` from PyPI, which MSYS2 does not package |

`.github/workflows/qemu.yml` builds all five, checks each with `oci-hermit machine info`, and uploads them as
artifacts. Release packaging and signing come later (#39, #40, #41).

## Bundle layout

```
bin/qemu-system-x86_64[.exe]
bin/qemu-system-aarch64[.exe]
bin/*.dll                 Windows: the DLLs QEMU loads
lib/                      Linux and macOS: the shared libraries QEMU loads, found through its rpath
share/qemu/bios-microvm.bin   microvm's SeaBIOS
share/qemu/linuxboot_dma.bin  the option ROM SeaBIOS needs to boot a -kernel bzImage
BUNDLE.txt                version, tarball sha256, marker, host
```

The bundle is relocatable: libraries are found through the binaries' rpath, and the host tool always passes
`-L <bundle>/share/qemu` for firmware, because QEMU's own relocation does not apply to a `/` prefix.

The host tool looks for the bundle, in order, in `$OCI_HERMIT_QEMU_DIR`, `<prefix>/libexec/oci-hermit/qemu` beside a
prefix install of `oci-hermit`, and `qemu/` next to the executable.

## Building locally

```bash
pixi run -e qemu-build qemu-build       # Linux or macOS; writes qemu/out
OCI_HERMIT_QEMU_DIR=$PWD/qemu/out pixi run cargo run -p oci-hermit -- machine info
```
