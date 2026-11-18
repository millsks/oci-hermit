# Getting started

This page takes you from nothing to a shell in a Linux image on Windows, macOS or Linux. None of it needs admin
rights, a hypervisor or WSL. Pick your operating system in any of the tabs below; the others follow.

## What you get

One file per kind of machine, from the [releases page](https://github.com/millsks/oci-hermit/releases). Each unpacks
into one folder that runs from wherever you put it:

```
oci-hermit/
  oci-hermit(.exe)        the host tool
  hrmt(.exe)              the same program under a shorter name
  qemu/                   the bundled emulator (QEMU, TCG only)
  guest/x86_64/           the guest kernel and agent, for x86_64 (amd64) images
  guest/aarch64/          the same for arm64 images
  LICENSE                 oci-hermit's license (Apache-2.0)
  THIRD-PARTY-LICENSES    every bundled component and its license
  README.txt
```

oci-hermit finds `qemu/` and `guest/` next to itself, so there is nothing to configure. Both guests run on every
host, because the CPU is emulated: an arm64 image runs on an Intel PC, and an amd64 image runs on a Mac with Apple
silicon.

| Your machine | File |
| --- | --- |
| Windows, x64 | `oci-hermit-<version>-win-64.zip` |
| macOS, Apple silicon | `oci-hermit-<version>-osx-arm64.tar.gz` |
| macOS, Intel | `oci-hermit-<version>-osx-64.tar.gz` |
| Linux, x86-64 | `oci-hermit-<version>-linux-64.tar.gz` |
| Linux, arm64 | `oci-hermit-<version>-linux-aarch64.tar.gz` |

Releases below 1.0 are marked as pre-releases. The binaries are not signed yet (#39), so Windows and macOS warn the
first time; see [If it will not start](#if-it-will-not-start).

## 1. Download

**With a browser:** open the [releases page](https://github.com/millsks/oci-hermit/releases), and under **Assets** of
the newest release download the file for your machine. No GitHub account is needed. Unpack it: on Windows,
right-click the `.zip` and choose **Extract All**; on macOS, double-click the `.tar.gz`.

**From a terminal**, with the GitHub CLI (`gh`):

=== "Windows"

    In PowerShell:

    ```powershell
    $tag = gh release list -R millsks/oci-hermit -L 1 --json tagName -q '.[0].tagName'
    gh release download $tag -R millsks/oci-hermit -p '*-win-64.zip' -p SHA256SUMS
    $zip = Get-Item oci-hermit-*-win-64.zip
    $want = (Select-String -Path SHA256SUMS -Pattern $zip.Name -SimpleMatch).Line.Split(' ')[0]
    if ((Get-FileHash $zip -Algorithm SHA256).Hash -eq $want) { 'OK' } else { 'MISMATCH' }
    Expand-Archive $zip -DestinationPath .
    ```

=== "macOS"

    Use `osx-arm64` on Apple silicon and `osx-64` on an Intel Mac:

    ```bash
    tag=$(gh release list -R millsks/oci-hermit -L 1 --json tagName -q '.[0].tagName')
    gh release download "$tag" -R millsks/oci-hermit -p '*-osx-arm64.tar.gz' -p SHA256SUMS
    shasum -a 256 --ignore-missing -c SHA256SUMS
    tar -xzf oci-hermit-*-osx-arm64.tar.gz
    ```

=== "Linux"

    Use `linux-64` on x86-64 and `linux-aarch64` on arm64:

    ```bash
    tag=$(gh release list -R millsks/oci-hermit -L 1 --json tagName -q '.[0].tagName')
    gh release download "$tag" -R millsks/oci-hermit -p '*-linux-64.tar.gz' -p SHA256SUMS
    sha256sum --ignore-missing -c SHA256SUMS
    tar -xzf oci-hermit-*-linux-64.tar.gz
    ```

`gh release list` takes the newest release, pre-releases included; `gh release download` alone would skip those.
Each tab checks the download against `SHA256SUMS` before unpacking it; anything but `OK` means a damaged or
different file.

**If the download never finishes.** On some managed networks a browser download from the releases page stalls, going
back and forth between 0 and 1 byte and resuming without end. GitHub serves the files from a second host,
`release-assets.githubusercontent.com`, through short-lived links, and a proxy or scanner that holds the file back
until it has seen all of it can outlast the link. Download it on another machine and copy it across (a USB stick or
a file share is fine), then check it with `SHA256SUMS` as above. The folder needs nothing from the internet to run.
To see what the network is doing, `curl.exe -L -v -o oci-hermit.zip <link to the file>` prints each step.

You now have an `oci-hermit` folder. The commands below run from the folder that contains it.

## 2. Check it

=== "Windows"

    ```powershell
    .\oci-hermit\oci-hermit.exe version
    .\oci-hermit\oci-hermit.exe machine info
    .\oci-hermit\oci-hermit.exe machine boot
    ```

=== "macOS"

    ```bash
    ./oci-hermit/oci-hermit version
    ./oci-hermit/oci-hermit machine info
    ./oci-hermit/oci-hermit machine boot
    ```

=== "Linux"

    ```bash
    ./oci-hermit/oci-hermit version
    ./oci-hermit/oci-hermit machine info
    ./oci-hermit/oci-hermit machine boot
    ```

`version` prints the version and the control protocol the host speaks:

```console
$ oci-hermit version
oci-hermit 0.1.0 (control protocol 1.2)
```

`machine info` lists both emulators with `accel tcg`, both guests, the defaults `run` uses, where images are stored
and how many there are, and which registry logins it found. Each path says whether it came from a setting or was
found beside `oci-hermit`. On a Mac with Apple silicon it looks like this (paths shortened):

```console
$ oci-hermit machine info
oci-hermit 0.1.0 (control protocol 1.2)
host: macos/aarch64, so images default to linux/arm64

emulator (QEMU, software CPU only):
  bundle:     /Users/me/oci-hermit/qemu (found beside oci-hermit)
  x86_64:     QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg
  aarch64:    QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg

guest (kernel and agent each sandbox boots):
  x86_64:     /Users/me/oci-hermit/guest/x86_64 (kernel 2.9 MB, agent 1.3 MB, found beside oci-hermit)
  aarch64:    /Users/me/oci-hermit/guest/aarch64 (kernel 5.9 MB, agent 1.2 MB, found beside oci-hermit)

sandbox defaults for run:
  cpus:       1 (--cpus)
  memory:     twice the image's size plus 256 MiB, at least 512 MiB (--memory)
  network:    none: no egress; -p (127.0.0.1 only) is not implemented yet
  run dir:    /var/folders/kb/…/T/oci-hermit (a sandbox-* folder per run: initramfs, console.log, qemu.log)

image store (pull, load):
  path:       /Users/me/Library/Application Support/oci-hermit/images (the platform default)
  images:     none yet; created by the first pull or load

registries (pulls happen on the host, never in the sandbox):
  auth files: searched in this order; the first with a login for the registry is used
    1. [not set]   $REGISTRY_AUTH_FILE
    2. [not set]   $XDG_RUNTIME_DIR/containers/auth.json
    3. [not found] /Users/me/.config/containers/auth.json
    4. [found]     /Users/me/.docker/config.json
  plain http: none (OCI_HERMIT_INSECURE_REGISTRIES)
```

`machine boot` starts a sandbox and stops it again, a self-check of QEMU, the kernel and the agent:

```console
$ oci-hermit machine boot
booting an aarch64 sandbox; console log: /var/folders/kb/…/T/oci-hermit/sandbox-12655-1794883275062816000/console.log
agent ready after 0.3s (control protocol 1.2)
guest powered off; sandbox stopped
```

Add `--arch x86_64` or `--arch aarch64` to try the other guest.

### Put it on your PATH

To type `oci-hermit` without the folder in front, add the folder to your `PATH`:

=== "Windows"

    For this PowerShell window only:

    ```powershell
    $env:Path = "$PWD\oci-hermit;$env:Path"
    ```

    For good, for your account only (no admin rights needed); open a new window afterwards:

    ```powershell
    [Environment]::SetEnvironmentVariable("Path", "$PWD\oci-hermit;" + [Environment]::GetEnvironmentVariable("Path", "User"), "User")
    ```

=== "macOS"

    For this terminal only:

    ```bash
    export PATH="$PWD/oci-hermit:$PATH"
    ```

    For good, add the same line, with the full path instead of `$PWD`, to `~/.zshrc`.

=== "Linux"

    For this terminal only:

    ```bash
    export PATH="$PWD/oci-hermit:$PATH"
    ```

    For good, add the same line, with the full path instead of `$PWD`, to `~/.bashrc` (or your shell's startup
    file).

The rest of the documentation assumes you did.

## 3. Run an image

```bash
oci-hermit run public.ecr.aws/docker/library/debian:bookworm-slim sh -c 'uname -a; cat /etc/debian_version'
oci-hermit run -it public.ecr.aws/docker/library/debian:bookworm-slim bash
```

The first `run` pulls the image, because it is not stored yet. The second one finds it:

```console
$ oci-hermit run public.ecr.aws/docker/library/debian:bookworm-slim sh -c 'uname -a; cat /etc/debian_version'
Unable to find image 'public.ecr.aws/docker/library/debian:bookworm-slim' locally
5b6c2c6cbc90: pulled (468 B)
2d66c782e484: pulled (28.1 MB)
Digest: sha256:ec8534cda2cb30f29f44f23e135297af7a1496f2990fd8e4e84aa36b081a757a
Linux (none) 6.18.63-oci-hermit-2 #1 SMP 2026-11-10 18:07:06 UTC aarch64 GNU/Linux
12.15
```

`uname -a` shows oci-hermit's Linux kernel, whatever your host is. In the shell, type `exit` to leave.

The same commands work in PowerShell. For `-it`, Windows Terminal works best.

Other ways in:

```bash
oci-hermit pull alpine                          # Docker Hub names work; anonymous pulls there are rate-limited
oci-hermit run alpine cat /etc/os-release
oci-hermit run -u nobody alpine id              # as another user than the image's
oci-hermit run --platform linux/amd64 alpine uname -m   # the other architecture
oci-hermit load -i app.tar                      # from `docker save` or an OCI layout, offline
oci-hermit run ./my-oci-layout sh               # an OCI layout directory or .tar, without storing it
```

Pulls happen on the host. The sandbox itself never gets network access.

### Private registries

Log in once, then pull and run as usual:

```bash
oci-hermit login registry.example.com          # asks for the user name and password
oci-hermit pull registry.example.com/team/app:1.4
```

- `login` checks the user name and password with the registry before it saves anything, so a typo is reported
  straight away. With no registry it logs in to Docker Hub.
- In scripts and CI, pass the password on stdin rather than as an argument, where other users could see it:
  `echo "$TOKEN" | oci-hermit login -u me --password-stdin ghcr.io`. Registries such as GHCR take an access token
  as the password.
- The login is stored where Podman stores it (`$REGISTRY_AUTH_FILE`, else `$XDG_RUNTIME_DIR/containers/auth.json`,
  else `~/.config/containers/auth.json`), readable by you only. Podman and skopeo use it too, and logins you already
  made with `podman login` or `docker login` work in oci-hermit without logging in again. `machine info` lists
  every auth file it reads and the one `login` writes.
- `oci-hermit logout registry.example.com` removes the login; `logout --all` removes them all.
- More, including tokens for GHCR, GitLab and Harbor: [Pull from a private registry](field-manual/private-registry.md).
- Registry certificates are checked against your operating system's trust store, so a company certificate
  authority that IT installed works. Per-registry certificates (`certs.d`) are #86.

Set `OCI_HERMIT_TIMINGS=1` to see how long each phase of a `run` took. Booting dominates; on CI hosts a shell is
ready in 1 to 6 seconds (see [Performance and limits](performance.md)).

`run` refuses the flags that are not implemented yet (`-d`, `--name`, `-p`, `-v`, `--pod`) instead of ignoring them.
The [roadmap](../ROADMAP.md) says when they come.

## Managing images

The image commands work as in Podman and Docker. They only touch the image store, so they need no sandbox:

```bash
oci-hermit images                                 # or `image ls`; -q for IDs, --digests, --no-trunc
oci-hermit tag alpine mine/alpine:test            # another name for the same image
oci-hermit inspect alpine                         # config, layers and names as JSON
oci-hermit image history alpine                   # the steps that built it
oci-hermit image exists alpine && echo stored     # exit 0 if stored, 1 if not
oci-hermit save -o alpine.tar alpine              # every platform stored; --platform for one
oci-hermit rmi mine/alpine:test                   # removes the name; blobs go when nothing uses them
oci-hermit image prune                            # images loaded without a name; -a for every image
oci-hermit system df                              # how much space the store uses
```

- **Names and IDs:** images go by their full name (`docker.io/library/alpine:latest`; `alpine` means the same) or by
  their image ID, the first few characters of which are enough. `run` takes the same names and IDs.
- **Platforms:** an image pulled for both platforms shows up once per platform. `rmi` and `tag` act on every
  platform under the name.
- **Moving images:** `save` writes the format Docker 25 and later write: an OCI image layout with a `manifest.json`
  beside it. `oci-hermit load`, `docker load`, `podman load` and skopeo all read it, so it carries images to another
  machine with no registry in between.
- **One at a time:** `rmi` and `image prune` refuse to run while another `pull`, `load` or `run` is reading the store,
  so they never delete an image that is in use. Try again when it has finished.

[Manage the image store](field-manual/image-store.md) walks through them, and
[Move images between machines](field-manual/move-images.md) covers `save` and `load`.

## Where it keeps things

| What | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Pulled and loaded images | `%LOCALAPPDATA%\oci-hermit` | `~/Library/Application Support/oci-hermit` | `$XDG_DATA_HOME/oci-hermit` or `~/.local/share/oci-hermit` |
| Each sandbox's logs | `%TMP%\oci-hermit\sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*` (or `/tmp`) |

`oci-hermit machine info` shows the ones in use. `OCI_HERMIT_DATA_DIR` moves the image store. Nothing is written into
the `oci-hermit` folder itself, so it can live somewhere read-only. [Configuration](configuration.md) lists every
setting.

## If it will not start

=== "Windows"

    **"Windows protected your PC", or a blocked file:** the browser marked the download (the Mark of the Web).
    Unblock the folder, if your organisation allows it:

    ```powershell
    Get-ChildItem -Recurse .\oci-hermit | Unblock-File
    ```

    `gh` downloads are not marked.

=== "macOS"

    **"cannot be opened because the developer cannot be verified":** the browser quarantined the download. Remove
    the mark:

    ```bash
    xattr -dr com.apple.quarantine oci-hermit
    ```

    Release binaries will be signed (#39).

=== "Linux"

    **"permission denied" on a file that is executable:** your home directory may be mounted `noexec`. Check:

    ```bash
    findmnt -no OPTIONS --target ~
    ```

**`machine boot` times out:** the console log in the sandbox's folder (above) shows how far the guest got. Security
software that inspects QEMU can slow it down a lot; give it longer with `--timeout 600`.

[Troubleshooting](troubleshooting.md) covers more. On a managed (company) device, check that running an emulator is
allowed first, and see [Running without admin rights](no-admin.md) for what it needs and a checklist.

## Development builds

Every successful CI run on `main` also publishes the same folder for each host, built from that commit. GitHub
keeps these for 14 days, and only signed-in GitHub users can download them. With `gh`:

```bash
run=$(gh run list -R millsks/oci-hermit -w kernel.yml -b main -s success -L 1 --json databaseId -q '.[0].databaseId')
gh run download "$run" -R millsks/oci-hermit -n oci-hermit-osx-arm64   # or -win-64, -osx-64, -linux-64, -linux-aarch64
```

On Windows this leaves the `oci-hermit` folder; elsewhere it leaves `oci-hermit-<host>.tar.gz` to unpack. In a
browser: the [Guest kernel workflow](https://github.com/millsks/oci-hermit/actions/workflows/kernel.yml?query=branch%3Amain+is%3Asuccess),
the newest run, **Artifacts**.

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
