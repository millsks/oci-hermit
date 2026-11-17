# Bring images in without a registry

**When:** you have an image as a file rather than in a registry you can reach: a `docker save` or `podman save`
archive someone sent you, an OCI archive from a build, or an OCI layout directory. Nothing here needs the network.

## What oci-hermit reads

| Format | Made by |
| --- | --- |
| Docker archive, old format (`manifest.json`, config and layer files) | `docker save` before Docker 25, `podman save` (the default `docker-archive` format) |
| Docker archive, Docker 25 and later (an OCI layout with a `manifest.json` beside it) | `docker save` from Docker 25, `oci-hermit save`, `pixi oci build` |
| OCI archive (a `.tar` of an OCI image layout) | `podman save --format oci-archive`, `skopeo copy … oci-archive:…`, `buildah push … oci-archive:…` |
| OCI layout directory | `skopeo copy … oci:<dir>`, `podman save --format oci-dir`, an unpacked OCI archive |

The archive must be a plain `.tar`. Unpack a compressed one first (`gunzip app.tar.gz`, or `tar -xzf` into a
directory). Layers inside may be plain or gzip; zstd layers are not supported yet.

## 1. Make the archive

On a machine with Docker or Podman:

```bash
docker save -o app.tar registry.example.com/team/app:1.2
# or
podman save -o app.tar registry.example.com/team/app:1.2
# or, an OCI archive
podman save --format oci-archive -o app.tar registry.example.com/team/app:1.2
```

`docker save` writes one platform: the one Docker has pulled. Pull the platform you need first (for example
`docker pull --platform linux/amd64 …`). See [Choose a platform](platforms.md).

Copy the file to the machine where oci-hermit runs, any way you like.

## 2. Load it into the store

```console
$ oci-hermit load -i debian.tar
Loaded image: quay.io/lib/debian:bookworm-slim (linux/arm64)
$ oci-hermit images
REPOSITORY           TAG             IMAGE ID       PLATFORM      CREATED         SIZE
quay.io/lib/debian   bookworm-slim   0e1693e99bef   linux/arm64   21 months ago   29.9 MB
```

`load` reads every platform in the archive and checks every blob against its digest. The image keeps the name it was
saved under, and `run` finds it by that name, without going to the registry:

```console
$ oci-hermit run quay.io/lib/debian:bookworm-slim cat /etc/debian_version
12.9
```

Loading the same image again, from either format, replaces its name. The blobs the old copy used stay in the store
until `oci-hermit image prune` removes them; see [Manage the image store](image-store.md#free-space).

## 3. An image without a name

An archive or layout with no name in it is stored under its image ID:

```console
$ oci-hermit load -i ./unnamed
Loaded image ID: sha256:ab3fe4defd29ba6231229a4d41440ac8bde8218e85870e53876277faa24b35c4 (linux/arm64)
$ oci-hermit images
REPOSITORY   TAG      IMAGE ID       PLATFORM      CREATED        SIZE
<none>       <none>   ab3fe4defd29   linux/arm64   7 months ago   4.1 MB
```

Use the ID, or enough of its start to be unique, wherever a name goes, and give it a name with `tag`:

```bash
oci-hermit run ab3f cat /etc/alpine-release
oci-hermit tag ab3f myteam/alpine:3.20
```

`oci-hermit image prune` removes images that have no name.

## Or run it without storing it

`run` also takes the path of an archive or a layout directory, and reads the image from there. Nothing is copied
into the store:

```console
$ oci-hermit run alpine.tar cat /etc/alpine-release
3.20.10
$ oci-hermit run ./layout uname -m
aarch64
```

This suits a one-off check, or a folder you cannot write to. A path wins over a stored name, so write `./name` to be
sure a relative path is read as one.

## When it fails

| Message | What it means |
| --- | --- |
| `… does not match its digest: the image is corrupt or has been altered` | The file was damaged on the way. Copy it again. |
| `the image has no linux/<arch> variant …`, a warning from `run` | The archive holds only the other architecture; `run` uses it anyway. See [Choose a platform](platforms.md). |
| an error about a zstd layer | The image was pushed with zstd compression. Re-save it with gzip layers, for example with `skopeo copy --dest-compress-format gzip`. |
