# Contributing

Thanks for helping build oci-hermit. The project is early, so the [roadmap](ROADMAP.md) and [design doc](docs/design.md) are the best place to start.

## Development setup

The development environment is managed with [pixi](https://pixi.sh), which provides the Rust toolchain from conda-forge.

```bash
pixi run build      # cargo build --workspace
pixi run test       # cargo test --workspace
pixi run fmt        # format
pixi run check      # format check, clippy and tests, as CI runs them
```

Plain `cargo` works too if you already have a recent stable Rust.

## Layout

| Path | What it is |
| --- | --- |
| `crates/oci-hermit` | Host tool and CLI |
| `crates/hermit-agent` | Guest agent, PID 1 inside the sandbox VM (Linux only) |
| `crates/hermit-proto` | Control protocol shared by host and guest |
| `docs/design.md` | Architecture and decisions |

## Workflow

- Work on a feature branch and open a pull request against `main`.
- CI runs format, clippy and tests on Linux (x86-64 and arm64), macOS and Windows. A PR is ready when all checks pass.
- Link the issue the PR resolves.

## Ground rules from the design

- Nothing may require root, admin rights or hardware virtualization.
- The sandbox never gets egress. Changes that open a path out of the guest need a design discussion first.
- No `unsafe` in host-side code without a documented reason.
