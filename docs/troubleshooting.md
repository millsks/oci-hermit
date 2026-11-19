# Troubleshooting

Start with these two commands. Most problems show up in one of them, with the reason:

```bash
oci-hermit machine info
oci-hermit machine boot
```

`machine info` says what oci-hermit found and where; `machine boot` boots a sandbox with no image involved. If both
work, the problem is with the image or the command. Every error oci-hermit prints starts with `oci-hermit:`.

## Where the logs are

Each `run` and `machine boot` gets its own folder under the run dir that `machine info` shows:

| OS | Run dir |
| --- | --- |
| Windows | `%TMP%\oci-hermit\sandbox-<pid>-<time>` |
| macOS | `$TMPDIR/oci-hermit/sandbox-<pid>-<time>` |
| Linux | `$TMPDIR/oci-hermit/sandbox-<pid>-<time>`, or under `/tmp` |

- `console.log`: the guest's console, the kernel's boot messages and the agent's `hermit-agent: …` lines. It shows
  how far the guest got.
- `qemu.log`: QEMU's own messages.
- `monitor.log`: for a `run`, what the container's monitor (the background `oci-hermit` that keeps its sandbox)
  could not do, such as listen for commands or record how the container ended.

`machine boot` prints the console log's path as it starts, and an error from a sandbox names both files. The
newest folder is the last run. oci-hermit does not delete these folders; remove old ones when you no longer need
them.

## It will not start at all

