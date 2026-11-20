# oci-hermit — Design

Status: draft, 2026-11-15

## Summary

Build a single-binary tool that runs prebuilt OCI images inside a software-emulated Linux sandbox on Linux, Windows and macOS. It needs no root, no admin rights, no hardware virtualization and no WSL. Each sandbox is a small emulated VM: images are unpacked on the host without privileges, handed to the guest as read-only disks, and started by crun inside the guest. Networking is sealed: no egress, and ingress only through ports the user publishes on the host's 127.0.0.1.

**Goals**

- Test prebuilt container images on locked-down laptops where Docker, Podman machines and WSL are unavailable.
- Strong isolation: the guest never runs directly on the host CPU and has no route out.
- A Docker/Podman-style CLI, including a Compose subset for frontend-plus-backend apps.
- One portable codebase, written in Rust.

**Non-goals**

- Building images. `npm install`, `pixi install` and similar run before the image reaches this tool.
- Native-speed or production workloads.
- Bridge or host networking, privileged containers, device passthrough.

## Requirements and constraints

| Requirement | Decision |
| --- | --- |
| Host platforms | Linux, Windows, macOS (x86-64 and arm64 hosts) |
| Privileges | None: no root, no admin, no installer drivers |
| Hardware virtualization | Not required; pure software CPU emulation |
| WSL | Not used |
| Egress | Blocked, always |
| Ingress | Only explicitly published ports, bound to 127.0.0.1 |
| In-sandbox networking | Loopback works; containers in one pod share localhost |
| Images | Prebuilt OCI images: registry pull, OCI layout or tarball |
| Workloads | Smoke-testing images: web frontends and backends, interactive shells |
| Host folders | Bind mounts allowed, read-only by default |
| CLI | Mirrors Docker and Podman commands |

## Architecture

Each sandbox is one emulator process that the host tool launches as the user. Every path into the guest passes through a host-tool component and a single virtio device, and there is no path out.

```mermaid
flowchart TB
    subgraph inputs[Host inputs]
        term[Terminal<br/>run, exec, compose]
        browser[Host browser<br/>127.0.0.1:8080]
        folders[Host folders<br/>ro by default]
        registry[OCI registry<br/>or local tarball]
    end

    subgraph host[Host tool — Rust, runs as you, no admin]
        cli[CLI and state]
        ports[Port publisher<br/>binds 127.0.0.1 only]
        ninep[9P file server<br/>enforces mount policy]
        store[Image store<br/>EROFS layer cache]
    end

    subgraph qemu[QEMU, software CPU — TCG: the security boundary]
        ctl[Control channel<br/>virtio-serial]
        nic[Restricted NIC<br/>slirp, restrict=on]
        fch[File channel<br/>virtio-serial, 9P]
        disks[Layer disks<br/>virtio-blk, read-only]

        subgraph guest[Guest Linux VM — minimal kernel]
            agent[Guest agent, PID 1<br/>mounts layers and 9P<br/>starts containers via crun<br/>relays published ports]
            subgraph pod[Pod: containers share localhost]
                fe[Frontend container]
                be[Backend container]
            end
            rootfs[Container root: read-only layers + throwaway writable layer]
        end
    end

    internet[(Internet and LAN)]

    term --> cli
    browser --> ports
    folders --> ninep
    registry --> store
    cli --> ctl
    ports --> nic
    ninep --> fch
    store --> disks
    ctl --> agent
    nic --> pod
    fch --> agent
    disks --> rootfs
    guest -. blocked: no route out .-x internet
```

Read it top to bottom: inputs reach the host tool, each host-tool component speaks to the guest through one device, and the guest agent wires those devices into the pod. The QEMU process is the security boundary.

## Components

**Host tool (Rust, single binary).** Owns everything on the host side and is the only process the user runs.

