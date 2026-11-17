# Architecture

oci-hermit is a Cargo workspace of three crates, plus the build scripts for the QEMU bundle and the guest kernel.
[How it works](how-it-works.md) shows how the pieces fit at run time; this page maps them to the source.

## Crates and modules

| Path | What it is |
| --- | --- |
| `crates/oci-hermit` | The host tool and its command line. |
| `crates/oci-hermit/src/cli.rs` | The command line (clap), modelled on Docker and Podman, including the commands that are not implemented yet. |
| `crates/oci-hermit/src/main.rs` | Dispatch; `run` (choose the architecture, find the image, build the initramfs, size the VM, run, report timings); `machine info` and `machine boot`. |
| `crates/oci-hermit/src/qemu.rs` | Finds the bundled QEMU (`$OCI_HERMIT_QEMU_DIR`, then the install layouts) and refuses any other: exact version and marker, `tcg`, firmware. |
| `crates/oci-hermit/src/guest.rs` | Finds each architecture's kernel and agent (`$OCI_HERMIT_GUEST_DIR/<arch>/{kernel,hermit-agent}`, then the install layouts). |
| `crates/oci-hermit/src/image.rs` | Reads an OCI image layout (directory or `.tar`): the manifest for the guest architecture, the config and the layers, checking every blob against its sha256 digest. |
| `crates/oci-hermit/src/store.rs` | The image store: one OCI layout in the user's data folder, images named by full reference and platform, a shared lock for readers and an exclusive one for deleting. |
| `crates/oci-hermit/src/registry.rs` | `pull` over oci-client (reqwest, rustls with ring, the OS trust store): Podman and Docker auth files and credential helpers, retries, `$OCI_HERMIT_INSECURE_REGISTRIES`. |
| `crates/oci-hermit/src/load.rs` | `load -i`: OCI layouts and `docker save` archives (old and Docker 25+) into the store; unnamed images go by ID. |
| `crates/oci-hermit/src/images.rs` | The image commands over the store: `images`, `rmi`, `inspect`, `tag`, `save`, `image prune`, `image history`, `image exists`, `system df`. |
| `crates/oci-hermit/src/rootfs.rs` | Applies layers in memory: whiteouts, opaque directories, hard links; refuses paths that climb out with `..`. |
| `crates/oci-hermit/src/users.rs` | Resolves the image's `User` (or `-u`) against the image's own `/etc/passwd` and `/etc/group`, as runc does. |
| `crates/oci-hermit/src/initramfs.rs` | Writes the guest initramfs (the agent as `/init`, `/dev/console`, the image under `/rootfs`) as a `newc` cpio in memory, without privileges. |
| `crates/oci-hermit/src/supervisor.rs` | Builds the QEMU command line, starts and supervises one sandbox VM, and drives the control protocol with timeouts. |
| `crates/hermit-agent` | The guest agent, PID 1 in the sandbox; Linux only. `init.rs` is the testable logic behind a `System` trait, `linux.rs` the real system calls (nix). It refuses to start anywhere but as PID 1 on Linux. |
| `crates/hermit-proto` | The control protocol shared by host and agent: message types, the protocol version, and the `Framer` both sides use to cut the byte stream into lines of at most 64 KiB. |
| `qemu/` | `build.sh` and patches for the TCG-only QEMU bundle. See [Building QEMU](building-qemu.md). |
| `guest/kernel/` | `build.sh`, config fragments and a boot test for the guest kernel. See [Building the guest](building-guest.md). |
| `docs/` | This site. `docs/design.md` is the [design document](design.md). |

## Rules the code keeps

These come from the project's ground rules; a change that breaks one needs a design discussion first.

- **Nothing needs root, admin rights, a kernel driver or hardware virtualization.** QEMU is always launched with
  `-accel tcg`.
- **The guest never gets egress.** No change may open a path out of the sandbox without a design discussion.
- **Bind mounts are read-only unless `:rw`; published ports bind to 127.0.0.1.** Both differ from Docker on purpose.
- **QEMU is GPL-2.0.** It ships as a separate helper process and is never linked into a crate; `deny.toml` keeps GPL
  crates out.
- **No `unsafe` in host-side crates** (`unsafe_code = "deny"` in the workspace lints).
- **The guest is untrusted.** Cap line lengths, reject what is not a message, time out instead of blocking.
- **Every blob is verified** against its digest when pulled, stored and read; a mismatch is an error, never a
  warning. Pulls happen in the host tool only.
- **PID 1 never exits or panics.** The agent powers off instead, and anything it spawns resets the signal mask.

## Platform traps

Workarounds the code depends on. Each is commented where it lives.

| Trap | Where it is handled |
| --- | --- |
| The `qemu64` CPU stops the agent with SIGILL before any of its code runs: the static glibc uses AVX. | `supervisor::X86_64_CPU`, an x86-64-v3 model. |
| Without `CONFIG_PCI=y`, ACPI fails on `microvm`, userspace console writes fail with EIO and the guest looks hung (#54). | `guest/kernel/config/x86_64.config` |
| `-run-with exit-with-parent=on` stops QEMU from starting on Windows. | Passed on POSIX hosts only; a kill-on-close Job Object (`win32job`) on Windows. |
| On Windows, upstream QEMU's `-chardev stdio` drops piped input the guest cannot take yet, losing the host's first message. | `qemu/patches/0001-…` |
| Under TCG, the x86 guest's clock calibration fails on slow hosts, and `microvm` has no other timer (#61). | `qemu/patches/0002-…` (a 1 GHz TSC) and `tsc_early_khz=1000000`; bundles older than `oci-hermit-qemu-4` are refused. |
| QEMU printing to stdout would corrupt the control channel. | The console goes to a file; QEMU's stderr to `qemu.log`. |
| A PID 1 that exits or panics panics the kernel; children inherit its blocked signals. | The agent catches panics and powers off; spawned commands get a clean signal mask. |
| A command must not keep root, but PID 1 needs root to power off. | Around `posix_spawn` the agent switches its real and effective IDs to the user and keeps root as its saved IDs, then takes root back. |
