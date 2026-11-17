# Move images between machines

**When:** one machine can reach the registry and another cannot, or you want to hand a colleague the exact image you
tested. `save` on one machine and `load` on the other carry images across with no registry in between.

## 1. On the machine with access: pull, then save

Pull every platform the other machine will need. An image pulled for both shows up once per platform:

```console
$ oci-hermit pull public.ecr.aws/docker/library/alpine:3.20
…
Status: pulled public.ecr.aws/docker/library/alpine:3.20 for linux/arm64
$ oci-hermit pull --platform linux/amd64 public.ecr.aws/docker/library/alpine:3.20
…
Status: pulled public.ecr.aws/docker/library/alpine:3.20 for linux/amd64
```

Optionally give it the name you want it to have on the other side, then save it. `save` writes every platform
stored under the name, unless `--platform` picks one:

```bash
oci-hermit tag public.ecr.aws/docker/library/alpine:3.20 mine/alpine:test
oci-hermit save -o alpine.tar mine/alpine:test
oci-hermit save --platform linux/amd64 -o alpine-amd64.tar mine/alpine:test
```

Several names can go in one archive: `oci-hermit save -o bundle.tar app:1.2 db:16`.

The archive is in the format Docker 25 and later write: an OCI image layout, with a `manifest.json` beside it for
older tools. `oci-hermit load`, `docker load`, `podman load` and skopeo (`oci-archive:alpine.tar`) all read it, and
every blob is checked against its digest as it is written.

## 2. Carry the file across

Any way that keeps the bytes intact: a shared folder, a USB stick, `scp`. To check it arrived whole, compare a hash
on both sides:

=== "macOS and Linux"

    ```bash
    shasum -a 256 alpine.tar
    ```

=== "Windows"

    ```powershell
    Get-FileHash -Algorithm SHA256 alpine.tar
    ```

`load` checks every blob anyway, so a damaged archive is refused rather than run.

## 3. On the other machine: load, then run

```console
$ oci-hermit load -i alpine.tar
Loaded image: docker.io/mine/alpine:test (linux/arm64)
Loaded image: docker.io/mine/alpine:test (linux/amd64)
$ oci-hermit run mine/alpine:test cat /etc/alpine-release
3.20.10
```

Both platforms arrived, and the image keeps its name, so `run` finds it without going to the registry. A name
without a registry is a Docker Hub name, as in Docker, so `mine/alpine:test` is `docker.io/mine/alpine:test`.

!!! tip "Carry every platform you will run"

    If a `run` asks for a platform the store does not have, oci-hermit tries to pull it. On a machine with no
    registry access that fails with a network error. Save every platform you need, or name the platform with
    `--platform` so the one you carried is used.

## Into Docker or Podman

The same archive loads into Docker and Podman, which makes it a way to hand an image back to a machine with a normal
container runtime:

```bash
docker load -i alpine.tar
podman load -i alpine.tar
```
