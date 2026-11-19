# Performance and limits

oci-hermit emulates the guest's CPU in software. That is what lets it run without a hypervisor or admin rights, and
it is also why it is slower than Docker. It is meant to answer "does this image work", not "how fast is it".

## Start time

Starting a sandbox takes about a second on a recent laptop, and 2 to 6 seconds on GitHub's hosted runners. Booting
dominates. Set `OCI_HERMIT_TIMINGS=1` to see the breakdown of any `run`:

```console
$ OCI_HERMIT_TIMINGS=1 oci-hermit run public.ecr.aws/docker/library/debian:bookworm-slim bash -ic exit
timings: prepare 0.35s, boot 0.63s, command 0.05s, stop 0.03s, total 1.06s
```

- **prepare:** find the image, build any layer disk it does not have yet (once per layer), read the user from them,
  write the initramfs. For an image run from a file, apply its layers into the initramfs, which grows with its size.
- **boot:** start QEMU, boot the kernel, wait for the agent to be ready.
- **command:** start the command and wait for it to exit.
- **stop:** power the guest off.

Measured with `oci-hermit run debian.tar bash -ic exit` (Debian 12 slim), a release build and one vCPU; each row is
the run with the median total out of three:

| Host | Guest | Prepare | Boot | Command | Stop | Total |
| --- | --- | --- | --- | --- | --- | --- |
| Apple M4 Mac, macOS 26.2 | aarch64 | 0.31s | 0.58s | 0.05s | 0.02s | 0.96s |
| Apple M4 Mac, macOS 26.2 | x86_64 | 0.29s | 1.09s | 0.05s | 0.03s | 1.45s |
| GitHub `ubuntu-24.04-arm` (Linux arm64) | aarch64 | 0.37s | 1.40s | 0.09s | 0.02s | 1.88s |
| GitHub `ubuntu-24.04-arm` (Linux arm64) | x86_64 | 0.34s | 3.75s | 0.11s | 0.02s | 4.22s |
| GitHub `ubuntu-latest` (Ubuntu 26.04, x64) | aarch64 | 0.49s | 1.46s | 0.11s | 0.02s | 2.08s |
| GitHub `ubuntu-latest` (Ubuntu 26.04, x64) | x86_64 | 0.41s | 2.41s | 0.13s | 0.02s | 2.98s |
| GitHub `windows-latest` (Windows Server 2025, x64) | aarch64 | 0.70s | 1.43s | 0.11s | 0.02s | 2.26s |
| GitHub `windows-latest` (Windows Server 2025, x64) | x86_64 | 0.52s | 2.80s | 0.16s | 0.04s | 3.54s |
| GitHub `macos-latest` (macOS 26, Apple silicon) | aarch64 | 0.49s | 2.35s | 0.12s | 0.09s | 3.04s |
| GitHub `macos-latest` (macOS 26, Apple silicon) | x86_64 | 0.50s | 2.93s | 0.14s | 0.03s | 3.61s |
| GitHub `macos-15-intel` (macOS 15, Intel) | aarch64 | 0.96s | 4.91s | 0.24s | 0.06s | 6.17s |
| GitHub `macos-15-intel` (macOS 15, Intel) | x86_64 | 0.61s | 3.32s | 0.19s | 0.04s | 4.17s |

The CI rows come from the `hosts` job in `kernel.yml`, which records the same runs in every job summary. Hosted
runners are small shared VMs, so they are slower and noisier than a laptop. An x86_64 guest boots more slowly than
an aarch64 one on every host except the Intel Mac. More detail is in the
[design document](design.md#measured-start-times).

A boot much slower than these on similar hardware often means security software is inspecting QEMU as it generates
code; see [Troubleshooting](troubleshooting.md#it-is-much-slower-than-expected).

## Running time

Once started, everything the command does runs on the emulated CPU. Expect it to be roughly 5 to 20 times slower than
native, depending on the work and the host. As an indication: a shell loop counting to a million took 2.5 seconds in
the Apple M4 Mac's own `/bin/sh`, and about 12 seconds in Alpine's `sh` in either guest (`time` inside the sandbox):

```console
$ oci-hermit run --platform linux/arm64 public.ecr.aws/docker/library/alpine:3.20 \
    sh -c "time sh -c 'i=0; while [ \$i -lt 1000000 ]; do i=\$((i+1)); done'"
real	0m 12.05s
user	0m 12.04s
sys	0m 0.00s
```

| Work | What to expect |
| --- | --- |
| An interactive shell, small scripts | Responsive. |
| A web frontend or backend answering test requests | Fine for smoke tests (once published ports arrive, #27). |
| Compiling, large data loads, test suites that are CPU-bound | Many times slower than native. Out of scope; run them on a real container runtime. |
| Timing-sensitive tests | May be flaky. |

## Memory and CPUs

| | Default | Change with |
| --- | --- | --- |
| CPUs | 1 | `run --cpus N`, `machine boot --cpus N` |
| Memory for `run` | 512 MiB for a stored image; twice the size plus 256 MiB for one run from a file | `run -m 2g` (at least `64m`) |
| Memory for `machine boot` | 512 MiB | `machine boot --memory 1024` (MiB) |

The memory is the guest's RAM, which QEMU reserves on your machine. An image from the store (pulled or loaded) runs
from read-only disks, one per layer, so 512 MiB does whatever its size; what the command writes is kept in that RAM
until the sandbox stops. An image run straight from a file (`run image.tar`), or one with more than 20 layers,
travels in the initramfs and is unpacked in the guest's RAM instead, so it gets twice its size plus 256 MiB. If a
command runs out of memory, raise `-m`. More CPUs help commands that use several threads; booting does not get faster.

## Limits

- **One command per sandbox.** Each `run` boots, runs one command and powers off, in the foreground or with `-d` in
  the background. Running more commands in a running container (`exec`) comes with #22.
- **No network.** No outbound access at all, by design, and no published ports yet (#27).
- **No bind mounts yet** (#33). Put what the command needs in the image, or pass it on stdin with `-i`.
- **Image size.** A stored image's layers become disks, once each, so the first `run` of a large image takes a moment
  longer and later ones do not. An image run straight from a file is read into memory and copied into the
  initramfs: fine for a few hundred MB, slow for multi-GB images. `load` it first to run it from disks.
- **Layer compression.** Plain and gzip layers. zstd layers are not supported yet.
- **Platforms.** `linux/amd64` and `linux/arm64` only.
- **Terminal.** The terminal size is sent when a `-t` command starts; resizing the window afterwards is not passed on.
- **Inside the guest** each command runs in a crun container with Docker's defaults, but with no seccomp profile
  yet (#102) and no resource limits beyond the VM's size.
