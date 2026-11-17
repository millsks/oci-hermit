# Field manual

The [command-line reference](../cli.md) is organised by command. This part is organised by task: you have something
to do, and each page below is a complete walk-through. A page gives the commands in order, shows what success looks
like, says what to do when it fails, and links the reference for the details.

| You need to… | Walk-through |
|---|---|
| get a shell in a public image, or run one command in it | [Run a public image and get a shell](run-a-public-image.md) |
| pull from a registry that needs a login, or one behind a company certificate authority | [Pull from a private registry](private-registry.md) |
| run an image you have as a file: `docker save`, `podman save`, an OCI archive | [Bring images in without a registry](without-a-registry.md) |
| carry images to a machine with no registry access | [Move images between machines](move-images.md) |
| list, rename, inspect and delete stored images, and free space | [Manage the image store](image-store.md) |
| try oci-hermit on a company laptop, and find out what blocks it | [Run on a managed device](../no-admin.md) |
| run an amd64 image on an arm64 machine, or the other way round | [Choose a platform](platforms.md) |

The commands assume `oci-hermit` is on your `PATH` ([Getting started](../getting-started.md#put-it-on-your-path)).
They are the same on Windows, macOS and Linux; where the shell differs, both versions are shown. The outputs were
captured on a Mac with Apple silicon, so the host's platform is `linux/arm64`; on an x64 machine it is `linux/amd64`.

Every guide uses images from `public.ecr.aws/docker/library`, Amazon's mirror of the Docker official images, because
it has no anonymous rate limit. The same images are on Docker Hub under their short names (`alpine`,
`debian:bookworm-slim`).
