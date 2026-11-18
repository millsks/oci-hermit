# Run a public image and get a shell

**When:** you want to look inside an image, try a command in it, or check that it starts, and the image is in a
public registry.

## 1. Check the sandbox works

```bash
oci-hermit machine boot
```

It prints `agent ready after …s` and `guest powered off; sandbox stopped`. If it does not, start with
[Troubleshooting](../troubleshooting.md): nothing below works until this does.

## 2. Run one command

`run` takes an image reference and a command. If the image is not stored yet, it is pulled first:

```console
$ oci-hermit run public.ecr.aws/docker/library/alpine:3.20 cat /etc/os-release
Unable to find image 'public.ecr.aws/docker/library/alpine:3.20' locally
ab3fe4defd29: pulled (628 B)
3f26bc2dec0b: pulled (4.1 MB)
Digest: sha256:45e09956dc667c5eff3583c9d94830261fb1ca0be10a0a7db36266edf5de9e1d
NAME="Alpine Linux"
ID=alpine
VERSION_ID=3.20.10
PRETTY_NAME="Alpine Linux v3.20"
HOME_URL="https://alpinelinux.org/"
BUG_REPORT_URL="https://gitlab.alpinelinux.org/alpine/aports/-/issues"
```

The pull messages go to stderr, the command's output to stdout. The next `run` of the same image finds it in the
store and starts at once. Without a command, `run` runs the image's own entrypoint and command, as Docker does.

oci-hermit exits with the command's exit code, so it works in scripts:

=== "macOS and Linux"

    ```console
    $ oci-hermit run public.ecr.aws/docker/library/alpine:3.20 sh -c 'exit 7'; echo $?
    7
    ```

=== "Windows"

    ```console
    PS> oci-hermit run public.ecr.aws/docker/library/alpine:3.20 sh -c 'exit 7'; $LASTEXITCODE
    7
    ```

If oci-hermit itself fails (the image cannot be found, the sandbox does not boot), it exits with 125, as Docker does.

## 3. Get a shell

`-i` keeps stdin open and `-t` gives the command a terminal; together they give you an interactive shell:

```bash
oci-hermit run -it public.ecr.aws/docker/library/debian:bookworm-slim bash
```

Type `exit` (or press Ctrl-D) to leave; the sandbox powers off. Alpine and other small images have `sh` instead of
`bash`.

- On Windows, use Windows Terminal for `-it`; the older console host works but handles keys less well.
- The terminal size is sent when the shell starts. Resizing the window afterwards is not passed on yet.

## 4. Pass input in

With `-i` and no `-t`, stdin is a pipe, so you can feed a file or another command's output to the container:

=== "macOS and Linux"

    ```bash
    echo 'hello from the host' | oci-hermit run -i public.ecr.aws/docker/library/alpine:3.20 cat
    ```

=== "Windows"

    ```powershell
    'hello from the host' | oci-hermit run -i public.ecr.aws/docker/library/alpine:3.20 cat
    ```

Without `-i`, the command's stdin is closed at once.

## 5. Run as another user

The command runs as the image's `User`, as Docker runs it. `-u` picks another, by name or number, with an optional
group:

```console
$ oci-hermit run -u nobody public.ecr.aws/docker/library/alpine:3.20 id
uid=65534(nobody) gid=65534(nobody)
```

Names are looked up in the image's own `/etc/passwd` and `/etc/group`, so a name the image does not have is an error.
`-u 1000:1000` works without either file.

## 6. Give it more room

Each `run` gets one emulated CPU and 512 MiB of memory: the image's layers are read-only disks, so its size does not matter. For a heavier
command, ask for more:

```console
$ oci-hermit run --cpus 2 -m 1g public.ecr.aws/docker/library/alpine:3.20 sh -c 'nproc; grep MemTotal /proc/meminfo'
2
MemTotal:        1020516 kB
```

See [Performance and limits](../performance.md) for what to expect from emulation.

## What it cannot do yet

Every `run` is a fresh sandbox, gone when the command ends; there are no containers to come back to. These flags are
refused with the issue that adds them, rather than ignored:

```console
$ oci-hermit run -d public.ecr.aws/docker/library/alpine:3.20
oci-hermit: -d/--detach is not supported yet (#22); run it without -d/--detach
```

| Flag | Issue |
| --- | --- |
| `-d`, `--detach` | #22 |
| `--name` | #20 |
| `-p`, `--publish` | #27 |
| `-v`, `--volume` | #33 |
| `--pod` | #35 |

`--rm` is accepted and changes nothing, because nothing is kept after a `run` anyway. The sandbox has no network,
so a command that downloads something fails (see [How it works](../how-it-works.md#no-network-on-purpose)).
