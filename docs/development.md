# Development

Everything runs through [pixi](https://pixi.sh), which provides the Rust toolchain and every other tool from
conda-forge. Clone the repository and install the hooks once:

```bash
git clone https://github.com/millsks/oci-hermit
cd oci-hermit
pixi run bootstrap        # pre-commit and commit-msg hooks
```

## Tasks

| Task | What it runs |
| --- | --- |
| `pixi run fmt` | `cargo fmt --all` |
| `pixi run lint` | clippy on the whole workspace, warnings as errors |
| `pixi run check` | `cargo check` on the whole workspace |
| `pixi run test` | `cargo test --workspace`; one test: `pixi run cargo test -p oci-hermit <name>` |
| `pixi run build` | a release build of the workspace |
| `pixi run cov` | the tests under `cargo llvm-cov`, failing below 90% line coverage |
| `pixi run ci` | the full gate: pre-commit, build, check, clippy, coverage |
| `pixi run -e lint deny` | `cargo deny`: advisories, licences, banned crates and sources |
| `pixi run -e msrv msrv-check` | `cargo check` on the oldest supported Rust |
| `pixi run -e agent agent-build` | the static guest agent (Linux only) |
| `pixi run -e qemu-build qemu-build` | the QEMU bundle into `qemu/out` (Linux and macOS) |
| `pixi run -e kernel-build kernel-build [arch] [out-dir]` | the guest kernel (Linux only, natively per architecture) |
| `pixi run -e docs docs-serve` | this site, served locally with live reload |
| `pixi run -e docs docs-build` | this site, built strictly into `site/` |

The pre-commit hooks run rustfmt, clippy, taplo (TOML), typos, actionlint (workflows) and the usual file checks;
the commit-msg hook enforces [Conventional Commits](https://www.conventionalcommits.org/).

## Running a source build

A source build needs a QEMU bundle and a guest. Take them from the package in
[Getting started](getting-started.md), or from CI artifacts, and point the build at them:

```bash
pixi run cargo build --release -p oci-hermit
export OCI_HERMIT_QEMU_DIR=/path/to/oci-hermit/qemu
export OCI_HERMIT_GUEST_DIR=/path/to/oci-hermit/guest
target/release/oci-hermit machine boot
```

Keep test images out of your real store with `OCI_HERMIT_DATA_DIR`:

```bash
export OCI_HERMIT_DATA_DIR=$(mktemp -d)
```

## Workflow

- Work on a branch and open a pull request against `main`; link the issue it resolves.
- Commit messages follow Conventional Commits (`feat(run): …`, `fix(load): …`, `docs: …`).
- Every change comes with tests, and `pixi run ci` passes before you push.
- The `CI result` check is the one a pull request needs.

## Continuous integration

| Workflow | What it does |
| --- | --- |
| `ci.yml` | pre-commit, fmt, clippy and check; tests on Linux x64 and arm64, macOS and Windows; the coverage gate; `cargo deny`; the MSRV; the static agent for both architectures. `CI result` sums them up. |
| `qemu.yml` | Builds the QEMU bundle for all five hosts and checks each with `machine info`. |
| `kernel.yml` | Builds and boot-tests both kernels, builds the agent, then on every kind of host boots both guests, runs a Debian image (output, exit code, stdin, a terminal), pulls and loads images, measures start times, runs as a standard user without admin rights, and packages the per-host folders that [Getting started](getting-started.md) downloads. |
| `docs.yml` | Builds this site strictly on pull requests that touch it; publishes it to GitHub Pages from `main` (`dev`) and from releases. |

## The documentation site

The site is [MkDocs](https://www.mkdocs.org) with the Material theme, set up as pixi-sbom's is. Pages live in
`docs/`; `mkdocs.yml` holds the navigation. A few repository files are published as pages without copies in `docs/`
(`SECURITY.md`, `ROADMAP.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `qemu/README.md`, `guest/README.md`):
`mkdocs_hooks.py` adds them, and points links that leave `docs/` at the page that publishes their target, or at
the file on GitHub. Write links as they work on GitHub (`../ROADMAP.md`); the hook takes care of the site.

```bash
pixi run -e docs docs-serve     # http://127.0.0.1:8000
pixi run -e docs docs-build     # fails on any broken link or anchor
```

Versions are published with mike: `dev` follows `main`, each release gets its own copy, and `latest` is the newest
release.

## Design discussions

Changes to the design start in an issue. The [design document](design.md) is the reference, and the
[roadmap](roadmap.md) says what comes when.
