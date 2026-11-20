# Command-line reference

```
oci-hermit <COMMAND> [OPTIONS]
```

Every command takes `-h` or `--help`. `oci-hermit --version` (or `-V`) prints the version. The commands follow Docker
and Podman; the [deliberate differences](docker-and-podman.md) are on their own page.

`hrmt` is the same program under a shorter name: every package has it beside `oci-hermit` (a symlink on Linux and
macOS, a copy on Windows), and `hrmt --help` says `hrmt`. A `hrmt` link put elsewhere, such as `~/bin`, still finds
the `qemu/` and `guest/` folders beside the real file.

| Command | What it does |
| --- | --- |
| [`run`](#run) | Run a command from an image in a new sandbox |
| [`exec`](#exec) | Run another command in a running container |
| [`ps`](#ps) | List containers |
| [`logs`](#logs) | Show a container's output |
| [`stop`](#stop) | Stop running containers |
| [`kill`](#kill) | Send a signal to running containers |
| [`wait`](#wait) | Wait for containers to stop and print their exit codes |
| [`attach`](#attach) | Connect this terminal to a running container |
| [`cp`](#cp) | Copy files between a running container and this machine |
| [`rm`](#rm) | Remove containers |
| [`pull`](#pull) | Pull an image from a registry |
| [`manifest inspect`](#manifest-inspect) | Show an image's platforms and digests in a registry, without pulling |
| [`load`](#load) | Load images from a `docker save` archive or an OCI layout |
| [`login`](#login) | Log in to a registry |
| [`logout`](#logout) | Remove a registry login |
| [`images`, `image ls`](#images) | List stored images |
| [`inspect`, `image inspect`](#inspect) | Show details of containers and images as JSON |
| [`tag`, `image tag`](#tag) | Give an image another name |
| [`save`, `image save`](#save) | Save images to an archive |
| [`rmi`, `image rm`](#rmi) | Remove images |
| [`image prune`](#image-prune) | Remove unnamed images, or all of them |
| [`image history`](#image-history) | Show the steps that built an image |
| [`image exists`](#image-exists) | Exit 0 if an image is stored, 1 if not |
| [`system df`](#system-df) | Show how much space images and containers use |
| [`container prune`](#container-prune) | Remove every stopped container |
| [`machine info`, `info`](#machine-info) | Show what oci-hermit uses and where it keeps things |
| [`machine boot`](#machine-boot) | Boot a sandbox until its agent is ready, then stop it |
| [`version`](#version) | Print version information |

Commands that are accepted but do nothing yet are listed under [Planned commands](#planned-commands).

## Images and names

Wherever a command takes an image, it takes:

- **a name**: a full reference (`docker.io/library/alpine:latest`, `ghcr.io/org/app:1.2`) or one Docker would expand
  (`alpine` is `docker.io/library/alpine:latest`; `mine/app:1` is `docker.io/mine/app:1`);
- **an image ID**: the digest of the image's config, with or without `sha256:`, or a unique prefix of it (`ab3f`).

`run` also takes the path of an OCI layout directory or a `.tar` (an OCI archive or a `docker save` archive). A path
that exists is read as a path, before any stored name.

## Platforms

`--platform` names an image platform: `linux/amd64` or `linux/arm64`. `run` and `pull` also accept the short forms
`amd64`, `arm64`, `x86_64` and `aarch64`. Without it, the host's platform is used. See
[Choose a platform](field-manual/platforms.md).

## run

Run a command from an image in a new sandbox, and exit with the command's exit code.

```
oci-hermit run [OPTIONS] <IMAGE> [COMMAND]...
```

| Argument or option | Meaning |
| --- | --- |
| `IMAGE` | A name or ID in the store (pulled if it is not there), or the path of an OCI layout directory or `.tar`. |
| `COMMAND...` | The command and its arguments. Without one, the image's entrypoint and command run. Everything after `IMAGE` belongs to the command, including options. |
| `-i`, `--interactive` | Keep stdin open and pass it to the command. Without it, the command's stdin is closed at once. |
| `-t`, `--tty` | Give the command a terminal. With `-i`, an interactive shell. The terminal size is sent at the start; later resizes are not passed on yet. `TERM` is passed in from the host (`xterm` if unset) unless the image sets it. |
| `-u`, `--user <USER>` | Run as this user instead of the image's `User`: `name`, `uid`, `name:group` or `uid:gid`. Names are looked up in the image's `/etc/passwd` and `/etc/group`. |
| `--platform <PLATFORM>` | The image platform. Without it, the host's, or the other one with a warning if the image has only that. |
| `--cpus <CPUS>` | The CPUs the container may use, as Docker's: a decimal number from `0.01` up to the host's CPU count (`0` is no limit). The sandbox gets an emulated CPU for each one started (`1.5` gives 2), and the container's cgroup holds it to the number given. Without it, 1 emulated CPU and no limit. |
| `-m`, `--memory <MEMORY>` | The memory the container may use, as Docker's: a number with `k`, `m` or `g` (`512m`, `2g`); bytes without a suffix. At least `6m`. The container's cgroup holds it to that (more is an out-of-memory kill, exit 137), and the sandbox gets 128 MiB more for the guest, plus room for an image run from a file. Without it, no limit, and the sandbox has 512 MiB for a stored image, which runs from its layer disks, or twice the size of one run from a file plus 256 MiB. |
| `--name <NAME>` | The container's name: a letter or digit, then letters, digits, `_`, `.` or `-`. Without one, a name such as `quiet_nautilus` is made up. A name stays taken until the container is removed. |
| `--rm` | Remove the container when the command ends. Without it the container is kept, exited, for `ps -a`, until `rm`. |
| `-d`, `--detach` | Run in the background: print the container's ID once its command started, and leave it running until it exits or is stopped. See [logs](#logs) and [stop](#stop). With `-i`, its stdin stays open, so `-dit … sh` keeps a shell waiting for [attach](#attach). |
| `-p`, `--publish <PUBLISH>` | Planned (#27): `[127.0.0.1:]HOST:CONTAINER`, always bound to 127.0.0.1. Refused today. |
| `-v`, `--volume <VOLUME>` | Planned (#33): `SRC:DST`, with `:ro` or `:rw`; read-only by default. Refused today. |
| `--pod <POD>` | Planned (#35). Refused today. |

The command runs in the image's working directory, with the image's environment, plus `HOME` from the user's
`/etc/passwd` entry if the image does not set it.

Every `run` is recorded as a container (see [ps](#ps)), with its exit code, or with 125 and the reason if
oci-hermit could not run it. Only the record and the output (see [logs](#logs)) are kept: the files the command
changed are thrown away with the sandbox.

Each container's sandbox belongs to a small background `oci-hermit` process of its own, its monitor, which keeps
QEMU, writes the output to the container's log and records how it ended. A `run` without `-d` stays attached to it:
if that `run` goes away (the terminal closes, the process is killed), the monitor stops the container. With `-d` the
container runs on after `run` returns. There is no daemon: nothing runs when no container does.

Ctrl-C at a `run` without `-d` is passed on to the container's command as `SIGINT`, and `SIGTERM` sent to `run` as
`SIGTERM`, as `docker run` does (on Windows, Ctrl-C and Ctrl-Break). What happens then is up to the command. One
that is PID 1 in its container, as every container's command is, ignores signals it has no handler for: `sleep`
does, so Ctrl-C does not end `run … sleep 60`. Use `oci-hermit stop` or `oci-hermit kill` from another terminal. With
`-t`, Ctrl-C goes to the container's terminal instead, as a keystroke.

```console
$ oci-hermit run public.ecr.aws/docker/library/alpine:3.20 cat /etc/alpine-release
3.20.10
$ oci-hermit run -u nobody public.ecr.aws/docker/library/alpine:3.20 id
uid=65534(nobody) gid=65534(nobody)
$ oci-hermit run -d --name ticker public.ecr.aws/docker/library/alpine:3.20 sh -c 'while true; do date; sleep 5; done'
b472a05e48e7c828a74996b9a0c363491ec4e605b00c42ff03bf933847940407
```

```bash
oci-hermit run -it public.ecr.aws/docker/library/debian:bookworm-slim bash
oci-hermit run --platform linux/amd64 --cpus 2 -m 2g ./app.tar
```

**Exit codes:** the command's own exit code (128 plus the signal number if a signal ended it), or **125** when
oci-hermit itself fails: the image cannot be found or read, a flag is refused, the sandbox does not boot, or the
command cannot be started. With `-d`, 0 once the command started.

!!! warning "Windows: capture `run -d`'s ID in a file, not with `$(...)`"
    On Windows, `id=$(oci-hermit run -d …)` does not return while the container runs: the container's monitor
    inherits the shell's output pipe (#105). Write the ID to a file instead (`oci-hermit run -d … > ticker.id`), or
    find it later with `oci-hermit ps`.

Set `OCI_HERMIT_TIMINGS=1` to print how long each phase took, on stderr:

```console
$ OCI_HERMIT_TIMINGS=1 oci-hermit run public.ecr.aws/docker/library/alpine:3.20 true
timings: prepare 0.04s, boot 0.33s, command 0.01s, stop 0.02s, total 0.41s
```

## exec

Run another command in a running container, beside its own: in the same filesystem (what the container wrote is
there), processes, hostname and limits. Exits with the command's exit code.

```
oci-hermit exec [OPTIONS] <CONTAINER> <COMMAND>...
```

| Option | Meaning |
| --- | --- |
| `-i`, `--interactive` | Pass stdin to the command. Without it, the command's stdin is closed. |
| `-t`, `--tty` | Give the command a terminal (see `run -t`). |
| `-u`, `--user <USER>` | Run as this user instead of the container's: `name`, `uid`, `name:group` or `uid:gid`. Names are looked up in the image's `/etc/passwd` and `/etc/group`, as `run -u` does; a user added inside the running container is not seen yet, so use its number. |
| `-e`, `--env <NAME=value>` | Set an environment variable over the container's; `-e NAME` passes this shell's own value. Repeatable. |
| `-w`, `--workdir <DIR>` | Working directory; the container's by default. |

```console
$ oci-hermit run -d --name web public.ecr.aws/docker/library/alpine:3.20 sleep 1d
$ oci-hermit exec web cat /etc/alpine-release
3.20.10
$ oci-hermit exec -it web sh
/ #
```

The container must be running: `exec` goes through its monitor. The command's output goes to `exec` only, not to
the container's [log](#logs). If `exec` goes away (the terminal closes), the command keeps running, as with Docker;
when the container stops, every command started with `exec` stops with it, and a waiting `exec` exits 137. `exec -d`
is not supported yet.

**Exit codes:** the command's own, or **125** when it could not be started: the container is not running or not
found, the user is not in the image, or the command does not exist.

## ps

List containers: the running ones, or every one with `-a`. Newest first.

```
oci-hermit ps [-a] [-q] [--no-trunc]
```

| Option | Meaning |
| --- | --- |
| `-a`, `--all` | Include containers that are not running. |
| `-q`, `--quiet` | Only the IDs. |
| `--no-trunc` | Whole IDs and commands. |

```console
$ oci-hermit ps -a
CONTAINER ID   IMAGE         COMMAND                  CREATED          STATUS                      PORTS   NAMES
1f0c2a9e4b7d   alpine:3.20   "sh -c 'sleep 60'"       10 seconds ago   Up 9 seconds                        job
8c3d5e0a1b2f   alpine:3.20   "cat /etc/alpine-rel…"   2 hours ago      Exited (0) 2 hours ago              quiet_nautilus
```

A container is running while its monitor is (see [run](#run)). If the monitor was killed, the container shows as
`Exited (unknown)`; one that was killed shows `Exited (137)`, and one that ended on `stop`'s `SIGTERM` usually
`Exited (143)` or the code its handler chose.

## logs

Show what a container's command printed, its stdout on stdout and its stderr on stderr, as it printed it.

```
oci-hermit logs [-f] <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `-f`, `--follow` | Keep showing new output until the container stops. |

Every container keeps its output, whether it ran with `-d` or not, until it is removed. The log grows for as long
as the container runs; there is no size limit yet.

```console
$ oci-hermit logs -f ticker
Thu Nov 19 14:02:10 UTC 2026
Thu Nov 19 14:02:15 UTC 2026
```

## stop

Stop running containers. Prints each one as it was given, once it has stopped; exits 1 if any could not be stopped.
A container that is not running is left as it is, and printed too.

```
oci-hermit stop [-t <SECONDS>] [-s <SIGNAL>] <CONTAINERS>...
```

| Option | Meaning |
| --- | --- |
| `-t`, `--time <SECONDS>` | How long to wait for the command to end before killing it. Default 10; `0` kills it at once. |
| `-s`, `--signal <SIGNAL>` | The signal to send first, instead of the image's `StopSignal` (`SIGTERM` when it has none). |

As `docker stop` does: the container's command gets its stop signal, and if it is still running when the time is
up, the sandbox is powered off and the container is recorded with exit code 137. Several containers stop at once.
A command that handles `SIGTERM` (a web server, a shell script with `trap … TERM`) ends with its own exit code;
one that ignores it, as PID 1 does without a handler, takes the full time.

```console
$ oci-hermit stop -t 2 ticker
ticker
```

## kill

Send a signal to running containers' commands. Prints each one as it was given; exits 1 if any is not running
(`cannot kill container: <name>: container <id> is not running`, as Docker says) or could not be signalled.

```
oci-hermit kill [-s <SIGNAL>] <CONTAINERS>...
```

| Option | Meaning |
| --- | --- |
| `-s`, `--signal <SIGNAL>` | The signal: a name such as `TERM`, `SIGHUP` or `RTMIN+1`, or a number. Default `KILL`. |

The command decides what a signal does, except `KILL`, which always ends it (exit code 137). Signals go to the
container's own command, not to commands started with [`exec`](#exec).

```console
$ oci-hermit kill -s HUP web
web
```

## attach

Connect this terminal to a running container's command, as `docker attach` does: its output from now on (earlier
output is in [`logs`](#logs)), and this terminal's input if the container was started with `-i`. `attach` ends when
the container does, with its exit code, or when you type the detach keys, which leave it running (exit 0). Several
`attach`es can watch one container at once.

```
oci-hermit attach [OPTIONS] <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `--detach-keys <KEYS>` | The keys that detach, comma-separated: `ctrl-` and a letter or one of `@ [ \ ] ^ _`, or a plain character. Default `ctrl-p,ctrl-q`. They are not passed on; typed otherwise, they are. |
| `--no-stdin` | Do not pass this terminal's input on. |
| `--sig-proxy <BOOL>` | Pass Ctrl-C and `SIGTERM` on to the command (default `true`). For a container with a terminal (`-t`), Ctrl-C is a key the container gets instead, as with Docker. With `--sig-proxy=false`, Ctrl-C ends `attach` and leaves the container running. |

For a container with a terminal, `attach` puts this one in raw mode, as `run -it` does, so the shell gets every key.
Ending `attach`'s own input (a pipe running out) does not close the container's.

```console
$ oci-hermit run -dit --name box debian:bookworm-slim bash
$ oci-hermit attach box
root@4f2a9c1d0b3e:/# echo hello
hello
root@4f2a9c1d0b3e:/#          # Ctrl-P Ctrl-Q
$ oci-hermit ps
CONTAINER ID   IMAGE                  COMMAND   CREATED          STATUS          NAMES
4f2a9c1d0b3e   debian:bookworm-slim   bash      30 seconds ago   Up 30 seconds   box
```

A stopped container is refused: `you cannot attach to a stopped container, start it first`.

## cp

Copy files and folders between a running container and this machine, as `docker cp` does. One side is
`CONTAINER:PATH` (a name or ID, and an absolute path in the container); the other is a path here. Prints
`Successfully copied <size> to <destination>` on stderr; exits 1 if the copy could not be made.

```
oci-hermit cp CONTAINER:PATH HOSTPATH
oci-hermit cp HOSTPATH CONTAINER:PATH
```

Where it lands follows Docker: a file copied onto a folder goes inside it under its own name, and onto anything
else becomes that path (its folder must exist); a folder copied onto a folder goes inside it, and onto a missing
path becomes it; `SRC/.` copies only what the folder holds. A path here is read as `CONTAINER:PATH` only if it is
not absolute and does not start with `.`, so `./a:b` is a file here.

The rules are those for bind mounts:

- This machine's drive roots, the home folder (and any folder holding it), and credentials folders (`~/.ssh`,
  `~/.aws`, `~/.kube`, `~/.docker`, `~/.gnupg`, the keychains and Windows' credential stores) are refused, as source
  or destination, links followed.
- Nothing leaves the copy: paths in the container resolve inside it (its absolute links point into the container,
  never out), the container cannot name a file outside the destination, and what is there is never written
  through a link (a link in the way is replaced, as Docker overwrites). A link inside what is copied is copied as a
  link (not followed, as `docker cp` without `-L`) only if it points inside the copy; one that points outside is
  left out, with a warning. On Windows links are left out.
- Devices, FIFOs and sockets are left out, with a warning. Setuid, setgid and sticky bits are dropped. Files copied
  into the container belong to root, as with Docker; copied out, they belong to you.

Only a running container can be copied to or from: its files live in its sandbox, so they are gone once it stops
(Docker keeps a stopped container's files; oci-hermit does not). The container's own mounts (`/proc`, `/sys`,
`/dev`, `/etc/hosts`) are not part of what is copied. `-` (a tar stream on stdin or stdout) is not supported yet.
A large copy out holds up the container's output until it is done.

```console
$ oci-hermit cp ./site web:/srv
Successfully copied 1.2 MB to web:/srv
$ oci-hermit cp web:/var/log/nginx/access.log .
Successfully copied 4.1 kB to .
```

## wait

Block until containers stop, then print each one's exit code, one a line, in the order given, as `docker wait`
does. A container that has already stopped prints the code it stopped with at once (`-1` if it ended without one).
It waits for them all at the same time, so a `--rm` container's code is printed although it is removed when it
stops. Exits 0, whatever the codes; 1 if any container is not found.

```
oci-hermit wait <CONTAINERS>...
```

The code is the one `run` would have exited with: the command's own, 128 plus the signal that ended it (137 after
`kill` or a `stop` that ran out of time), or 125 if the container could not run.

```console
$ oci-hermit run -d --name job debian:bookworm-slim sh -c 'sleep 5; exit 3'
$ oci-hermit wait job
3
```

## rm

Remove containers that are not running, with their logs. Prints each one as it was given; exits 1 if any could not
be removed.

```
oci-hermit rm [-f] <CONTAINERS>...
```

A container is named by its name, its ID, or the start of its ID. A running container is refused unless `-f`
(`--force`) is given, which [stops](#stop) it first.

```console
$ oci-hermit rm quiet_nautilus 1f0c
quiet_nautilus
oci-hermit: container job is running; stop it first (oci-hermit stop job), or remove it with rm -f
$ oci-hermit rm -f job
job
```

## pull

Pull an image from a registry into the store, for one platform, checking every digest.

```
oci-hermit pull [OPTIONS] <IMAGE>
```

| Argument or option | Meaning |
| --- | --- |
| `IMAGE` | An image reference, e.g. `debian:bookworm-slim` or `ghcr.io/org/app:1.2`. A digest (`name@sha256:…`) works too. |
| `--platform <PLATFORM>` | The platform to pull. Default: the host's. |
| `--cert-dir <DIR>` | Use the CA certificates (`*.crt`) and client certificate (`*.cert` with `*.key`) in `DIR` for this registry, instead of its `certs.d` folder. |

```console
$ oci-hermit pull public.ecr.aws/docker/library/alpine:3.20
ab3fe4defd29: pulled (628 B)
3f26bc2dec0b: pulled (4.1 MB)
Digest: sha256:45e09956dc667c5eff3583c9d94830261fb1ca0be10a0a7db36266edf5de9e1d
Status: pulled public.ecr.aws/docker/library/alpine:3.20 for linux/arm64
```

Logins come from [`login`](#login) and from the Podman and Docker auth files and credential helpers (see
[Pull from a private registry](field-manual/private-registry.md)); with none, the pull is anonymous. Throttling and
server errors are retried four times. A registry's own CA and client certificate come from its `certs.d` folder
(see [a registry with its own CA](field-manual/private-registry.md#a-registry-behind-a-company-certificate-authority)).
Exits 0 on success, 1 on failure.

## manifest inspect

Show an image's index or manifest as the registry serves it, without pulling anything: which platforms it has, and
each one's digest and size. Handy before a pull over a slow link, or to check that a private registry has the
`linux/amd64` or `linux/arm64` build you need.

```
oci-hermit manifest inspect [OPTIONS] <IMAGE>
```

| Argument or option | Meaning |
| --- | --- |
| `IMAGE` | An image reference, as for `pull`; `name@sha256:…` works too. |
| `-v`, `--verbose` | Docker's verbose form: for each entry of an index, its descriptor (`Descriptor`) and the manifest it points to (`OCIManifest`, or `SchemaV2Manifest` for Docker's format), named by digest (`Ref`). For a single-platform image, the same for that manifest alone. |
| `--cert-dir <DIR>` | Use the CA certificates and client certificate in `DIR` for this registry, as for `pull`. |

```console
$ oci-hermit manifest inspect ghcr.io/prefix-dev/pixi:0.84.2
{
  "manifests": [
    {
      "digest": "sha256:96b7c4d3efbd1d7b7ec8c6bb601545e8cf8ff62d6aedf4957cabc716e504cc50",
      "mediaType": "application/vnd.oci.image.manifest.v1+json",
      "platform": {
        "architecture": "amd64",
        "os": "linux"
      },
      "size": 1056
    },
    …
```

The JSON is the registry's, indented, with its keys in alphabetical order. Logins, certificates, plain-HTTP registries
and retries work as for [`pull`](#pull), and every manifest is checked against its digest. Nothing is stored. Exits
0 on success, 1 on failure.

## login

Log in to a registry. The user name and password are checked with the registry before anything is saved, as
`docker login` does, then stored where Podman keeps logins, so `pull`, Podman and skopeo all use them.

```
oci-hermit login [OPTIONS] [REGISTRY]
```

| Argument or option | Meaning |
| --- | --- |
| `REGISTRY` | The registry's host name and port, e.g. `ghcr.io` or `registry.example.com:5000`. Default: `docker.io`. |
| `-u`, `--username <USERNAME>` | The user name. Asked for if not given. |
| `-p`, `--password <PASSWORD>` | The password or access token. Other users can see it in the process list, so it prints a warning; prefer `--password-stdin`. Asked for, without echo, if neither is given. |
| `--password-stdin` | Read the password from stdin. Needs `--username`. |
| `--authfile <AUTHFILE>` | Store the login in this file. Default: `$REGISTRY_AUTH_FILE`, else `$XDG_RUNTIME_DIR/containers/auth.json`, else `~/.config/containers/auth.json`. |
| `--get-login` | Print the user name logged in to `REGISTRY`, and nothing else. |
| `--cert-dir <DIR>` | Use the CA certificates and client certificate in `DIR` for this registry, as for `pull`. |

```console
$ echo "$GHCR_TOKEN" | oci-hermit login -u your-user --password-stdin ghcr.io
Login Succeeded!
```

The file is written readable by you only (mode 0600 on Linux and macOS; on Windows it is in your user profile). A
registry whose logins a credential helper keeps (`credHelpers` in that file) is refused. Exits 0 on success, 1 if the
registry rejects the login, cannot be reached, or the options do not fit together; nothing is written then.

## logout

Remove a registry login from the file `login` writes.

```
oci-hermit logout [OPTIONS] [REGISTRY]
```

| Argument or option | Meaning |
| --- | --- |
| `REGISTRY` | The registry host. Default: `docker.io`. |
| `-a`, `--all` | Remove every login in the file. |
| `--authfile <AUTHFILE>` | Change this file instead. |

```console
$ oci-hermit logout ghcr.io
Removed login credentials for ghcr.io
```

Exits 1 if there was no login to remove. A login made with `docker login` is in Docker's own file; `logout` names
that file and suggests `docker logout`.

## load

Load images from an archive or a layout into the store, offline.

```
oci-hermit load --input <INPUT>
```

| Option | Meaning |
| --- | --- |
| `-i`, `--input <INPUT>` | A `docker save` or `podman save` archive (old or Docker 25+ format), an OCI archive (`.tar`), or an OCI layout directory. |

Every platform in the archive is loaded, and every blob is checked against its digest. Each image keeps the name it
was saved under; one without a name is stored under its image ID.

```console
$ oci-hermit load -i alpine.tar
Loaded image: docker.io/mine/alpine:test (linux/arm64)
Loaded image: docker.io/mine/alpine:test (linux/amd64)
$ oci-hermit load -i ./unnamed
Loaded image ID: sha256:ab3fe4defd29ba6231229a4d41440ac8bde8218e85870e53876277faa24b35c4 (linux/arm64)
```

Exits 0 on success, 1 on failure. See [Bring images in without a registry](field-manual/without-a-registry.md).

## images

List stored images, newest first, one row per name and platform. `image ls` and `image list` are the same command.

```
oci-hermit images [OPTIONS] [REFERENCE]
```

| Argument or option | Meaning |
| --- | --- |
| `REFERENCE` | Only images from this repository, or with this name. |
| `-q`, `--quiet` | Only show image IDs. |
| `--digests` | Show each image's manifest digest. |
| `--no-trunc` | Show full image IDs. |

```console
$ oci-hermit images
REPOSITORY                             TAG             IMAGE ID       PLATFORM      CREATED        SIZE
public.ecr.aws/docker/library/debian   bookworm-slim   5b6c2c6cbc90   linux/arm64   12 days ago    28.1 MB
public.ecr.aws/docker/library/alpine   3.20            bf8527eb54c3   linux/amd64   7 months ago   3.6 MB
public.ecr.aws/docker/library/alpine   3.20            ab3fe4defd29   linux/arm64   7 months ago   4.1 MB
```

An image without a name shows `<none>` as its repository and tag.

## inspect

Show details of containers and images as a JSON array, one object per name. As with `docker inspect`, a name or ID is
looked up among containers first, then images; `image inspect` looks only at images.

```
oci-hermit inspect <NAMES>...
oci-hermit image inspect <IMAGES>...
```

An image's fields are close to `podman image inspect`: `Id`, `Digest`, `RepoTags`, `RepoDigests`, `Created`,
`Author`, `Architecture`, `Variant`, `Os`, `Size`, `Labels`, `Config`, `RootFS`, `History` and `ManifestType`. An
example is in [Manage the image store](field-manual/image-store.md#look-inside-an-image).

A container's are close to `docker inspect`'s: `Id`, `Created`, `Path`, `Args`, `State` (`Status`, `Running`,
`ExitCode`, `Error`, `StartedAt`, `FinishedAt`), `Image` (the image ID), `Name`, `Platform`, `Config` (`Hostname`,
`User`, `Tty`, `OpenStdin`, `Cmd`, `Image`) and `HostConfig` (`NanoCpus` and `Memory`, its `--cpus` and `-m` limits,
0 when there is none; `AutoRemove`). `Sandbox` is oci-hermit's own: the VM it runs in (`Platform`, `Cpus`, and
`Memory` in bytes).

```console
$ oci-hermit run -d --name web --cpus 1.5 -m 1g debian:bookworm-slim sleep 600
$ oci-hermit inspect web
[
  {
    "Id": "4f2a9c1d0b3e…",
    …
    "HostConfig": {
      "AutoRemove": false,
      "Memory": 1073741824,
      "NanoCpus": 1500000000
    },
    "Sandbox": {
      "Cpus": 2,
      "Memory": 1207959552,
      "Platform": "linux/amd64"
    }
  }
]
```

A name that is neither prints `oci-hermit: no such image or container: <name>` on stderr (`no such image` for
`image inspect`), and `inspect` exits 1.

## tag

Give a stored image another name, for every platform stored under the source. Nothing is copied. `image tag` is the
same command.

```
oci-hermit tag <SOURCE> <TARGET>
```

```bash
oci-hermit tag public.ecr.aws/docker/library/alpine:3.20 mine/alpine:test
```

## save

Save images to an archive that `oci-hermit load`, `docker load`, `podman load` and skopeo read. `image save` is the
same command.

```
oci-hermit save [OPTIONS] --output <OUTPUT> <IMAGES>...
```

| Argument or option | Meaning |
| --- | --- |
| `IMAGES...` | Names or IDs. |
| `-o`, `--output <OUTPUT>` | The file to write. Required. |
| `--platform <PLATFORM>` | Only this platform, written in full (`linux/arm64`). Default: every platform stored. |

The archive is an OCI image layout with a `manifest.json` beside it, the format Docker 25 and later write. See
[Move images between machines](field-manual/move-images.md).

## rmi

Remove images by name or ID. Blobs no other image uses are deleted. `image rm` and `image remove` are the same
command.

```
oci-hermit rmi [OPTIONS] <IMAGES>...
```

| Option | Meaning |
| --- | --- |
| `-f`, `--force` | Given an image ID with several names, remove every name. Without it, such an ID is refused. |

```console
$ oci-hermit rmi public.ecr.aws/docker/library/alpine:3.20
Untagged: public.ecr.aws/docker/library/alpine:3.20
Deleted: sha256:bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731
```

Exits 1 if any name matched nothing or was refused. Refuses to run while another oci-hermit is using the store.

## image prune

Remove images loaded without a name, and every blob nothing uses.

```
oci-hermit image prune [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `-a`, `--all` | Remove every image, not only unnamed ones. |
| `-f`, `--force` | Do not ask for confirmation. |

Without `-f`, it asks `Are you sure you want to continue? [y/N]` and goes ahead only on `y` or `yes`. Refuses to run
while another oci-hermit is using the store.

## image history

Show the steps that built an image, newest first, with the size of the layer each made.

```
oci-hermit image history [OPTIONS] <IMAGE>
```

| Option | Meaning |
| --- | --- |
| `--no-trunc` | Do not shorten IDs and commands. |
| `-q`, `--quiet` | Only show image IDs. |

## image exists

Exit 0 if an image is stored, 1 if not. Prints nothing.

```
oci-hermit image exists <IMAGE>
```

## system df

Show how much space the image store and the containers use, as `docker system df` does.

```console
$ oci-hermit system df
TYPE         TOTAL   ACTIVE   SIZE      RECLAIMABLE
Images       3       1        35.9 MB   7.4 MB (20%)
Containers   4       1        52.1 kB   48.6 kB (93%)
```

An image is active when a container (running or not) was made from it; what only the other images need is
reclaimable, so a layer an active image shares is not. A container is active while it runs; the stopped ones'
records and logs are reclaimable, which [`container prune`](#container-prune) frees. A container's writable layer
lives in the sandbox's memory, so it takes no disk space once the container stops.

## container prune

Remove every container that is not running, with its record and logs, as `docker container prune` does. Running
containers are never touched; one that starts while `prune` runs is left alone.

```
oci-hermit container prune [-f]
```

| Option | Meaning |
| --- | --- |
| `-f`, `--force` | Do not ask for confirmation. |

Without `-f`, it asks `Are you sure you want to continue? [y/N]` and goes ahead only on `y` or `yes`.

```console
$ oci-hermit container prune
WARNING! This will remove all stopped containers.
Are you sure you want to continue? [y/N] y
Deleted Containers:
4f2a9c1d0b3e5a7c9e1f3b5d7a9c1e3f5b7d9a1c3e5f7b9d1a3c5e7f9b1d3a5c
1b3d5f7a9c1e3b5d7f9a1c3e5b7d9f1a3c5e7b9d1f3a5c7e9b1d3f5a7c9e1b3d

Total reclaimed space: 48.6 kB
```

## machine info

Show the emulators, the guests, the defaults `run` uses, the image store and the registry auth files, and where each
came from. `oci-hermit info` is the same command.

```
oci-hermit machine info
```

A full example is in [Getting started](getting-started.md#2-check-it), and every line is explained in
[Configuration](configuration.md#what-machine-info-shows). Exits 1 when the QEMU bundle is missing or is not the one
oci-hermit was built for, because nothing can run then; a missing guest is shown but is not an error.

## machine boot

Boot a sandbox until its agent is ready, then stop it: a self-check of QEMU, the kernel and the agent. It needs no
image.

```
oci-hermit machine boot [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--arch <ARCH>` | Guest architecture: `x86_64` or `aarch64`. Default: the host's. |
| `--memory <MEMORY>` | Memory for the sandbox, in MiB. Default 512. |
| `--cpus <CPUS>` | Number of emulated CPUs. Default 1. |
| `--timeout <TIMEOUT>` | Seconds to wait for the agent to be ready. Default 120. |

```console
$ oci-hermit machine boot --arch x86_64
booting an x86_64 sandbox; console log: /var/folders/kb/…/T/oci-hermit/sandbox-12666-1794883275416245000/console.log
agent ready after 0.6s (control protocol 1.4)
guest powered off; sandbox stopped
```

Exits 0 when the guest booted and powered off, 1 otherwise; the error names the console and QEMU logs.

## version

Print the version and the control protocol version the host speaks.

```console
$ oci-hermit version
oci-hermit 0.2.0 (control protocol 1.4)
```

## Exit codes

| Code | When |
| --- | --- |
| 0 | Success. |
| 1 | The command failed: a pull, load, image or container command (`logs`, `stop`, `kill`, `rm`) failed, `image exists` found nothing, `machine info` found no usable QEMU, `machine boot` did not boot. |
| 2 | The command is not implemented yet, or the command line is wrong (clap's usage errors). |
| 125 | `run`, `exec`: oci-hermit itself failed before or around the command. |
| other | `run`, `exec`: the command's own exit code. |

## Planned commands

These are part of the command line already, so `--help` lists them, but they do nothing yet. Each prints
`oci-hermit: `<command>` is not implemented yet; see …/ROADMAP.md` and exits 2:

```console
$ oci-hermit pod ps
oci-hermit: `pod` is not implemented yet; see https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md
```

| Command | Will | Issue |
| --- | --- | --- |
| `pod create --name <NAME> [-p <PUBLISH>]`, `pod rm <POD>`, `pod ps` | pods: containers that share localhost | #35 |
| `compose up [-d] [-f <FILE>]`, `compose down [-f <FILE>]` | a Compose subset | #36, #37 |

`run -p`, `-v` and `--pod` are refused, with their issue, rather than ignored (see [run](#run)).

Not in the command line yet, and planned:

| Command | Issue |
| --- | --- |
| `create`, `start`, `restart` | #77 |
| `top`, `stats` | #80 |
| `diff`, `export` | #81 |
| `healthcheck run` | #83 |
| `port` | #84 |
