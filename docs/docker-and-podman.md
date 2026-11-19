# oci-hermit, Docker and Podman

oci-hermit takes Docker's and Podman's commands, so habits carry over: `pull`, `load`, `images`, `run -it`, `rmi`
and the rest mean what you expect. It reads the same images, the same archives and the same registry logins. Some
things differ on purpose, because the point of oci-hermit is a sandbox that needs nothing from the machine and lets
nothing out.

## How it runs

| | Docker and Podman | oci-hermit |
| --- | --- | --- |
| Privileges | Docker needs a daemon running as root, or rootless set-up by an admin. Podman is rootless on Linux. | None. A folder you unpack and run as yourself. |
| Virtualization | On Windows and macOS, a Linux VM under a hypervisor (WSL 2, Hyper-V, Apple's Virtualization framework). | A VM with an emulated CPU (QEMU TCG). Nothing to enable. |
| Daemon | Docker has a daemon; Podman machine has a VM that stays up. | Neither. Each `run` boots its own VM and powers it off when the command ends. |
| Kernel | The host's (Linux) or the VM's. | oci-hermit's own minimal Linux 6.18 kernel, the same on every host. |
| Runtime | runc or crun, with namespaces and cgroups. | crun inside the VM, with Docker's default namespaces, capabilities, masked paths and devices; no seccomp profile yet (#102). The VM is the main boundary. |
| Speed | Native, or close to it. | Emulated: roughly 5 to 20 times slower for CPU work. See [Performance and limits](performance.md). |
| Other architectures | Through QEMU user-mode emulation (binfmt), set up separately. | Built in: amd64 and arm64 images run on every host. |

## Deliberate differences

| Area | Docker and Podman | oci-hermit |
| --- | --- | --- |
| Outbound network | The container can reach the network. | **Never.** The sandbox has no network device. There is no flag to turn it on. |
| Published ports (`-p`, planned in #27) | Bind to all interfaces (`0.0.0.0`) unless you name one. | Always bind to `127.0.0.1`, even if you ask for `0.0.0.0`. |
| Bind mounts (`-v`, planned in #33) | Read-write unless you add `:ro`. | Read-only unless you add `:rw`. Sensitive folders (`~/.ssh`, `~/.aws`, `~/.kube`, `~/.docker`, the whole home folder, drive roots) are refused. |
| `--network` (planned) | `bridge`, `host`, user networks. | Only `none` and a pod's shared network. |
| `--privileged`, `--cap-add`, `--device`, `--pid host`, `--ipc host` (planned) | Accepted. | Refused. |
| `--cpus`, `--memory` | Limits for the container. | The size of the emulated VM. |
| `stop` | `SIGTERM`, then `SIGKILL` after 10 seconds. | Powers the sandbox off at once: exit code 137 (a grace period comes with #25). |
| Running containers | Kept by a daemon (`dockerd`) or, in Podman, by a `conmon` process per container. | Kept by a monitor process per container, as Podman's `conmon`; no daemon. A `run` without `-d` takes its container down with it if it is killed. |
| Containers | Kept after they stop, until `rm`, with their files. | Kept after they stop, until `rm` (`--rm` removes them at once), but only as a record and the output: name, image, command, exit code and `logs`. The files a command changed go with its sandbox. |
| Building images | `build` | Not supported; build elsewhere (#50 discusses it). |
| Compose (planned in #36, #37) | Full Compose. | A subset: `services`, `image`, `ports`, `environment`, `command`, `depends_on`, `healthcheck`, `volumes`. `build:` is refused. |

A flag that exists but is not implemented yet is refused with its issue number, never ignored. See the
[command-line reference](cli.md#planned-commands) for what is planned.

## What is shared with them

- **Images and names.** The same references, with Docker's expansion: `alpine` is `docker.io/library/alpine:latest`.
- **Registry logins.** `oci-hermit login` and `logout` work as `podman login` and `podman logout` do and write the
  same file; oci-hermit also reads Docker's auth file and runs Docker credential helpers
  ([Pull from a private registry](field-manual/private-registry.md)).
- **Archives.** `oci-hermit load` reads `docker save` and `podman save` archives; `oci-hermit save` writes the
  Docker 25 format, which `docker load` and `podman load` read.
- **Exit codes.** `run` exits with the command's code, or 125 when oci-hermit itself fails, as `docker run` does.

oci-hermit never talks to a Docker daemon or to Podman, and does not share their image stores: an image you pulled
with Docker is not visible to oci-hermit until you `docker save` it and `oci-hermit load` it.

## When to use which

Use **Docker or Podman** where you can: for building images, for anything that needs the network, for performance,
and for the final check before you deploy. Use **oci-hermit** where they are not an option, such as a locked-down
laptop, or when you want a sealed sandbox that cannot reach anything. [When to use oci-hermit](when-to-use.md) goes
into more detail.

If Docker and Podman are blocked on your machine on purpose, check that running an emulator is allowed before you
use oci-hermit there. Needing no admin rights is not the same as being permitted.
