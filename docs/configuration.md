# Configuration

oci-hermit has no configuration file. It finds what it needs beside itself, keeps images in your data folder, and
reads a few environment variables. `oci-hermit machine info` shows the result.

## Environment variables

| Variable | What it does | Default |
| --- | --- | --- |
| `OCI_HERMIT_DATA_DIR` | Where the image store lives (in `images/` under it). | The platform's per-user data folder: see [Where things are kept](#where-things-are-kept). |
| `OCI_HERMIT_QEMU_DIR` | A QEMU bundle to use, a folder with `bin/qemu-system-x86_64` and `bin/qemu-system-aarch64`. Searched first. | `qemu/` beside `oci-hermit`, or `<prefix>/libexec/oci-hermit/qemu` for a prefix install. |
| `OCI_HERMIT_GUEST_DIR` | A guest to use, a folder with `x86_64/kernel`, `x86_64/hermit-agent`, `aarch64/kernel` and `aarch64/hermit-agent`. Searched first. | `guest/` beside `oci-hermit`, or `<prefix>/libexec/oci-hermit/guest`. |
| `OCI_HERMIT_INSECURE_REGISTRIES` | Registries to reach over plain HTTP, as `host:port`, comma separated. | None: every registry needs HTTPS. |
| `OCI_HERMIT_TIMINGS` | Anything but empty or `0` makes `run` print how long each phase took, on stderr. | Off. |
| `REGISTRY_AUTH_FILE` | An auth file to read first, as Podman does. | Not set. |
| `XDG_RUNTIME_DIR` | Podman's runtime auth file is `$XDG_RUNTIME_DIR/containers/auth.json`. | Set by the system on most Linux desktops. |
| `XDG_CONFIG_HOME` | Podman's auth file is `$XDG_CONFIG_HOME/containers/auth.json`. | `~/.config` |
| `DOCKER_CONFIG` | Docker's auth file is `$DOCKER_CONFIG/config.json`. | `~/.docker` |
| `XDG_DATA_HOME` | Linux only: the image store is `$XDG_DATA_HOME/oci-hermit`. | `~/.local/share` |
| `HOME`, `USERPROFILE`, `LOCALAPPDATA` | Where the defaults above start. `USERPROFILE` stands in for `HOME` on Windows. | Set by the system. |
| `TMPDIR` (`TMP` on Windows) | Where each sandbox's folder goes, in `oci-hermit/`. | The system's temp folder. |
| `TERM` | Passed to a command run with `-t`, unless the image sets its own. | `xterm` |
| `PATH` | Where credential helpers (`docker-credential-*`) are found. | |

An empty value counts as not set.

Set a variable for one command or for the session:

=== "macOS and Linux"

    ```bash
    OCI_HERMIT_DATA_DIR=/tmp/scratch-store oci-hermit images     # one command
    export OCI_HERMIT_DATA_DIR=/tmp/scratch-store                # this session
    ```

=== "Windows"

    ```powershell
    $env:OCI_HERMIT_DATA_DIR = "$env:TEMP\scratch-store"         # this session
    [Environment]::SetEnvironmentVariable("OCI_HERMIT_DATA_DIR", "D:\oci-hermit", "User")   # for good, new windows
    ```

## Where things are kept

| What | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Image store | `%LOCALAPPDATA%\oci-hermit\images` | `~/Library/Application Support/oci-hermit/images` | `$XDG_DATA_HOME/oci-hermit/images`, or `~/.local/share/oci-hermit/images` |
| Each sandbox's initramfs and logs | `%TMP%\oci-hermit\sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*` | `$TMPDIR/oci-hermit/sandbox-*`, or `/tmp/oci-hermit/sandbox-*` |
| QEMU, kernel and agent | the `qemu/` and `guest/` folders beside `oci-hermit` | the same | the same |
| A registry's own CAs and client certificate | `~\.config\containers\certs.d\<host><port>\`, or Docker's | `~/.config/containers/certs.d/<host:port>/`, or `/etc/containers/certs.d`, or Docker's | the same as macOS |