=== "Windows"

    **"Windows protected your PC" (SmartScreen), or the file is blocked.** A browser marks what it downloads with the
    Mark of the Web, and SmartScreen warns about unsigned programs that carry it. Release binaries will be signed
    (#39). Until then, unblock the folder, if your organisation allows it:

    ```powershell
    Get-ChildItem -Recurse .\oci-hermit | Unblock-File
    ```

    Or download with `gh run download` (see [Getting started](getting-started.md#1-download)), which does not mark
    files.

    **"This app has been blocked by your system administrator",** or nothing happens: application control
    (AppLocker, App Control for Business) or endpoint protection blocked `oci-hermit.exe`, `qemu-system-*.exe` or one
    of QEMU's DLLs. The Event Viewer, under *Applications and Services Logs > Microsoft > Windows > AppLocker* or
    *CodeIntegrity*, names the blocked file, and *Windows Security > Protection history* shows Defender's blocks. This
    needs your IT department; see [Running without admin rights](no-admin.md).

=== "macOS"

    **"cannot be opened because the developer cannot be verified", or "is damaged".** Gatekeeper refuses unsigned
    programs downloaded with a browser, which marks them as quarantined. Release binaries will be signed (#39). Until
    then, remove the mark from the folder:

    ```bash
    xattr -dr com.apple.quarantine oci-hermit
    ```

    Or download with `gh run download`, which does not set it. If *System Settings > Privacy & Security* has no
    *Open Anyway* button and `xattr` is not allowed, your organisation manages this; see
    [Running without admin rights](no-admin.md).

=== "Linux"

    **"Permission denied" on a file that is executable.** Your home directory may be mounted `noexec`:

    ```bash
    findmnt -no OPTIONS --target ~
    ```

    If the options include `noexec`, unpack oci-hermit somewhere that allows programs to run. `noexec` on the temp
    directory is fine: oci-hermit only reads from there. If the mount is not the cause, check for a policy denial:
    `fapolicyd`, SELinux (`ausearch -m avc -ts recent`) or AppArmor (`journalctl -k | grep apparmor`).

## QEMU or the guest is missing

```
oci-hermit: no bundled QEMU found; looked in …/libexec/oci-hermit/qemu …/qemu (set OCI_HERMIT_QEMU_DIR to a bundle built by qemu/build.sh)
```

The `qemu/` folder is not beside `oci-hermit`. Keep the package's folder together: the binary, `qemu/` and `guest/`.
Copying only `oci-hermit(.exe)` somewhere else breaks this. To keep them apart, set `OCI_HERMIT_QEMU_DIR` and
`OCI_HERMIT_GUEST_DIR` ([Configuration](configuration.md)).

```
oci-hermit: no x86_64 guest kernel, agent and crun found; looked in … (set OCI_HERMIT_GUEST_DIR to a directory with x86_64/kernel, x86_64/hermit-agent and x86_64/crun)
```

The guest for that architecture is missing, or it is one from before 0.3 without `crun`. `machine info` shows which
ones were found. A guest that is too old to run containers is refused with `the guest agent speaks control protocol
1.3 and cannot run containers; it needs 1.4 or later`: use the `guest/` folder that came with this `oci-hermit`.

A message saying a QEMU `is not the bundled QEMU` means the folder holds some other QEMU. oci-hermit runs only the
bundle it was built for, from the same package or CI run.

## The boot times out or is very slow

`machine boot` waits 120 seconds for the agent by default, and so does `run`. A timeout ends with
`the agent did not answer within …s` and the paths of both logs.

1. Look at the end of `console.log`. If the kernel's messages stop early, the guest is stuck or very slow; if the
   last line is `hermit-agent: ready …`, the agent started and something on the host side failed.
2. Try the other guest: `oci-hermit machine boot --arch aarch64` or `--arch x86_64`.
3. Give it longer: `oci-hermit machine boot --timeout 600`. If it then boots, the machine is just slow, or something
   is slowing QEMU down (next section).

`QEMU exited before the agent answered` means QEMU stopped; `qemu.log` says why.

### It is much slower than expected

Compare a boot with the [measured start times](performance.md#start-time). Endpoint protection often inspects
programs that generate code as they run, which QEMU does all the time, and that can make it many times slower. Look
for alerts from your security software naming `qemu-system-*`. Running oci-hermit from a folder IT has allowed, or
asking IT for an exclusion, is the fix; there is nothing to change in oci-hermit.

## Registry errors

| Message | What to do |
| --- | --- |
| `pulling the manifest of …: error sending request for url (https://…)` | The registry cannot be reached: check the name, the network and the proxy. |
| `… invalid peer certificate …` | The system does not trust the registry's certificate. If the registry uses your company's own CA, put that CA in the registry's `certs.d` folder: see [a registry with its own CA](field-manual/private-registry.md#a-registry-behind-a-company-certificate-authority). |
| `… (the registry may want a client certificate: …)` | The registry asks for a client certificate (mutual TLS). Put it and its key in the registry's `certs.d` folder: see [client certificates](field-manual/private-registry.md#a-registry-that-wants-a-client-certificate). |
| `… has no matching ….key` | A client certificate in a `certs.d` folder is missing its key (or the other way round). Copy both. |
| `pulling the manifest of …: 401 … (no login for … was found …)` | No login was found for that registry. Check the auth files in `machine info` and log in. See [Pull from a private registry](field-manual/private-registry.md). |
| `… the token server … answered 401` or `403 DENIED` | The login was refused, or it cannot see this repository. |
| `pulling the manifest of …: 404 MANIFEST_UNKNOWN` or `NAME_UNKNOWN` | Wrong repository or tag, or your login cannot see it. |
| `toomanyrequests` or `rate exceeded` | The registry is throttling you; Docker Hub limits anonymous pulls. oci-hermit already retried four times. Log in, wait, or use a mirror such as `public.ecr.aws/docker/library/…` for official images. |
| `… does not match its digest: the image is corrupt or has been altered` | A blob did not hash to its digest, so nothing was stored. Try again; if it repeats, something between you and the registry is changing the data. |

A `run` with an image that is not stored tries to pull it first (`Unable to find image '…' locally`). For an image
you loaded from a file, check the name with `oci-hermit images`, including the platform: asking for a platform the
store does not have also leads to a pull.

## The command fails to start

| Message | What it means |
| --- | --- |
| `the agent refused: starting <cmd>: executable file not found in $PATH` | The image has no such program. Small images have `sh`, not `bash`. |
| `user "<name>" is not in the image's /etc/passwd` | The image's `User`, or `-u`, names a user the image does not have. Use a number (`-u 1000:1000`) or a user it does have. |
| `the image has no entrypoint or command, and none was given` | Give a command after the image name. |
| `-p/--publish is not supported yet (#27); run it without -p/--publish` | The flag is planned, not implemented; the same for `-v` and `--pod`. |
| `the container's control socket … would be longer than the 103 bytes a socket path may have` | Linux and macOS: the data folder is too deep for the socket a container's monitor listens on. Set `OCI_HERMIT_DATA_DIR` to a shorter folder. |
| `the container's monitor stopped before it was ready: …` | The background `oci-hermit` that runs the container could not start it; the rest of the message, or `monitor.log`, says why. |
| `the container name "<name>" is already in use by container …` | A kept container has that name. Remove it (`oci-hermit rm <name>`), or choose another name; `--rm` stops runs from keeping their containers. |
| `container <name> is not running` | From `exec`: it runs commands only in a running container. Start one with `run -d`. |
| `the guest agent speaks control protocol 1.4, which has no exec; it needs 1.5 or later` | The container was started with an older `guest/` folder. Use the one that came with this `oci-hermit`, and start the container again. |
| `--memory 10m: use e.g. 512m or 2g, at least 64m` | `-m` needs a size with a unit, at least 64 MiB. |
| `unsupported --platform …` | Only `linux/amd64` and `linux/arm64` run. |

All of these make `run` (or `exec`) exit with 125.

## A container will not stop or go away

| Message | What to do |
| --- | --- |
| `container <name> is running; stop it first …` | `oci-hermit stop <name>`, or `oci-hermit rm -f <name>`. |
| `stop` takes 10 seconds | The command ignores `SIGTERM`, as a PID 1 without a handler does (`sleep`, many shell scripts), so `stop` waits out its time and then kills it. Use `stop -t 0`, handle `SIGTERM` in the command, or give the image a `StopSignal` it handles. |
| `the container's guest agent is too old to pass signals on …` | The container was started with a `guest/` folder from before 0.3: `kill` can only send `KILL` to it, and `stop` kills it at once. Start it again with the guest that came with this `oci-hermit`. |
| `could not stop container <name>: cannot reach its monitor: …` | The container's monitor is not answering. Find it (`ps aux \| grep __monitor`, or Task Manager on Windows: an `oci-hermit.exe` whose command line ends with the container's ID), end it, and its QEMU stops with it. The container then shows as `Exited (unknown)` and `rm` removes it. |

On Windows, `id=$(oci-hermit run -d …)` and other pipes from a `run -d` do not finish while the container runs,
because its monitor inherits the pipe (#105). Redirect the output to a file instead: `oci-hermit run -d … > id.txt`.

A `run` without `-d` takes its container down with it: closing the terminal or killing that `run` (`kill -9`) stops
the container, which then shows as `Exited (137)`. Ctrl-C does not: it is passed on to the command, which may ignore
it (see [run](cli.md#run)). Use `-d` for a container that should keep running.

## The image store is busy

```
oci-hermit: the image store is in use by another oci-hermit (a pull, load or run); try again when it has finished
```

`rmi` and `image prune` delete blobs, so they wait for no one: they refuse while another oci-hermit is reading the
store. Let the other command finish and run them again. If no other oci-hermit is running, check for a stuck process
(Task Manager, or `ps aux | grep oci-hermit`).

## The command cannot reach the network

That is by design: the sandbox has no network device, and nothing turns it on. Put what the command needs into the
image beforehand. See [How it works](how-it-works.md#no-network-on-purpose).

## Asking for help

Open an [issue](https://github.com/millsks/oci-hermit/issues) with the output of `oci-hermit version` and
`oci-hermit machine info`, the command you ran, the full error, and the end of `console.log` and `qemu.log`. Leave
out anything your organisation treats as confidential. A security problem goes through the
[security policy](security.md) instead.
