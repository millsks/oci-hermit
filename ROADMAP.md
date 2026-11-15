# Roadmap

oci-hermit is built in milestones. Each one ends with something you can run. Work items are tracked as [GitHub issues](https://github.com/millsks/oci-hermit/issues), grouped by [milestone](https://github.com/millsks/oci-hermit/milestones).

The design behind all of this is in [docs/design.md](docs/design.md).

| # | Milestone | Outcome |
| --- | --- | --- |
| M0 | Project scaffold | Cargo workspace, pixi environment, CI on Linux, Windows and macOS |
| M1 | Spike: boot and shell | `oci-hermit run -it alpine sh` works from a local OCI layout, without admin, on all three OSes |
| M2 | Images | Pull, load, cache and inspect images; overlayfs root in the guest |
| M3 | Container lifecycle | Detached containers, `exec`, `logs`, `ps`, `stop`, `rm`, `cp`, resource limits |
| M4 | Networking | Sealed networking: no egress, `-p` bound to 127.0.0.1, egress tests |
| M5 | Bind mounts | 9P file server, mount policy, read-only by default |
| M6 | Pods and Compose | Shared-localhost pods and a Compose subset |
| M7 | Hardening and release | OS sandbox around the emulator, signed binaries, conda-forge package, docs |

## M0 — Project scaffold

- Cargo workspace with the host CLI and guest agent crates
- pixi development environment and tasks
- CI: format, lint and test on Linux, Windows and macOS
- Contributing guide

## M1 — Spike: boot and shell

- Minimal guest kernel config for x86-64 and arm64
- Guest agent as PID 1: early mounts, console, shutdown
- Bundle QEMU (TCG) per host OS and locate it at runtime
- Sandbox supervisor: launch a `microvm` guest and capture its console
- Control protocol over virtio-serial
- `run -it` with PTY relay from a local OCI layout
- Measure boot time on all three OSes
- Verify no-admin execution on locked-down Windows and macOS accounts

## M2 — Images

- Registry pull with digest verification
- Load docker-archive and OCI layout tarballs
- Layer to EROFS image builder, cached by digest, without mounting anything
- Guest overlayfs root from layer disks plus a throwaway writable layer
- `images`, `rmi`, `inspect`, `image prune`
- Multi-arch image selection and `--platform`

## M3 — Container lifecycle

- Container state store
- crun in the guest and OCI runtime bundle generation from image config
- `run -d`, `exec`, `logs`, `ps`, `stop`, `rm`
- `cp` into and out of containers
- `--cpus` and `--memory` mapped to VM sizing
- Signal and exit-code propagation

## M4 — Networking

- libslirp with `restrict=on`: no egress
- `-p` publishing, always bound to 127.0.0.1
- Egress test suite proving nothing leaves the sandbox
- Spike: NIC-less port relay over virtio-serial

## M5 — Bind mounts

- 9P2000.L server in Rust over virtio-serial
- Guest-side mount with `trans=fd`
- Mount policy: read-only default, path containment, deny-list, no special files
- `-v` and `--mount` parsing; mounts shown in `inspect` and `ps`
- Benchmark against QEMU's built-in 9P

## M6 — Pods and Compose

- Pods with a shared network namespace: `pod create`, `pod rm`, `pod ps`
- Compose subset parser that rejects unsupported keys
- `compose up`, `down`, `ps`, `logs` with `depends_on` and `healthcheck`

## M7 — Hardening and release

- OS sandbox around the emulator: seccomp and Landlock, macOS App Sandbox, Windows AppContainer
- Code signing for Windows and macOS releases
- Release pipeline with cross-builds and guest artifacts
- conda-forge recipe, including how QEMU is provided
- Optional single-file download with the QEMU bundle embedded
- Image signature verification (cosign and/or notation)
- User documentation
- Optional `ohm` short alias
- pixi-oci integration for build-then-smoke-test

## Later: needs design

Ideas beyond M7 that need a design decision before any work starts, tracked in the
[Later: needs design](https://github.com/millsks/oci-hermit/milestone/9) milestone.

- Build images from a Containerfile with a build-time network policy (#50)
