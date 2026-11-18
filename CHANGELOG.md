# Changelog

Each release's downloads are on its [GitHub release page](https://github.com/millsks/oci-hermit/releases).

## 0.2.0 (2026-11-18)

The host and guest now speak control protocol 1.3, and `oci-hermit` refuses an older guest agent. Replace the whole
`oci-hermit` folder; do not copy the new `oci-hermit` or `hrmt` over a 0.1.0 install. The image store from 0.1.0
keeps working; layer disks are built into it the first time each image runs.

### Features

- `run` runs a pulled or loaded image from its layer disks, stacked with overlayfs under a writable layer that is
  thrown away when the run ends, so an image of any size runs in 512 MiB instead of twice its size in memory. An
  image run straight from a path, or one with more than 20 layers, still goes into memory (#98, #17)
- Each image layer is built once into a read-only EROFS disk and kept in the store; `rmi` and `image prune` remove
  the disks with their layers, and `system df` counts them (#97, #16)
- `manifest inspect` shows an image's platforms and digests without pulling it (#95, #76)
- Per-registry CA certificates and client certificates for `pull`, `login` and `manifest inspect`, from Podman's
  and Docker's `certs.d` folders or `--cert-dir`, without admin rights; `machine info` lists the folders it reads
  (#94, #86)

### Bug fixes

- The release's source archive falls back to verified mirrors when a download host fails (#92)
- The Windows CI package step no longer fails with "Permission denied" (#96)

### Documentation

- Getting started shows how to check the Windows download and what to do when a browser download never finishes
  (#92)
- The private registry field manual covers registries with their own CA or client certificates (#94)

### Refactoring

- `pull` uses oci-hermit's own registry client on reqwest instead of oci-client, which drops the unmaintained
  `chrono` dependency and its advisory exception (#93, #91)

## 0.1.0 (2026-11-17)

The first pre-release.

### Features

- Releases: one download per host with `oci-hermit`, the `hrmt` alias, QEMU and both guests, their licenses, and a
  separate archive with the source of every GPL and LGPL component (#89)
- `login` and `logout`, checked with the registry and stored in Podman's `auth.json` (#88)
- Image commands: `images` (`image ls`), `rmi` (`image rm`), `inspect`, `tag`, `save`, `image prune`,
  `image history`, `image exists`, `system df` and `info` (#85)
- `machine info` shows the guest, the defaults `run` uses, the image store and the registry auth files it reads
  (#74)
- `run` runs an image built for the other architecture when the image has no variant for the host's, with a warning
  (#72)
- `load -i` reads `docker save` archives (old and Docker 25+) and OCI layouts, offline (#71)
- `pull` pulls images from registries, with Podman and Docker logins, and verifies every digest (#70)
- `run` runs the command as the image's `User`, and `-u`/`--user` picks another (#68)
- `run` reports how long each phase took with `OCI_HERMIT_TIMINGS=1` (#64)
- `run` runs a command from a local OCI image, with `-i` and `-t` (#58)
- The control channel between the host tool and the guest agent (#57)
- `machine boot` launches and supervises one sandbox VM (#55)
- A minimal Linux 6.18 LTS guest kernel for x86-64 and arm64 (#52)
- The guest agent runs as PID 1, with early mounts, console logging and a clean power-off (#51)
- A minimal TCG-only QEMU bundle, and a host tool that refuses any other QEMU (#47)

### Bug fixes

- `run` refuses the flags that are not implemented yet instead of ignoring them (#67)
- The x86_64 agent boots under `microvm` (#56)
- The QEMU bundle keeps the option ROM `microvm`'s SeaBIOS needs to boot a kernel (#53)

### Documentation

- A documentation site (#43)
- Getting started on Windows, macOS and Linux (#73)
- When to use oci-hermit (#62)

### Performance

- The QEMU bundle is built without debug info (#49)