- CLI front end and a small state store (images, containers, pods) under the user's data directory.
- Image store: pulls and verifies images from registries, or loads OCI layouts and tarballs offline.
- Layer builder: turns each layer into a read-only EROFS disk image, cached by digest.
- Sandbox supervisor: launches and monitors one emulator process per sandbox, applies CPU and memory limits. QEMU is
  started with `-accel tcg -nodefaults -no-user-config -display none -no-reboot`, the console on a log file, and no
  network yet. It exits with the host tool: `-run-with exit-with-parent=on` on Linux and macOS, a kill-on-close Job
  Object on Windows. `oci-hermit machine boot` boots one sandbox until the agent is ready, as a self-check.
- Container monitor (#22): each container's sandbox belongs to a background `oci-hermit __monitor` process of its own,
  like Podman's `conmon`, with no daemon. It holds the container's running lock, owns QEMU, writes the output to the
  container's log and records the exit code. Other commands reach it over an owner-only local channel (a Unix
  socket in the data folder; a named pipe on Windows): `run` attaches to it, `stop` asks it to stop, `exec` asks it
  to start another command in the container (protocol 1.5, `crun exec` in the guest). A foreground `run` that goes
  away takes its container with it; `run -d` leaves it running.
- Control client: talks to the guest agent over the virtio-serial port `org.oci-hermit.control`. QEMU wires that port to its own stdin and stdout, which the host tool holds as an anonymous pipe pair, so nothing on the filesystem or network can reach it. Messages are newline-delimited JSON (`hermit-proto`), at most 64 KiB a line; a session opens with `Hello`/`Ready` (major versions must match) and ends with `Shutdown`/`ShuttingDown`. Anything else from either side gets an error reply, and the host never trusts the guest to behave.
- 9P file server: serves bind-mounted folders and enforces the mount policy.
- Port publisher: listens on 127.0.0.1 and relays connections into the guest.

**Emulator (bundled QEMU, TCG mode).** A helper process launched by the host tool. oci-hermit builds it itself, TCG-only, and refuses any other QEMU ([qemu/README.md](../qemu/README.md)).

- `microvm` machine type for x86_64 guests and `virt` for aarch64 ones, virtio devices only: block disks, one network card, serial channels, console.
- Multi-threaded TCG so a sandbox can have several vCPUs.
- Emulates x86-64 or arm64 guests on any host, so `--platform` works everywhere.

**Guest kernel.** A minimal Linux kernel built by the project: virtio, overlayfs, EROFS, 9P, namespaces and cgroups. Nothing else. It is the newest LTS release, configured from `tinyconfig` plus committed fragments; see [guest/README.md](../guest/README.md).

**Guest agent (Rust, static, PID 1).**

