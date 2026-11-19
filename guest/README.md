# Guest

What runs inside the sandbox VM. The guest only ever sees QEMU's virtual devices, so it is the same on every host.

## Kernel

`kernel/build.sh` builds Linux **6.18.63**, the newest longterm (LTS) release on kernel.org, from the release
tarball, checked against a pinned sha256 (from kernel.org's signed `sha256sums.asc`). No distribution kernels.

The configuration is `make tinyconfig` with two fragments merged over it by `scripts/kconfig/merge_config.sh`, then
`make olddefconfig`. The build fails if any option a fragment asks for does not end up in the final `.config`.

| Fragment | What it adds |
| --- | --- |
| `kernel/config/common.config` | printk and earlycon; virtio-mmio with virtio-blk, virtio-net, virtio-console and virtio-rng; 9P with `NET_9P_FD` (bind mounts over virtio-serial); overlayfs, EROFS, tmpfs, devtmpfs (automounted), proc, sysfs, devpts; IPv4 and IPv6 (so `[::]` and `::1` work), loopback and unix sockets; mount, PID, net, IPC, UTS and user namespaces; cgroup v2 with the memory, cpu and pids controllers, and cgroup BPF for crun's device rules; seccomp; ELF and `#!` binaries; initramfs (gzip, zstd) |
| `kernel/config/x86_64.config` | 64-bit, 8250 serial console, CMOS RTC, ACPI and the PCI layer ACPI needs, for QEMU's `microvm` machine |
| `kernel/config/aarch64.config` | PL011 serial console and PL031 RTC, for QEMU's `virt` machine |

Everything is built in: no modules, graphics, sound, USB, wireless or real-hardware drivers, and no debug info.

The x86_64 kernel has the PCI layer although `microvm` has no PCI bus. Without it ACPI fails to load ("AE_BAD_PARAMETER,
During Region initialization"), so PnP-ACPI never sets up the serial port's interrupt: the kernel's own messages
still reach the console, but every write from userspace fails with EIO, which looks like a hung guest (#54).

| Architecture | Machine | Image | Console |
| --- | --- | --- | --- |
| x86_64 | `microvm` (firmware `bios-microvm.bin`) | `bzImage`, zstd-compressed | `console=ttyS0` |
| aarch64 | `virt`, `-cpu max` | `Image` | `console=ttyAMA0` |

Builds are reproducible: the build user, host, version counter and timestamp (the release's own) are fixed, and the
kernel release string is `6.18.63-oci-hermit-<revision>`. Bump `KERNEL_REVISION` in `build.sh` when the
configuration changes without a new kernel release.

### Output

```
guest/out/<arch>/bzImage or Image
guest/out/<arch>/config      the final .config
guest/out/<arch>/BUNDLE.txt  kernel version, tarball sha256, release, image and config sha256, compiler
```

### Building

Kernels build natively on Linux only (x86_64 on x86_64, aarch64 on aarch64), with the toolchain from the pixi
`kernel-build` environment:

```bash
pixi run -e kernel-build kernel-build                       # this machine's architecture, into guest/out/<arch>
pixi run -e kernel-build kernel-build aarch64 guest/out/aarch64
```

### Boot test

`kernel/boot-test.sh` boots a kernel under the bundled QEMU (`qemu/README.md`) with TCG, no initramfs and no root
disk, and passes when the console shows the kernel looking for init (`No working init found` or `Unable to mount root
fs`). It runs on any host the QEMU bundle runs on, so a kernel built in CI can be tested on macOS too:

```bash
pixi run -e qemu-build qemu-build                           # writes qemu/out
bash guest/kernel/boot-test.sh aarch64 qemu/out guest/out/aarch64/Image
```

`.github/workflows/kernel.yml` builds both architectures on GitHub-hosted Linux runners, boot-tests each, and uploads
the image, config and `BUNDLE.txt` as artifacts.

## crun

Each command runs in a container started by [crun](https://github.com/containers/crun). `crun/build.sh` builds it
statically for one architecture, with the libraries it links, all from release tarballs checked against pinned
sha256 sums:

| Component | Version | License | Why |
| --- | --- | --- | --- |
| crun | 1.30.1 | GPL-2.0-or-later (libcrun LGPL-2.1-or-later) | the OCI runtime |
| json-c | 0.18 | MIT | crun reads `config.json` with it |
| libseccomp | 2.6.1 | LGPL-2.1-only | seccomp filters (a profile comes with #102) |
| libcap | 2.78 | BSD-3-Clause or GPL-2.0-only | capabilities |

crun is configured without systemd (cgroups are managed through cgroupfs), CRIU, `dlopen`ed handlers or a shared
libcrun, and linked against the same conda-forge 2.39 sysroot as the agent, so the glibc source in the release's
source archive covers both. The build fails unless the binary is static and reports `+CAP` and `+SECCOMP`.

### Output

```
<out>/crun                    the static binary
<out>/licenses/<name>/        each component's license files
<out>/licenses/SOURCES.json   name, version, license and source tarball of each
<out>/CRUN.txt                versions, the binary's sha256, the compiler and crun's feature list
```

`packaging/licenses.py crun <guest-dir> <out>` merges the licenses and sources into a guest folder.

### Building

Natively on Linux only, like the kernel:

```bash
pixi run -e crun-build crun-build                           # this machine's architecture, into guest/out/crun-<arch>
pixi run -e crun-build crun-build aarch64 guest/out/crun-aarch64
```

The host puts crun in the initramfs at `/bin/crun`. At boot the agent moves its root onto a tmpfs, taking crun
along, because crun's `pivot_root` refuses an initramfs root.