oci-hermit never writes beside itself, so its folder can be read-only. It does not delete old `sandbox-*` folders:
each holds the initramfs (about the image's size) and the logs of one `run` or `machine boot`. Delete them when you
no longer need the logs.

The image store is an [OCI image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md):
`index.json` names the images, `blobs/sha256/` holds their contents, and `lock` and `index.lock` coordinate several
oci-hermit processes. Change it only through oci-hermit's commands.

Each sandbox folder holds:

| File | What it is |
| --- | --- |
| `initrd.cpio` | The initramfs the guest boots: the agent as `/init`, and for `run` the image's files under `/rootfs`. |
| `console.log` | The guest's serial console: the kernel's boot messages and the agent's `hermit-agent: …` lines. |
| `qemu.log` | QEMU's own messages (its stderr). |

## How oci-hermit finds QEMU and the guest

For each, the first folder that has everything wins:

1. the folder named by `OCI_HERMIT_QEMU_DIR` (or `OCI_HERMIT_GUEST_DIR`);
2. `<prefix>/libexec/oci-hermit/qemu` (or `…/guest`), when `oci-hermit` is installed as `<prefix>/bin/oci-hermit`,
   as a conda or `pixi global` package will be;
3. `qemu/` (or `guest/`) beside the `oci-hermit` executable, as in the downloaded package.

A QEMU found this way must be the bundle oci-hermit was built for: `--version` must report `11.1.3
(oci-hermit-qemu-4)` and offer the `tcg` accelerator, and the x86_64 firmware must be there. Any other QEMU, even the
same release from elsewhere, is refused. A missing guest for one architecture only stops images of that architecture.

## What machine info shows

```console
$ oci-hermit machine info
oci-hermit 0.1.0 (control protocol 1.2)
host: macos/aarch64, so images default to linux/arm64
```

The version, and the host's OS and architecture, which decide the default image platform.

```console
emulator (QEMU, software CPU only):
  bundle:     /Users/me/oci-hermit/qemu (found beside oci-hermit)
  x86_64:     QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg
  aarch64:    QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg
```

The QEMU bundle, and where it was found: `from OCI_HERMIT_QEMU_DIR` or `found beside oci-hermit`. Each emulator's
version and accelerator. A problem here makes `machine info` exit 1, with the reason on stderr.

```console
guest (kernel and agent each sandbox boots):
  x86_64:     /Users/me/oci-hermit/guest/x86_64 (kernel 2.9 MB, agent 1.3 MB, found beside oci-hermit)
  aarch64:    /Users/me/oci-hermit/guest/aarch64 (kernel 5.9 MB, agent 1.2 MB, found beside oci-hermit)
```

Each guest, its sizes and where it was found. A missing one says `not found; <platform> images cannot run`.

```console
sandbox defaults for run:
  cpus:       1 (--cpus)
  memory:     twice the image's size plus 256 MiB, at least 512 MiB (--memory)
  network:    none: no egress; -p (127.0.0.1 only) is not implemented yet
  run dir:    /var/folders/kb/…/T/oci-hermit (a sandbox-* folder per run: initramfs, console.log, qemu.log)
```

What `run` gives a sandbox unless told otherwise, and where the sandbox folders go.

```console
image store (pull, load):
  path:       /Users/me/Library/Application Support/oci-hermit/images (the platform default)
  images:     3 images under 2 names (one per name and platform)
  size:       35.9 MB
```

The store, whether its path is `the platform default` or `from OCI_HERMIT_DATA_DIR`, and what it holds.

```console
registries (pulls happen on the host, never in the sandbox):
  auth files: searched in this order; the first with a login for the registry is used
    1. [not set]   $REGISTRY_AUTH_FILE
    2. [not set]   $XDG_RUNTIME_DIR/containers/auth.json
    3. [not found] /Users/me/.config/containers/auth.json
    4. [found]     /Users/me/.docker/config.json
  plain http: none (OCI_HERMIT_INSECURE_REGISTRIES)
```

Every auth file, in the order they are read: `[not set]` when the variable it needs is not set, `[not found]` when
the file does not exist, `[found]` when it does. Then the registries reached over plain HTTP. See
[Pull from a private registry](field-manual/private-registry.md).
