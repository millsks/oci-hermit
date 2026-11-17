# Manage the image store

**When:** you want to see what is stored, give an image another name, look inside one, or free space. These commands
only read and write the store, a folder you own; they need no sandbox and no network.

## See what is stored

```console
$ oci-hermit images
REPOSITORY                             TAG             IMAGE ID       PLATFORM      CREATED        SIZE
public.ecr.aws/docker/library/debian   bookworm-slim   5b6c2c6cbc90   linux/arm64   12 days ago    28.1 MB
public.ecr.aws/docker/library/alpine   3.20            bf8527eb54c3   linux/amd64   7 months ago   3.6 MB
public.ecr.aws/docker/library/alpine   3.20            ab3fe4defd29   linux/arm64   7 months ago   4.1 MB
```

There is one row per name and platform, newest first. `image ls` is the same command.

| Option | Shows |
| --- | --- |
| `REFERENCE` | only images from that repository, or with that name: `oci-hermit images public.ecr.aws/docker/library/alpine` |
| `-q`, `--quiet` | only the image IDs, one per line, for scripts |
| `--digests` | a `DIGEST` column with each manifest's digest |
| `--no-trunc` | full image IDs |

Names follow Docker: `alpine` means `docker.io/library/alpine:latest`, so `oci-hermit images alpine` lists the Docker
Hub image, not one from another registry.

## Names and IDs

Every command that takes an image takes either a name or an ID:

- **A name**, in full (`docker.io/library/alpine:latest`) or as Docker shortens it (`alpine`).
- **An image ID**, the digest of the image's config, or the first few characters of it, enough to be unique:
  `ab3f`. `sha256:` in front is optional.

An image pulled for two platforms has two IDs under one name. `rmi`, `tag` and `save` act on every platform under a
name.

## Look inside an image

`inspect` prints details as JSON, close to `podman image inspect`: the IDs and digests, names, creation time,
platform, size, labels, the image's config (entrypoint, command, environment, working directory and so on), its
layers and its history. Trimmed:

```console
$ oci-hermit inspect mine/alpine:test
[
  {
    "Id": "sha256:ab3fe4defd29ba6231229a4d41440ac8bde8218e85870e53876277faa24b35c4",
    "Digest": "sha256:45e09956dc667c5eff3583c9d94830261fb1ca0be10a0a7db36266edf5de9e1d",
    "RepoTags": [
      "docker.io/mine/alpine:test"
    ],
    "Created": "2026-04-16T23:53:24.896953537Z",
    "Architecture": "arm64",
    "Os": "linux",
    "Size": 4093973,
    "Config": {
      "Cmd": [
        "/bin/sh"
      ],
      "Env": [
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
      ],
      "WorkingDir": "/"
    },
    …
  }
]
```

A name that matches nothing prints `oci-hermit: no such image: …` on stderr, and `inspect` exits 1.

`image history` shows the steps that built the image, newest first, with the size each one added:

```console
$ oci-hermit image history public.ecr.aws/docker/library/alpine:3.20
IMAGE          CREATED        CREATED BY                                      SIZE     COMMENT
ab3fe4defd29   7 months ago   CMD ["/bin/sh"]                                 0 B      buildkit.dockerfile.v0
<missing>      7 months ago   ADD alpine-minirootfs-3.20.10-aarch64.tar.gz…   4.1 MB   buildkit.dockerfile.v0
```

`--no-trunc` shows full IDs and commands; `-q` only the IDs.

`image exists` says, through its exit code, whether an image is stored; it prints nothing:

=== "macOS and Linux"

    ```bash
    oci-hermit image exists mine/alpine:test && echo stored
    ```

=== "Windows"

    ```powershell
    oci-hermit image exists mine/alpine:test; if ($LASTEXITCODE -eq 0) { "stored" }
    ```

## Give an image another name

```bash
oci-hermit tag public.ecr.aws/docker/library/alpine:3.20 mine/alpine:test
```

The new name points at the same image, for every platform stored under the old one; nothing is copied. A name
without a registry is a Docker Hub name, so this one is `docker.io/mine/alpine:test`.

## Remove images

`rmi` (or `image rm`) removes names. When nothing uses an image's blobs any more, they are deleted too:

```console
$ oci-hermit rmi mine/alpine:test
Untagged: docker.io/mine/alpine:test
$ oci-hermit rmi public.ecr.aws/docker/library/alpine:3.20
Untagged: public.ecr.aws/docker/library/alpine:3.20
Deleted: sha256:bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731
```

The first `rmi` only removed a name, because the same image was still stored under another one.

Given an image ID, `rmi` removes that image. If the ID has several names, it asks you to choose, as Docker and Podman
do; `-f` removes them all:

```console
$ oci-hermit rmi ab3f
oci-hermit: image ab3fe4defd29 has 2 names (docker.io/mine/alpine:test, public.ecr.aws/docker/library/alpine:3.20); remove them by name, or use -f to remove them all
$ oci-hermit rmi -f ab3f
Untagged: docker.io/mine/alpine:test
Untagged: public.ecr.aws/docker/library/alpine:3.20
Deleted: sha256:ab3fe4defd29ba6231229a4d41440ac8bde8218e85870e53876277faa24b35c4
```

A name that matches nothing prints `oci-hermit: no such image: …`, and `rmi` exits 1.

## Free space

`system df` shows how much the store holds:

```console
$ oci-hermit system df
TYPE         TOTAL   ACTIVE   SIZE      RECLAIMABLE
Images       3       0        35.9 MB   35.9 MB (100%)
Containers   0       0        0 B       0 B
```

There are no containers yet, so every image counts as reclaimable.

`image prune` removes images loaded without a name, and any blob nothing uses (for example the layers of an image
that was loaded again from a different archive). It asks first; `-f` skips the question, and `-a` removes every
image, named or not:

```console
$ oci-hermit image prune -f
Total reclaimed space: 100.1 MB
$ oci-hermit image prune -a
WARNING! This will remove all images.
Are you sure you want to continue? [y/N] y
Deleted Images:
untagged: docker.io/mine/alpine:test
deleted: sha256:bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731
deleted: sha256:ab3fe4defd29ba6231229a4d41440ac8bde8218e85870e53876277faa24b35c4

Total reclaimed space: 7.7 MB
```

Anything but `y` or `yes` leaves the store as it is.

To start again from nothing, `oci-hermit image prune -a -f`.

## One at a time

`rmi` and `image prune` delete blobs, so they need the store to themselves. While a `pull`, `load` or `run` is
reading it, they refuse with:

```
oci-hermit: the image store is in use by another oci-hermit (a pull, load or run); try again when it has finished
```

`run` lets go of the store once it has read the image, so a long-running command does not block them.

## Where the store is

`oci-hermit machine info` shows the path, how many images it holds and their size. To keep it somewhere else (another
disk, or a scratch folder for a test), set `OCI_HERMIT_DATA_DIR`; see [Configuration](../configuration.md).
