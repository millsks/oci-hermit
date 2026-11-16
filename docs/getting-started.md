# Getting started

This page takes you from nothing to a shell in a Linux image on Windows, macOS or Linux. None of it needs admin
rights, a hypervisor or WSL.

oci-hermit has no release yet (#39–#41). Until it does, every successful CI run on `main` publishes a ready-to-run
folder for each host. GitHub keeps these for 14 days, and only signed-in GitHub users can download them.

## What you get

One folder that runs from wherever you unpack it:

```
oci-hermit/
  oci-hermit(.exe)        the host tool
  qemu/                   the bundled emulator (QEMU, TCG only)
  guest/x86_64/           the guest kernel and agent, for x86_64 images
  guest/aarch64/          the same for arm64 images
```

oci-hermit finds `qemu/` and `guest/` next to itself, so there is nothing to configure. Both guests run on every
host, because the CPU is emulated: an arm64 image runs on an Intel PC, and an amd64 image runs on a Mac with Apple
silicon.

| Your machine | Package |
| --- | --- |
| Windows, x64 | `oci-hermit-win-64` |
| macOS, Apple silicon | `oci-hermit-osx-arm64` |
| macOS, Intel | `oci-hermit-osx-64` |
| Linux, x86-64 | `oci-hermit-linux-64` |
| Linux, arm64 | `oci-hermit-linux-aarch64` |

## 1. Download

**With the GitHub CLI** (`gh`, signed in with `gh auth login`), from the latest successful run on `main`. Replace
`<package>` with the name from the table.

Windows (PowerShell):

```powershell
$run = gh run list -R millsks/oci-hermit -w kernel.yml -b main -s success -L 1 --json databaseId -q '.[0].databaseId'
gh run download $run -R millsks/oci-hermit -n oci-hermit-win-64
```

macOS and Linux:

```bash
run=$(gh run list -R millsks/oci-hermit -w kernel.yml -b main -s success -L 1 --json databaseId -q '.[0].databaseId')
gh run download "$run" -R millsks/oci-hermit -n <package>
tar -xzf <package>.tar.gz
```

**With a browser:** open the [Guest kernel workflow](https://github.com/millsks/oci-hermit/actions/workflows/kernel.yml?query=branch%3Amain+is%3Asuccess),
pick the newest run, and download your package from **Artifacts** at the bottom. Unzip it. On macOS and Linux, unpack
the `.tar.gz` inside it as well. Files a browser downloads are marked as coming from the internet; see
[If it will not start](#if-it-will-not-start).

You now have an `oci-hermit` folder. The commands below run from the folder that contains it.

## 2. Check it

Windows:

```powershell
.\oci-hermit\oci-hermit.exe version
.\oci-hermit\oci-hermit.exe machine info
.\oci-hermit\oci-hermit.exe machine boot
```

macOS and Linux:

```bash
./oci-hermit/oci-hermit version
./oci-hermit/oci-hermit machine info
./oci-hermit/oci-hermit machine boot
```

- `machine info` lists both emulators with `accel tcg`.
- `machine boot` starts a sandbox, prints `agent ready after …s` and `guest powered off`. Add `--arch aarch64` or
  `--arch x86_64` to try the other guest.

To type `oci-hermit` without the folder in front, add the folder to your `PATH`. On Windows, for this window only:
`$env:Path = "$PWD\oci-hermit;$env:Path"`. On macOS and Linux: `export PATH="$PWD/oci-hermit:$PATH"`. The rest of
this page assumes you did.

## 3. Run an image

```bash
oci-hermit run public.ecr.aws/docker/library/debian:bookworm-slim sh -c 'uname -a; cat /etc/debian_version'
oci-hermit run -it public.ecr.aws/docker/library/debian:bookworm-slim bash
```

The first `run` pulls the image, because it is not stored yet. The second one finds it. `uname -a` shows a Linux
kernel, whatever your host is. Type `exit` to leave the shell.

The same commands work in PowerShell. For `-it`, Windows Terminal works best.

Other ways in:

```bash
oci-hermit pull alpine                          # Docker Hub names work; anonymous pulls there are rate-limited
oci-hermit run alpine cat /etc/os-release
oci-hermit run -u nobody alpine id              # as another user than the image's
oci-hermit run --platform linux/arm64 alpine uname -m   # the other architecture
oci-hermit load -i app.tar                      # from `docker save` or an OCI layout, offline
oci-hermit run ./my-oci-layout sh               # an OCI layout directory or .tar, without storing it
```

Registry logins come from Podman or Docker: oci-hermit reads their auth files and credential helpers. Pulls happen on
the host. The sandbox itself never gets network access.

Set `OCI_HERMIT_TIMINGS=1` to see how long each phase of a `run` took. Booting dominates; on CI hosts a shell is
ready in 1 to 6 seconds (see [Measured start times](design.md#measured-start-times)).

`run` refuses the flags that are not implemented yet (`-d`, `--name`, `-p`, `-v`, `--pod`) instead of ignoring them.
The [roadmap](../ROADMAP.md) says when they come.

## Where it keeps things

| What | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Pulled and loaded images | `%LOCALAPPDATA%\oci-hermit` | `~/Library/Application Support/oci-hermit` | `$XDG_DATA_HOME/oci-hermit` or `~/.local/share/oci-hermit` |
| Each sandbox's logs | `%TMP%\oci-hermit\sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*` (or `/tmp`) |

`OCI_HERMIT_DATA_DIR` moves the image store. Nothing is written into the `oci-hermit` folder itself, so it can live
somewhere read-only.

## If it will not start

- **Windows, "Windows protected your PC" or a blocked file:** the browser marked the download. Unblock the folder
  with `Get-ChildItem -Recurse .\oci-hermit | Unblock-File`, if your organisation allows it. `gh` downloads are not
  marked.
- **macOS, "cannot be opened because the developer cannot be verified":** the browser quarantined the download.
  Remove the mark with `xattr -dr com.apple.quarantine oci-hermit`. Release binaries will be signed (#39).
- **Linux, "permission denied" on a file that is executable:** your home directory may be mounted `noexec`. Check
  with `findmnt -no OPTIONS --target ~`.
- **`machine boot` times out:** the console log in the sandbox's folder (above) shows how far the guest got. Security
  software that inspects QEMU can slow it down a lot; give it longer with `--timeout 600`.

On a managed (company) device, check that running an emulator is allowed first, and see
[Running without admin rights](no-admin.md) for what it needs and a checklist.

## Building from source

For working on oci-hermit itself. You need [pixi](https://pixi.sh), which installs per user.

```bash
git clone https://github.com/millsks/oci-hermit
cd oci-hermit
pixi run cargo build --release -p oci-hermit        # target/release/oci-hermit
```

On Windows, pixi's Rust links with Microsoft's linker, so Visual Studio Build Tools with the "Desktop development
with C++" workload must be installed.

The emulator and guest are separate builds:

| Piece | Build | Where |
| --- | --- | --- |
| QEMU bundle | `pixi run -e qemu-build qemu-build`, writes `qemu/out` | Linux and macOS; Windows builds under MSYS2 (`.github/workflows/qemu.yml`) |
| Guest kernel | `pixi run -e kernel-build kernel-build <arch> guest/out/<arch>` | Linux only, natively per architecture |
| Guest agent | `pixi run -e agent agent-build` | Linux only, natively per architecture |

You can also take any of them from a CI run instead (`oci-hermit-qemu-<host>` from the QEMU bundle workflow,
`sandbox-<arch>` from the Guest kernel workflow), or take the whole package above and replace only the binary.

Point a source build at them with two settings, or copy them next to the binary in the package layout:

```bash
export OCI_HERMIT_QEMU_DIR=$PWD/qemu/out                 # contains bin/qemu-system-*
export OCI_HERMIT_GUEST_DIR=$PWD/guest/out/sandbox       # contains <arch>/kernel and <arch>/hermit-agent
target/release/oci-hermit machine info
```

[CONTRIBUTING.md](../CONTRIBUTING.md) covers the development workflow.
