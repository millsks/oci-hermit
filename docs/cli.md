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
| [`pull`](#pull) | Pull an image from a registry |
| [`manifest inspect`](#manifest-inspect) | Show an image's platforms and digests in a registry, without pulling |
| [`load`](#load) | Load images from a `docker save` archive or an OCI layout |
| [`login`](#login) | Log in to a registry |
| [`logout`](#logout) | Remove a registry login |
| [`images`, `image ls`](#images) | List stored images |
| [`inspect`, `image inspect`](#inspect) | Show details of images as JSON |
| [`tag`, `image tag`](#tag) | Give an image another name |
| [`save`, `image save`](#save) | Save images to an archive |
| [`rmi`, `image rm`](#rmi) | Remove images |
| [`image prune`](#image-prune) | Remove unnamed images, or all of them |
| [`image history`](#image-history) | Show the steps that built an image |
| [`image exists`](#image-exists) | Exit 0 if an image is stored, 1 if not |
| [`system df`](#system-df) | Show how much space images use |
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
| `--cpus <CPUS>` | Number of emulated CPUs. Default 1. |
| `-m`, `--memory <MEMORY>` | Memory for the sandbox: a number with `k`, `m` or `g` (`512m`, `2g`); bytes without a suffix, as Docker. At least `64m`. Default: 512 MiB for a stored image, which runs from its layer disks; for an image run from a file, twice its size plus 256 MiB. |
| `--name <NAME>` | The container's name: a letter or digit, then letters, digits, `_`, `.` or `-`. Without one, a name such as `quiet_nautilus` is made up. A name stays taken until the container is removed. |
| `--rm` | Remove the container when the command ends. Without it the container is kept, exited, for `ps -a`, until `rm`. |
| `-d`, `--detach` | Planned (#22). Refused today. |
| `-p`, `--publish <PUBLISH>` | Planned (#27): `[127.0.0.1:]HOST:CONTAINER`, always bound to 127.0.0.1. Refused today. |
| `-v`, `--volume <VOLUME>` | Planned (#33): `SRC:DST`, with `:ro` or `:rw`; read-only by default. Refused today. |
| `--pod <POD>` | Planned (#35). Refused today. |

The command runs in the image's working directory, with the image's environment, plus `HOME` from the user's
`/etc/passwd` entry if the image does not set it.

Every `run` is recorded as a container (see [ps](#ps)), with its exit code, or with 125 and the reason if
oci-hermit could not run it. Only the record is kept: the files the command changed are thrown away with the
sandbox.

```console
$ oci-hermit run public.ecr.aws/docker/library/alpine:3.20 cat /etc/alpine-release
3.20.10
$ oci-hermit run -u nobody public.ecr.aws/docker/library/alpine:3.20 id
uid=65534(nobody) gid=65534(nobody)
$ oci-hermit run -d public.ecr.aws/docker/library/alpine:3.20
oci-hermit: -d/--detach is not supported yet (#22); run it without -d/--detach
```

```bash
oci-hermit run -it public.ecr.aws/docker/library/debian:bookworm-slim bash
oci-hermit run --platform linux/amd64 --cpus 2 -m 2g ./app.tar
```

**Exit codes:** the command's own exit code (128 plus the signal number if a signal ended it), or **125** when
oci-hermit itself fails: the image cannot be found or read, a flag is refused, the sandbox does not boot, or the
command cannot be started.

Set `OCI_HERMIT_TIMINGS=1` to print how long each phase took, on stderr:

```console
$ OCI_HERMIT_TIMINGS=1 oci-hermit run public.ecr.aws/docker/library/alpine:3.20 true
timings: prepare 0.04s, boot 0.33s, command 0.01s, stop 0.02s, total 0.41s
```

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

A container is running while the `run` that started it is. If that `run` was killed, the container shows as
`Exited (unknown)`.

## rm

Remove containers that are not running. Prints each one as it was given; exits 1 if any could not be removed.

```
oci-hermit rm [-f] <CONTAINERS>...
```

A container is named by its name, its ID, or the start of its ID. `-f` is accepted; stopping a running container
first is planned (#22), so a running one is refused either way.

```console
$ oci-hermit rm quiet_nautilus 1f0c
quiet_nautilus
oci-hermit: container job is running; let its `run` finish first (stop and rm -f are planned, #22)
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

Show details of images as a JSON array, one object per name, close to `podman image inspect`. `image inspect` is the
same command.

```
oci-hermit inspect <IMAGES>...
```

The fields are `Id`, `Digest`, `RepoTags`, `RepoDigests`, `Created`, `Author`, `Architecture`, `Variant`, `Os`,
`Size`, `Labels`, `Config`, `RootFS`, `History` and `ManifestType`. An image that is not stored prints
`oci-hermit: no such image: <name>` on stderr, and `inspect` exits 1. An example is in
[Manage the image store](field-manual/image-store.md#look-inside-an-image).

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

Show how much space the store uses. There are no containers yet, so every image counts as reclaimable.

```console
$ oci-hermit system df
TYPE         TOTAL   ACTIVE   SIZE      RECLAIMABLE
Images       3       0        35.9 MB   35.9 MB (100%)
Containers   0       0        0 B       0 B
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
| 1 | The command failed: a pull, load or image command failed, `image exists` found nothing, `machine info` found no usable QEMU, `machine boot` did not boot. |
| 2 | The command is not implemented yet, or the command line is wrong (clap's usage errors). |
| 125 | `run`: oci-hermit itself failed before or around the command. |
| other | `run`: the command's own exit code. |

## Planned commands

These are part of the command line already, so `--help` lists them, but they do nothing yet. Each prints
`oci-hermit: `<command>` is not implemented yet; see …/ROADMAP.md` and exits 2:

```console
$ oci-hermit logs web
oci-hermit: `logs` is not implemented yet; see https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md
```

| Command | Will | Issue |
| --- | --- | --- |
| `exec [-i] [-t] <CONTAINER> <COMMAND>...` | run a command in a running container | #22 |
| `logs [-f] <CONTAINER>` | show a container's logs | #22 |
| `stop <CONTAINERS>...` | stop containers | #22 |
| `cp <SOURCE> <DESTINATION>` | copy files between a container and the host | #23 |
| `pod create --name <NAME> [-p <PUBLISH>]`, `pod rm <POD>`, `pod ps` | pods: containers that share localhost | #35 |
| `compose up [-d] [-f <FILE>]`, `compose down [-f <FILE>]` | a Compose subset | #36, #37 |

`run -d`, `-p`, `-v` and `--pod` are refused, with their issue, rather than ignored (see [run](#run)).

Not in the command line yet, and planned:

| Command | Issue |
| --- | --- |
| `create`, `start`, `restart` | #77 |
| `wait`, `kill` | #78 |
| `attach` | #79 |
| `top`, `stats` | #80 |
| `diff`, `export` | #81 |
| `container prune` | #82 |
| `healthcheck run` | #83 |
| `port` | #84 |
