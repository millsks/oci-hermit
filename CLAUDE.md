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
| `crates/oci-hermit/src/image.rs` | reads an OCI image layout (directory or `.tar`): manifest for the guest arch, config, layers |
| `crates/oci-hermit/src/rootfs.rs` | applies layers in memory (whiteouts, opaque dirs, hard links); refuses paths that climb out with `..` |
| `crates/oci-hermit/src/supervisor.rs` | builds the QEMU command line and runs one sandbox VM; `oci-hermit machine boot` drives it |
| `crates/hermit-agent` | guest agent, PID 1 inside the sandbox VM; Linux only. `init.rs` is the testable logic behind a `System` trait, `linux.rs` the real calls (nix) |
| `crates/hermit-proto` | control protocol shared by host and agent: newline-delimited JSON (max 64 KiB a line) over the virtio-serial port `org.oci-hermit.control`, which QEMU wires to its stdin/stdout; the `Framer` both sides use |
| `docs/design.md` | architecture, networking, mounts, security model |

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
  replace both. Blob digests are not verified yet (#14).
- The guest agent (PID 1) must never exit or panic; it powers off instead. It blocks SIGTERM, SIGINT and SIGCHLD, and
  children inherit that mask, so anything it spawns must reset the signal mask before exec.
