# Choose a platform

**When:** the image you need was built for another architecture than your machine, or it was built for both and you
want to test a particular one.

oci-hermit emulates the guest's CPU, so both architectures run on every host: an `linux/arm64` image runs on an
Intel or AMD PC, and a `linux/amd64` image runs on a Mac with Apple silicon or an arm64 Linux machine. Only Linux
images run; Windows container images do not.

| Platform | Also called | Guest |
| --- | --- | --- |
| `linux/amd64` | `x86_64` | `guest/x86_64`, QEMU's `microvm` machine |
| `linux/arm64` | `aarch64` | `guest/aarch64`, QEMU's `virt` machine |

## The default: the host's platform

Without `--platform`, `pull` and `run` use your machine's own platform. `machine info` names it on its second line:

```console
host: macos/aarch64, so images default to linux/arm64
```

## Ask for one with --platform

`--platform` takes `linux/amd64` or `linux/arm64`; `run` and `pull` also accept `amd64`, `arm64`, `x86_64` and
`aarch64`:

```console
$ oci-hermit run --platform linux/amd64 public.ecr.aws/docker/library/alpine:3.20 uname -m
Unable to find image 'public.ecr.aws/docker/library/alpine:3.20' locally
bf8527eb54c3: pulled (612 B)
25f1d6b1951a: pulled (3.6 MB)
Digest: sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e
x86_64
```

The store already held the arm64 variant, so `run` pulled the amd64 one. Both are now stored, under the same name:

```console
$ oci-hermit images public.ecr.aws/docker/library/alpine
REPOSITORY                             TAG    IMAGE ID       PLATFORM      CREATED        SIZE
public.ecr.aws/docker/library/alpine   3.20   bf8527eb54c3   linux/amd64   7 months ago   3.6 MB
public.ecr.aws/docker/library/alpine   3.20   ab3fe4defd29   linux/arm64   7 months ago   4.1 MB
```

The same flag works on `pull` (`oci-hermit pull --platform linux/amd64 …`) and on `save`, which then writes only
that platform (spelled in full there, `linux/amd64`). `machine boot --arch x86_64` or `--arch aarch64` boots either guest without an image.

Any other platform is refused:

```console
$ oci-hermit run --platform linux/riscv64 alpine.tar true
oci-hermit: unsupported --platform linux/riscv64
```

## An image built for one platform only

When an image has no variant for your platform and you did not ask for one, `run` uses the other one, as Docker
does, and says so on stderr:

```console
$ oci-hermit run mine/alpine:test uname -m
WARNING: the image has no linux/arm64 variant; running its linux/amd64 variant (choose with --platform)
x86_64
```

That applies to stored images, to archives and layouts given as a path, and to pulls. With `--platform`, there is no
fallback: oci-hermit runs exactly that platform, or pulls it, or fails.

## What it costs

Every guest is emulated, so neither platform runs at native speed. The difference between them depends on the
host: on the hosts measured, an x86_64 guest boots more slowly than an aarch64 one, except on Intel Macs. See
[Performance and limits](../performance.md).

## Test the platform you ship

If production runs on amd64 servers, test with `--platform linux/amd64`, even on an arm64 laptop: an image's variants
can differ (a missing package, a binary built for one architecture only), and the variant your host would pick is
not the one you deploy.
