# oci-hermit

oci-hermit runs prebuilt OCI container images inside a small Linux virtual machine whose CPU is emulated in
software (QEMU's TCG). It works on Windows, macOS and Linux as a normal user: no root, no admin rights, no hypervisor,
no WSL and nothing to enable in the firmware. The commands follow Docker and Podman.

It is for people who need to test a container image on a machine where Docker and Podman are not an option, such as
a locked-down company laptop. It is a development and testing tool, not a production runtime: it is slower than
native, and the sandbox has no network at all.

!!! warning "Early days"

    oci-hermit has no release yet. You can pull, load, store and run images today; detached containers, published
    ports, bind mounts, pods and Compose are still to come. The [roadmap](roadmap.md) says when, and the
    [command-line reference](cli.md#planned-commands) lists every planned command with its issue.

## In 30 seconds

```console
$ oci-hermit run public.ecr.aws/docker/library/debian:bookworm-slim sh -c 'uname -a; cat /etc/debian_version'
Unable to find image 'public.ecr.aws/docker/library/debian:bookworm-slim' locally
5b6c2c6cbc90: pulled (468 B)
2d66c782e484: pulled (28.1 MB)
Digest: sha256:ec8534cda2cb30f29f44f23e135297af7a1496f2990fd8e4e84aa36b081a757a
Linux (none) 6.18.63-oci-hermit-2 #1 SMP 2026-11-10 18:07:06 UTC aarch64 GNU/Linux
12.15
```

That pulled a Debian image on the host, checked every byte of it against its digest, booted a sandbox, ran the
command in it and powered the sandbox off again, in about a second on a recent laptop. `uname` shows oci-hermit's own
Linux kernel, whatever the host is. For a shell, add `-it` and run `bash`.

## What makes it different

- **No elevated rights.** It installs by unpacking a folder and runs as you. It installs no driver, service or
  network adapter. [Running without admin rights](no-admin.md) says exactly what it needs.
- **No hardware virtualization.** QEMU always runs with `-accel tcg`, so KVM, Hyper-V and Apple's Hypervisor
  framework are never used.
- **Sealed.** The guest has no network device and no path out. Images are pulled by the host tool, never by the
  guest.
- **Both architectures everywhere.** Because the CPU is emulated, amd64 and arm64 images run on any host.
- **Docker-style commands,** with a few deliberate differences: see [oci-hermit, Docker and Podman](docker-and-podman.md).

## Where to go

<div class="grid cards" markdown>

-   **[Getting started](getting-started.md)**

    Download the package for Windows, macOS or Linux, check it, and run your first image.

-   **[Field manual](field-manual/index.md)**

    Walk-throughs by task: a shell in a public image, a private registry, images without a registry, moving images
    between machines, the image store, managed devices, platforms.

-   **[Command-line reference](cli.md)**

    Every command and flag, with exit codes, and the commands that are still planned.

-   **[When to use oci-hermit](when-to-use.md)**

    What it is good for, and where it differs from the container runtime you deploy to.

-   **[Configuration](configuration.md)**

    Every environment variable, where things are kept on each OS, and what `machine info` shows.

-   **[How it works](how-it-works.md)**

    The sandbox, what isolates it, the image store and digest checks.

-   **[Troubleshooting](troubleshooting.md)**

    SmartScreen, Gatekeeper, `noexec`, boot timeouts, registry errors, and where the logs are.

-   **[Contributing](contributing.md)**

    The architecture, the development workflow, and how QEMU, the kernel and the agent are built.

</div>
