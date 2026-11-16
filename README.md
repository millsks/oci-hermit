# oci-hermit

**Run OCI container images in a sealed, software-emulated sandbox on Linux, Windows and macOS — no root, no admin, no hypervisor, no WSL.**

> A hermit for your containers: it lives alone, takes visitors only at the door you choose, and never goes out.

> [!WARNING]
> oci-hermit is an early spike. `oci-hermit run` runs a command from a local OCI image on Linux, macOS and Windows; most of the commands below are still to come. See the [roadmap](ROADMAP.md) and the [design doc](docs/design.md).

## Why

Testing a container image usually needs Docker, a Podman machine or WSL. On locked-down laptops, those need admin rights or hardware virtualization that you can't turn on. oci-hermit is meant for exactly that environment:

- **No elevated privileges.** Installs and runs as a normal user.
- **No hardware virtualization.** The guest runs on a software-emulated CPU (QEMU TCG), so nothing has to be enabled in the BIOS or Windows features.
- **Sealed by default.** No egress at all. Ingress only through ports you publish, bound to `127.0.0.1`.
- **Familiar commands.** Mirrors the Docker and Podman CLI, with a Compose subset for frontend-plus-backend apps.

It targets smoke-testing prebuilt images: start an image and get a shell, or bring up a web app and hit it from your browser. Images arrive fully built; `npm install`, `pixi install` and the like happen before oci-hermit sees them.

It is a development and testing tool, not a production runtime: it is slower than native and runs on its own guest kernel. [When to use oci-hermit](docs/when-to-use.md) covers what it is good for and where it differs from production.

## What it will look like

```bash
oci-hermit pull ghcr.io/org/app:1.2
oci-hermit load -i app.tar

oci-hermit run -it --rm alpine sh
oci-hermit run -d --name web -p 8080:80 -v ./data:/data app:1.2

oci-hermit compose up -d
```

Differences from Docker are deliberate: `-p` always binds to `127.0.0.1`, `-v` is read-only unless you add `:rw`, and flags like `--privileged` are refused.

## How it works

Each sandbox is one emulator process launched as you. The host tool pulls and unpacks images without privileges, hands the layers to a minimal Linux guest as read-only disks, and a small guest agent starts the containers with crun. Every path into the guest passes through the host tool; there is no path out. Details in [docs/design.md](docs/design.md).

## Installation

Not released yet. The plan is to publish on conda-forge, so it installs with:

```bash
pixi global install oci-hermit
# or
conda install -c conda-forge oci-hermit
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Work is tracked in the [issues](https://github.com/millsks/oci-hermit/issues) by [milestone](https://github.com/millsks/oci-hermit/milestones).

## License

[Apache-2.0](LICENSE). The bundled QEMU emulator runs as a separate process and keeps its own GPL-2.0 license.