- Boots as init: mounts `/proc`, `/sys`, devtmpfs on `/dev`, devpts, and tmpfs on `/run` and `/tmp` (an already-mounted target is skipped), logs line-oriented `hermit-agent: ...` messages to the console, and reaps orphaned children.
- Never exits: on a shutdown request or any fatal setup error it logs, syncs and powers the VM off, because a PID 1 that exits or panics panics the kernel. The shutdown request is the host's `Shutdown` message, or SIGTERM or SIGINT; the agent logs a `ready` line with the protocol version once it is waiting. Run anywhere other than as PID 1 on Linux, it refuses to start.
- A static binary per guest architecture (`pixi run -e agent agent-build`, x86-64 and arm64). It is static glibc (`crt-static`, conda-forge's 2.39 sysroot) rather than musl, because conda-forge ships no musl Rust standard library.
- Mounts the layer disks and builds an overlayfs root with a throwaway writable layer.
- Mounts bind-mounted folders over 9P.
- Generates OCI runtime bundles and starts containers with crun.
- Handles exec, PTYs, logs, signals and exit codes for the host tool.
- Runs containers of one pod in a shared network namespace.

**`run` today.** A stored image runs through the image pipeline below. An image opened from a path (an OCI layout directory or `.tar`), or one with more layers than a sandbox takes disks, is applied in memory instead and its files go into the initramfs under `/rootfs`. Either way the agent writes an OCI runtime bundle (`/run/oci-hermit/config.json`, with Docker's default namespaces, capabilities, masked paths and device rules) and starts the command with `crun run`, on a pty crun hands over through a console socket for `-t` or on pipes otherwise, then relays it over the control channel and reports its exit code. crun's `pivot_root` refuses an initramfs root, so at boot the agent moves its root onto a tmpfs, as `switch_root` does. Seccomp (#102) is not applied yet.

**Image pipeline.** pull or load → verify digests → per-layer EROFS image (cached) → attached read-only to the VM → stacked with overlayfs in the guest. The host never mounts anything, so no privileges are needed.

## Networking

The sandbox has no egress at all. The only way in is a port the user publishes, and it is bound to the host's 127.0.0.1.

- **Starting point:** QEMU user-mode networking (libslirp) with `restrict=on`. The guest cannot reach the host or anything beyond it, and explicit forwarding rules still work. `-p 8080:80` becomes a forward from `127.0.0.1:8080` on the host to port 80 in the guest.
- **Inside the sandbox:** loopback works normally. Containers in one pod share a network namespace, so a frontend reaches its backend on `localhost`.
- **Binding rule:** published ports bind to 127.0.0.1 even when the user asks for `0.0.0.0`. Exposing to the LAN would need an explicit opt-in flag, not in the first release.
- **Stronger later option:** remove the guest NIC entirely. The guest agent accepts relayed connections over virtio-serial and connects to the container's port on loopback. With no network device, there is no network stack exposed to the guest.
- **Image pulls** happen in the host tool only. The guest never sees registry credentials or a route out.
- **Failure is a feature:** an image that tries to download things at runtime fails, which proves it isn't self-contained.

## Filesystem and bind mounts

The container root is an overlay of read-only image layers plus a throwaway writable layer, discarded when the container is removed. Host folders can be bind-mounted, read-only unless the user says otherwise.

**Mechanism.** The host tool runs its own 9P server and carries it over a virtio-serial channel. The guest agent mounts it with the Linux 9P client in `trans=fd` mode, then bind-mounts it into the container. The same code works on all three host OSes, and every file operation passes through code the project controls.

**Mount policy, enforced by the 9P server:**

- Read-only by default. `-v ./src:/app` is read-only; writing needs `:rw`. This is a deliberate difference from Docker.
- No escaping the shared root: `..`, outward symlinks and Windows junctions are refused. On Linux, `openat2` with `RESOLVE_BENEATH` does this in the kernel.
- Deny-list of sources: drive roots, the whole home folder, `~/.ssh`, `~/.aws`, `~/.kube`, `~/.docker` and OS credential stores.
- No device nodes, FIFOs or sockets. Setuid bits and host ownership are ignored; files appear owned by the container user.
- Every active mount and its mode is shown in `oci-hermit inspect` and `oci-hermit ps`.

**Syntax** follows Docker: `-v src:dst[:ro|:rw]` and `--mount type=bind,src=,dst=,readonly`. Compose `volumes:` entries follow the same rules. Named volumes come later.

**Documented trade-off:** an `:rw` mount is an intentional hole. Anything the container writes there lands on the host.

## CLI and Compose subset

Commands mirror Docker and Podman so existing habits carry over. `oci-hermit` is the binary name. A short name, `hrmt`, ships beside it for daily use (#44). No bare `hermit` binary is shipped, to avoid clashing with Cash App's Hermit on a user's PATH.

```bash
# Images (host side)
oci-hermit pull ghcr.io/org/app:1.2
oci-hermit load -i app.tar            # docker-archive or OCI layout
oci-hermit images | rmi | inspect | tag | save | image prune | image history | image exists
oci-hermit system df

# Containers
oci-hermit run -it --rm alpine sh
oci-hermit run -d --name web -p 127.0.0.1:8080:80 -v ./data:/data app:1.2
oci-hermit exec -it web sh
oci-hermit logs -f web
oci-hermit ps -a | stop | rm | cp

# Pods (shared localhost, like podman pod)
oci-hermit pod create --name shop -p 8080:3000
oci-hermit run -d --pod shop backend:1.0
oci-hermit run -d --pod shop frontend:1.0

# Compose subset
oci-hermit compose up -d
oci-hermit compose down

# Sandbox details
oci-hermit machine info
oci-hermit machine boot [--arch x86_64|aarch64]   # boot a sandbox until its agent is ready
```

**Deliberate differences from Docker**

| Area | Behaviour |
| --- | --- |
| `-p` | Always binds to 127.0.0.1 |
| `--network` | Only `none` and pod-shared; no bridge or host |
| Refused flags | `--privileged`, `--cap-add`, `--device`, host PID or IPC namespaces |
| `-v` | Read-only unless `:rw` |
| `--cpus`, `--memory` | Limit the container, as Docker does, with its cgroup; the emulated VM is sized to fit (a vCPU per CPU started, 128 MiB over the memory limit) |
| Compose | `services`, `image`, `ports`, `environment`, `command`, `depends_on`, `healthcheck`, `volumes`; `build:` rejected |

A Compose project runs as one pod in one sandbox VM.

## Security model

The security boundary is the emulator process: guest code never runs directly on the host CPU, and it can only touch the devices the emulator provides. Defence in depth comes from keeping that boundary small and locking down the process around it.

**Layers, outermost first**

1. **Host OS sandbox around the emulator,** using unprivileged features: seccomp and Landlock on Linux, App Sandbox on macOS, an AppContainer or low-integrity token on Windows.
2. **Software CPU emulation (TCG).** No hypervisor or kernel driver on the host.
3. **Minimal device model:** virtio block, serial and (initially) one restricted NIC. No USB, graphics, sound or shared clipboard.
4. **Guest kernel** separating the container from the agent.
5. **Container namespaces and cgroups** inside the guest, via crun.

**What the guest can reach:** its own read-only layer disks, its writable layer, published-port connections coming in, and bind-mounted folders under the mount policy. Nothing else.

**Host-side code** is Rust, removing most memory-safety bugs from the parts that parse guest input (9P, control protocol, port relay).

**Supply chain:** image digests are verified on pull. Signature verification (cosign or notation) is a later milestone. Release binaries are code-signed on Windows and macOS.

## Performance expectations

Software emulation is slower than native, but the target workloads are light because images arrive fully built.

| Activity | Expectation |
| --- | --- |
| Sandbox start | About 1 to 1.5 seconds on a recent laptop, 2 to 6 on CI runners; see [Measured start times](#measured-start-times) |
| Interactive shell | Responsive |
| Web frontend and backend under test traffic | Acceptable for smoke tests |
| CPU-heavy work (compiles, large data loads) | Several times to more than 10x slower than native; out of scope |

Levers if needed: layer caching, a pre-booted kernel snapshot, and more vCPUs through multi-threaded TCG.

### Measured start times

Time from `oci-hermit run` to a shell in the Debian 12 slim image, then back out: `oci-hermit run debian.tar bash -ic
exit`, release build, one vCPU; each row is the run with the median total out of three. Set `OCI_HERMIT_TIMINGS=1` to have `run` print the same breakdown
on stderr:

- **Prepare:** read the image, apply its layers and write the initramfs.
- **Boot:** start QEMU, boot the kernel and wait for the agent to say it is ready.
- **Command:** start the command and wait for it to exit (here, an interactive `bash` that exits at once).
- **Stop:** power the guest off.

| Host | Guest | Prepare | Boot | Command | Stop | Total |
| --- | --- | --- | --- | --- | --- | --- |
| Apple M4 Mac, macOS 26.2 | aarch64 | 0.31s | 0.58s | 0.05s | 0.02s | 0.96s |
| Apple M4 Mac, macOS 26.2 | x86_64 | 0.29s | 1.09s | 0.05s | 0.03s | 1.45s |
| GitHub `ubuntu-24.04-arm` (Linux arm64) | aarch64 | 0.37s | 1.40s | 0.09s | 0.02s | 1.88s |
| GitHub `ubuntu-24.04-arm` (Linux arm64) | x86_64 | 0.34s | 3.75s | 0.11s | 0.02s | 4.22s |
| GitHub `ubuntu-latest` (Ubuntu 26.04, x64) | aarch64 | 0.49s | 1.46s | 0.11s | 0.02s | 2.08s |
| GitHub `ubuntu-latest` (Ubuntu 26.04, x64) | x86_64 | 0.41s | 2.41s | 0.13s | 0.02s | 2.98s |
| GitHub `windows-latest` (Windows Server 2025, x64) | aarch64 | 0.70s | 1.43s | 0.11s | 0.02s | 2.26s |
| GitHub `windows-latest` (Windows Server 2025, x64) | x86_64 | 0.52s | 2.80s | 0.16s | 0.04s | 3.54s |
| GitHub `macos-latest` (macOS 26, Apple silicon) | aarch64 | 0.49s | 2.35s | 0.12s | 0.09s | 3.04s |
| GitHub `macos-latest` (macOS 26, Apple silicon) | x86_64 | 0.50s | 2.93s | 0.14s | 0.03s | 3.61s |
| GitHub `macos-15-intel` (macOS 15, Intel) | aarch64 | 0.96s | 4.91s | 0.24s | 0.06s | 6.17s |
| GitHub `macos-15-intel` (macOS 15, Intel) | x86_64 | 0.61s | 3.32s | 0.19s | 0.04s | 4.17s |

The CI rows come from the `hosts` job in `kernel.yml`, which records the same three runs in every job summary.
Hosted runners are small shared VMs (3 or 4 vCPUs), so they are slower and noisier than a laptop; the Intel macOS
runner varied most, from 5.3s to 7.4s for the aarch64 guest.

Boot dominates everywhere, and a sandbox is ready in 1 to 6 seconds. An x86_64 guest boots more slowly than an
aarch64 one on every host except the Intel Mac, where the order flips. These runs are from a file, so the image is copied into the initramfs and prepare grows with its size; a stored
image runs from its EROFS layer disks (#16, #17) instead.

## Prior art

No existing tool combines rootless, no-hypervisor, cross-platform and a Docker-style UX.

| Project | What it does | Gap for this use |
| --- | --- | --- |
| [container2wasm](https://github.com/ktock/container2wasm) | Converts an image to Wasm that runs Linux and the container on an emulated CPU | Experimental; per-image conversion; no Docker-style CLI |
| [Blink](https://github.com/jart/blink) | Unprivileged user-space emulator for x86-64 Linux binaries | Forwards syscalls to the host, so no sandbox; Windows only via Cygwin without JIT |
| VirtualBox | Full VMs | Software virtualization removed in [6.1](https://docs.oracle.com/en/virtualization/virtualbox/6.1/relnotes/rn-features.html); needs VT-x or Hyper-V and admin-installed drivers |
| Podman rootless | Native rootless containers | Linux only; needs a VM on Windows and macOS |
| Lima, Podman machine, Docker Desktop | Linux VM for containers | Need a hypervisor; some need admin or WSL |

## Packaging, risks and open questions

**Packaging decision:** a standalone tool, published on conda-forge so `pixi global install`, `pixi add` and `conda install` all work. It is not a pixi extension, because its audience is anyone testing images and it ships its own emulator, kernel and agent. A thin `pixi-oci-hermit` shim or a pixi-oci hook can add a build-then-smoke-test flow later.

**Risks**

| Risk | Mitigation |
| --- | --- |
| Endpoint allow-listing (AppLocker, Defender) blocks an unsigned emulator on corporate laptops | Code-signed releases on Windows and macOS from the first release |
| QEMU is GPL-2.0 | Ship it as a separate helper process, not linked into the host tool; publish its source |
| 9P over virtio-serial is too slow for large bind mounts | Prototype early; fall back to QEMU's built-in 9P where it works |
| Emulation too slow for some test suites | Scope to smoke tests; document limits; measure in milestone 1 |
| Guest-to-emulator escape bugs | OS sandbox around the emulator; minimal device model; track QEMU security fixes |

**Open questions**

- [x] conda-forge has no system-emulation QEMU for macOS or Windows, so oci-hermit builds its own minimal one ([qemu/README.md](../qemu/README.md)).
- [ ] Keep libslirp long-term, or move to the NIC-less relay design?
- [x] Project name: oci-hermit (free on conda-forge, crates.io, PyPI, npm and GitHub as of 2026-11-15).
- [ ] Signature verification: cosign, notation, or both?

## Milestones

See [ROADMAP.md](../ROADMAP.md).
