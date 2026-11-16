# When to use oci-hermit

oci-hermit is a development and testing tool. It runs a real OCI image on a machine where Docker and Podman are not an option, at the cost of speed. It does not replace the container runtime you deploy to.

## What it is good for

- **Testing the image you will ship.** It runs the exact OCI image you deploy: the same bytes, filesystem, entrypoint, command and environment. "Does it start, does the shell work, does the app answer on its port" get real answers.
- **Machines you are not allowed to change.** Everything runs as your own user: no root, no admin rights, no kernel driver, no hypervisor, no WSL. The guest runs on an emulated CPU (QEMU TCG), so nothing needs enabling in the firmware or in Windows features.
- **Strong isolation.** The boundary is a whole virtual machine, stronger than a container's namespaces. The guest has no network path out, and you reach it only through ports you publish on `127.0.0.1`.
- **Other architectures.** An arm64 image runs on an x86-64 machine and the other way round. Docker's cross-architecture support uses the same emulation.

## Where it differs from production

- **Speed.** Expect roughly 5–20 times slower than native, and slower again when emulating x86-64 on arm64. It answers "does it work", not "how fast is it". Timing-sensitive tests may be flaky.
- **Kernel.** Images run on oci-hermit's small guest kernel, not your production hosts' kernel. Most applications never notice. Anything that depends on particular kernel features, cgroup limits or system calls may behave differently.
- **Runtime.** For now, `oci-hermit run` starts the command in a `chroot` inside the guest. A real OCI runtime (crun) is planned. Even then, it will not behave exactly like Docker, Kubernetes or your cloud's container service.
- **Deliberate defaults.**
    - There is no outbound network at all, so an application that needs to reach the internet fails here and works in production.
    - Bind mounts are read-only unless you add `:rw`.
    - Published ports bind to `127.0.0.1` only.

## Recommendations

- **Keep a real gate before you deploy.** Use oci-hermit for the inner loop. Run the final validation, such as integration tests or a staging deploy, on a real container runtime, for example a Linux CI runner with Docker or Podman.
- **Check your organisation's policy.** If Docker and Podman are blocked on your machine on purpose, running a virtual machine emulator may go against that policy even though it needs no admin rights. Get it approved; do not treat "no admin needed" as permission.
- **Build images elsewhere, for now.** oci-hermit runs images but does not build them yet. [pixi-oci](https://github.com/millsks/pixi-oci) builds and pulls images without Docker, and writes the OCI archives oci-hermit runs.
