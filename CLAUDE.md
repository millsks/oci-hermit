# CLAUDE.md

Project guidance for millsks/oci-hermit. This project is **Rust**. The design is in `docs/design.md`, the plan in
`ROADMAP.md`, and the work in GitHub issues grouped by milestone (M0–M7).

## Architecture

A Cargo workspace. oci-hermit runs prebuilt OCI images inside a software-emulated (QEMU TCG) Linux VM, with no root,
no admin, no hypervisor and no WSL.

| Path | What it is |
|---|---|
| `crates/oci-hermit` | host tool and CLI (clap), Docker/Podman-style commands |
| `crates/hermit-agent` | guest agent, PID 1 inside the sandbox VM; Linux only |
| `crates/hermit-proto` | control protocol shared by host and agent (newline-delimited JSON over virtio-serial) |
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
