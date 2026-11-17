# Changelog

oci-hermit has no release yet. Until the first one (#40), this page lists what has landed on `main`, newest first.
The CI builds of `main` described in [Getting started](docs/getting-started.md) contain all of it.

## Unreleased

### Features

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
