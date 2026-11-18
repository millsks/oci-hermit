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
| `crates/oci-hermit/src/guest.rs` | finds each architecture's kernel and agent (`$OCI_HERMIT_GUEST_DIR/<arch>/{kernel,hermit-agent}`) |
| `crates/oci-hermit/src/initramfs.rs` | writes the guest initramfs (agent as `/init`, `/dev/console`) without privileges |
| `crates/oci-hermit/src/image.rs` | reads an OCI image layout (directory or `.tar`): manifest for the guest arch, config, layers; checks every blob against its sha256 digest |
| `crates/oci-hermit/src/store.rs` | the image store: one OCI layout in the user's data folder (`$OCI_HERMIT_DATA_DIR`), images named by full reference and platform |
| `crates/oci-hermit/src/images.rs` | `images`, `rmi`, `inspect`, `tag`, `save`, `image prune`/`history`/`exists`, `system df` over the store; `find` resolves names, references and ID prefixes for every command including `run` |
| `crates/oci-hermit/src/load.rs` | `load -i`: OCI layouts (dir or `.tar`, every platform) and `docker save` archives (old and Docker 25+) into the store; unnamed images go by ID |
| `crates/oci-hermit/src/registry.rs` | `pull`: our own distribution-API client on reqwest (rustls with ring): Basic and Bearer (token server) auth, retries, Podman/Docker auth files and credential helpers, `$OCI_HERMIT_INSECURE_REGISTRIES` for plain HTTP |
| `crates/oci-hermit/src/certs.rs` | per-registry CAs and client certificates from Podman's and Docker's `certs.d` folders (or `--cert-dir`); one HTTP client per registry, so a registry's CA never applies elsewhere |
| `crates/oci-hermit/src/reference.rs` | image references as Docker reads them (`debian` is `docker.io/library/debian:latest`) |
| `crates/oci-hermit/src/login.rs` | `login`/`logout`: checks credentials with the registry (reqwest: `/v2/` with Basic, or its token server for a Bearer challenge), then writes Podman's `auth.json` (0600); never prints secrets |
| `crates/oci-hermit/src/rootfs.rs` | applies layers in memory (whiteouts, opaque dirs, hard links); refuses paths that climb out with `..` |
| `crates/oci-hermit/src/users.rs` | resolves the image's `User` (or `-u`) against the image's own `/etc/passwd` and `/etc/group`, as runc does |
| `crates/oci-hermit/src/supervisor.rs` | builds the QEMU command line and runs one sandbox VM; `oci-hermit machine boot` drives it |
| `crates/hermit-agent` | guest agent, PID 1 inside the sandbox VM; Linux only. `init.rs` is the testable logic behind a `System` trait, `linux.rs` the real calls (nix) |
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
- `run` is the M1 spike: the image's files go into the initramfs under `/rootfs` (so the guest needs about twice
  the image's size in RAM) and the agent chroots there with the command. EROFS layer disks (#16, #17) and crun (#21)
  replace both.
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
