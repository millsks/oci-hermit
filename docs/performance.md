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

- **prepare:** read the image, apply its layers, write the initramfs. Grows with the image's size.
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
| Memory for `run` | twice the image's size plus 256 MiB, at least 512 MiB | `run -m 2g` (at least `64m`) |
| Memory for `machine boot` | 512 MiB | `machine boot --memory 1024` (MiB) |

The memory is the guest's RAM, which QEMU reserves on your machine. It is twice the image's size
because today the image's files travel in the initramfs and are unpacked from it in the guest's RAM; layer disks
(#16, #17) will remove that. A large image therefore needs a lot of memory: a 1 GB image gets a 2.3 GB sandbox. If a
command runs out of memory, raise `-m`. More CPUs help commands that use several threads; booting does not get faster.

## Limits

- **One command per sandbox.** Each `run` boots, runs one command and powers off. There are no detached or named
  containers yet (#20, #22).
- **No network.** No outbound access at all, by design, and no published ports yet (#27).
- **No bind mounts yet** (#33). Put what the command needs in the image, or pass it on stdin with `-i`.
- **Image size.** The whole image is read into memory and copied into the initramfs. Images of a few hundred MB are
  fine; multi-GB images are slow to prepare and need a lot of RAM until layer disks arrive (#16, #17).
- **Layer compression.** Plain and gzip layers. zstd layers are not supported yet.
- **Platforms.** `linux/amd64` and `linux/arm64` only.
- **Terminal.** The terminal size is sent when a `-t` command starts; resizing the window afterwards is not passed on.
- **Inside the guest** there are no namespaces or cgroups yet, only `chroot` (crun comes with #21). Software that
  expects to manage cgroups, or to see its own PID namespace, may behave differently.
