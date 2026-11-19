# CLAUDE.md

Project guidance for millsks/oci-hermit. This project is **Rust**. The design is in `docs/design.md`, the plan in
`ROADMAP.md`, and the work in GitHub issues grouped by milestone (M0–M7).

## Architecture

A Cargo workspace. oci-hermit runs prebuilt OCI images inside a software-emulated (QEMU TCG) Linux VM, with no root,
no admin, no hypervisor and no WSL.

| Path | What it is |
|---|---|
| `crates/oci-hermit` | host tool and CLI (clap), Docker/Podman-style commands |
| `crates/oci-hermit/src/qemu.rs` | finds and verifies the bundled QEMU (`$OCI_HERMIT_QEMU_DIR`, then install layouts) |
| `crates/oci-hermit/src/guest.rs` | finds each architecture's kernel, agent and crun (`$OCI_HERMIT_GUEST_DIR/<arch>/{kernel,hermit-agent,crun}`) |
| `crates/oci-hermit/src/initramfs.rs` | writes the guest initramfs (agent as `/init`, `/dev/console`) without privileges |
| `crates/oci-hermit/src/image.rs` | reads an OCI image layout (directory or `.tar`): manifest for the guest arch, config, layers; checks every blob against its sha256 digest |
| `crates/oci-hermit/src/store.rs` | the image store: one OCI layout in the user's data folder (`$OCI_HERMIT_DATA_DIR`), images named by full reference and platform |
| `crates/oci-hermit/src/images.rs` | `images`, `rmi`, `inspect`, `tag`, `save`, `image prune`/`history`/`exists`, `system df` over the store; `find` resolves names, references and ID prefixes for every command including `run` |
| `crates/oci-hermit/src/load.rs` | `load -i`: OCI layouts (dir or `.tar`, every platform) and `docker save` archives (old and Docker 25+) into the store; unnamed images go by ID |
| `crates/oci-hermit/src/registry.rs` | `pull`: our own distribution-API client on reqwest (rustls with ring): Basic and Bearer (token server) auth, retries, Podman/Docker auth files and credential helpers, `$OCI_HERMIT_INSECURE_REGISTRIES` for plain HTTP |
| `crates/oci-hermit/src/certs.rs` | per-registry CAs and client certificates from Podman's and Docker's `certs.d` folders (or `--cert-dir`); one HTTP client per registry, so a registry's CA never applies elsewhere |
| `crates/oci-hermit/src/reference.rs` | image references as Docker reads them (`debian` is `docker.io/library/debian:latest`) |
| `crates/oci-hermit/src/login.rs` | `login`/`logout`: checks credentials with the registry (reqwest: `/v2/` with Basic, or its token server for a Bearer challenge), then writes Podman's `auth.json` (0600); never prints secrets |
| `crates/oci-hermit/src/erofs.rs` | a layer tarball as a read-only EROFS image, in user space (#16): whiteouts as 0:0 devices, opaque dirs as `trusted.overlay.opaque`, the layer's own `trusted.*` xattrs dropped; `Store::layer_disk` caches one per layer digest under `erofs/<FORMAT>/`. Bump `erofs::FORMAT` whenever the output changes |
| `crates/oci-hermit/src/containers.rs` | the container state store (#20): `<data>/containers/<id>/{container.json,state.json}`; `ps`, `rm` (`-f` stops first); `run` records each container and `--rm` removes it |
| `crates/oci-hermit/src/monitor.rs` | the per-container monitor (#22): hidden `__monitor` process that owns QEMU, `container.log`, the exit code; `run` (attach, `-d`), `logs [-f]`, `stop` talk to it |
| `crates/oci-hermit/src/ipc.rs` | the monitor's channel: Unix socket in `<data>/sockets/` (0700), or a named pipe on Windows (tokio, blocked on from threads) |
| `crates/oci-hermit/src/rootfs.rs` | applies layers in memory (whiteouts, opaque dirs, hard links); refuses paths that climb out with `..` |
| `crates/oci-hermit/src/users.rs` | resolves the image's `User` (or `-u`) against the image's own `/etc/passwd` and `/etc/group`, as runc does |
| `crates/oci-hermit/src/supervisor.rs` | builds the QEMU command line and runs one sandbox VM; `oci-hermit machine boot` drives it |
| `crates/hermit-agent` | guest agent, PID 1 inside the sandbox VM; Linux only. `init.rs` is the testable logic behind a `System` trait (and the containers' OCI `config.json`), `linux.rs` the real calls (nix) |
| `guest/crun/build.sh` | static crun (with json-c, libseccomp, libcap) per guest architecture, from pinned tarballs |
| `crates/hermit-proto` | control protocol shared by host and agent: newline-delimited JSON (max 64 KiB a line) over the virtio-serial port `org.oci-hermit.control`, which QEMU wires to its stdin/stdout; the `Framer` both sides use |
| `docs/design.md` | architecture, networking, mounts, security model |
| `packaging/` | release packaging: `licenses.py` (license files and source of everything bundled, called by `qemu/build.sh` and the guest steps), `package.py` (one host's folder and archive, with `hrmt` and THIRD-PARTY-LICENSES), `sources.py` (the GPL/LGPL source archive); used by `kernel.yml` and `release.yml` |
| `docs/getting-started.md` | per-host setup; CI's hosts job (x86_64 leg) uploads the ready-to-run `oci-hermit-<host>` folder it describes |

## Commands

Everything runs through pixi.

| Task | Command |
|---|---|
| Format, lint, check | `pixi run fmt`, `pixi run lint` (clippy `-D warnings`), `pixi run check` |
| Tests | `pixi run test`; one test: `pixi run cargo test -p oci-hermit <name>` |
| Full gate | `pixi run ci` (pre-commit, build, check, clippy, coverage ≥ 90%) |
| Dependencies | `pixi run -e lint deny` |
| MSRV | `pixi run -e msrv msrv-check` |
| Static guest agent (Linux only) | `pixi run -e agent agent-build` (static glibc; conda-forge has no musl Rust std) |
| Hooks | `pixi run bootstrap` once per clone |

## Workflow

- Feature branch and pull request for every change; work is not finished until the PR's checks pass.
- Conventional commit messages (enforced by the commit-msg hook).
- The `CI result` job is the single required check.

## Off-limits and traps

- Nothing may require root, admin rights, a kernel driver or hardware virtualization (KVM, HVF, WHPX, Hyper-V). QEMU
  is always launched with `-accel tcg`.
- The guest never gets egress. A change that opens any path out of the sandbox needs a design discussion first.
- Bind mounts are read-only unless `:rw`; published ports always bind to 127.0.0.1. These differ from Docker on
  purpose.
- QEMU is GPL-2.0: it ships as a separate helper process and is never linked into a crate. `deny.toml` keeps GPL
  crates out.
- No `unsafe` in host-side crates (`unsafe_code = "deny"` in the workspace lints).
- QEMU's `-run-with exit-with-parent=on` exists only on POSIX hosts; passing `-run-with` on Windows stops QEMU from
  starting. Windows ties QEMU's lifetime to oci-hermit with a kill-on-close Job Object (`win32job`) instead.
- The x86_64 guest needs an x86-64-v3 CPU model (`supervisor::X86_64_CPU`): the agent's static glibc uses AVX in
  `init_cpu_features`, so `qemu64` dies with SIGILL before any of our code runs.
- The x86_64 kernel needs `CONFIG_PCI=y` although microvm has no PCI bus: without it ACPI fails to load, and the
  serial console, RTC and virtio-mmio devices all break. Userspace console writes then fail with EIO while the
  kernel's own messages still appear, which looks like a hang (#54).
- On Windows, upstream QEMU's `-chardev stdio` drops piped input the guest cannot take yet (and every `\r`), which
  loses the host's first message. `qemu/patches/0001-…` fixes it.
- The x86_64 guest's TSC is 1 GHz on every host (`qemu/patches/0002-…`) and the kernel is told so
  (`tsc_early_khz=1000000`): PIT calibration under TCG fails on slow hosts, and microvm has no other timer, so the
  guest hangs without a clock (#61). A bundle without these patches (marker below `oci-hermit-qemu-4`) is refused.
- The control channel is QEMU's stdin and stdout (`-chardev stdio`), so QEMU must never print to stdout: its
  diagnostics go to stderr (`qemu.log`). Treat everything from the guest as untrusted: cap line length, reject what
  is not a message, time out instead of blocking.
- `run` attaches a stored image's layers as read-only virtio disks (EROFS, built once per layer by `Store::layer_disk`,
  at most `supervisor::MAX_DISKS`), and the agent (protocol 1.3) stacks them with overlayfs under a tmpfs upper layer
  on `/rootfs` (#17). The user is resolved by reading `/etc/passwd` and `/etc/group` from the disks (`erofs::Layers`),
  never by unpacking the image. An image opened from a path, or with more layers than disks fit, still goes into
  the initramfs under `/rootfs` (about twice its size in RAM). `run` holds its shared store until QEMU has the disks
  open (after the handshake), so `rmi` cannot delete a disk being attached.
- Commands run in crun containers (#21, protocol 1.4; the host refuses an older agent). The agent writes
  `/run/oci-hermit/config.json` (`init::container_config`: Docker's namespaces, capabilities, masked paths, devices;
  no user namespace, no seccomp yet, #102) and spawns `/bin/crun --cgroup-manager=cgroupfs run`. crun's `pivot_root`
  fails on an initramfs root, so the agent first moves its root onto a tmpfs (`leave_initramfs`, as switch_root
  does), taking `/bin/crun` and binding `/rootfs`; never use `--no-pivot`. With `-t`, crun sends the pty over
  `--console-socket` (`SCM_RIGHTS`; the one `unsafe` in the agent). The agent checks the command exists inside the
  image first (`openat2` with `RESOLVE_IN_ROOT`), so a missing command is exit 125, not crun's 1. crun is built
  statically by `guest/crun/build.sh` (pixi `crun-build`); libtool needs `-all-static` at make time.
- Containers (#20): `containers/lock` serializes create, start and remove (unique names; nothing removed while it
  starts). The process running a container holds `<id>/running.lock` alone for its whole life, and only it writes
  `state.json`; "running" means that lock is held, never what `state.json` says, so a run that crashed shows as
  `Exited (unknown)`. Readers probe with a shared `try_lock`, so writers wait up to 1 s (`containers::PATIENCE`)
  before taking the lock as held. `container.json` is written last and removed first: a folder without it is not a
  container.
- Every `run` goes through a monitor (#22): `run` prepares everything, writes `<id>/launch.json`, spawns
  `oci-hermit __monitor <data> <id>` (own process group on Unix; `DETACHED_PROCESS`, and job breakaway when allowed,
  on Windows) and attaches over `ipc`. The monitor holds `running.lock`, owns QEMU (so `exit-with-parent` and the Job
  Object tie QEMU to the monitor, not to `run`) and records the exit. A foreground `run` that disconnects makes the
  monitor stop the VM; only `-d` survives its `run`. `monitor.log` lives in the run dir, not the container dir, so
  `--rm` cannot delete the reason a monitor failed. Unix socket paths must fit in 103 bytes: tests use `/tmp` scratch
  folders (`qemu::testing::scratch`), since macOS's `$TMPDIR` is too deep. Tests run the monitor on a thread
  (`Env::start_monitor` = `monitor::testing::in_thread`). `stop` powers the sandbox off at once (137); SIGTERM first
  is #25.
- The store has a lock file (`images/lock`, `File::lock`, hence MSRV 1.89). `Store::open` holds it shared for the
  store's lifetime; only `Store::open_exclusive` (rmi, prune) may delete blobs. It waits up to 2 s, then fails with
  `Busy`: on Unix a child process holds an inherited lock between `fork` and `exec`, so a zero wait is flaky. A pull writes blobs before naming them, so garbage collection without the exclusive lock would
  delete a pull in progress. `run` keeps its shared store until the rootfs is read, then drops it. `index.json`
  changes go through `Store::update` under `index.lock`.
- Every blob is verified against its digest when pulled, stored and read (#14); a mismatch is an error, never a
  warning. reqwest is built without a crypto provider and `registry::pull` installs ring: aws-lc-rs needs CMake and
  NASM on some hosts. Pulls happen in the host tool only.
- The guest agent (PID 1) must never exit or panic; it powers off instead. It blocks SIGTERM, SIGINT and SIGCHLD, and
  children inherit that mask, so anything it spawns must reset the signal mask before exec.
- Commands run as the image's `User` (#66). Around `posix_spawn` the agent switches its real and effective IDs to the
  user and keeps root as its saved IDs, so the command's exec drops root for good; then it takes root back. Never drop
  the saved root ID: PID 1 needs root to power off. The host sends a user only to an agent speaking protocol 1.2 or
  later, since an older one would ignore it and run the command as root.
