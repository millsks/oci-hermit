# Running without admin rights

oci-hermit is meant for machines where you cannot install Docker or Podman: no admin rights, no hypervisor, no WSL.
This page says what it needs from the machine, what CI proves on every change, and how to check a locked-down
(managed) device that CI cannot reproduce.

## What oci-hermit needs

Nothing that requires elevation. A `run`:

- starts the bundled QEMU as a normal child process, always with `-accel tcg` (a software CPU), so KVM, Hyper-V
  (WHPX) and Apple's Hypervisor framework are never used and need not be enabled;
- reads the image, kernel and agent from wherever oci-hermit is installed, and never writes there;
- writes the sandbox's initramfs and logs under your temp directory (`TMPDIR`, or `TMP` on Windows), in
  `oci-hermit/sandbox-*`;
- talks to the guest over QEMU's stdin and stdout only: no sockets, no network, no listening ports;
- on Windows, puts QEMU in a Job Object so it ends when oci-hermit does. Any user can create one.

It installs no driver, service, kernel extension or network adapter, and changes no system setting.

## What CI proves

Every change runs the `hosts` job in `.github/workflows/kernel.yml` on five kinds of host (Windows x64, macOS on
Apple silicon and Intel, Linux x64 and arm64), each with an x86_64 and an aarch64 guest. After the usual checks, each
job:

1. creates a new account with no admin rights:
   - **Windows:** a local account in the Users group only. The job checks it is not in Administrators
     (`S-1-5-32-544`) and runs at medium integrity, so nothing is elevated.
   - **macOS:** a standard account. The job checks it is not in the `admin` group and cannot use `sudo`.
   - **Linux:** a new account in no `sudo`, `adm`, `wheel`, `kvm`, `docker` or `lxd` group. The job checks it cannot
     use `sudo`, and takes `/dev/kvm` away from everyone but root first.
2. installs oci-hermit, the QEMU bundle, the guest and a Debian image in a folder the account cannot write to;
3. runs `oci-hermit run debian.tar sh -c '...; exit 7'` as that account and checks the output and the exit code.

The job summary shows the account's identity, its groups and the command's output. That covers the "no admin
rights" part on stock Windows, macOS and Linux.

## What CI cannot prove

GitHub's runners are not managed devices. Organisations lock machines down with tools that CI does not have, and any
of them can stop oci-hermit even though it needs no admin rights:

- **Application allow-listing:** AppLocker or App Control for Business (WDAC) on Windows; Santa or an MDM profile on
  macOS; fapolicyd on Linux.
- **Endpoint protection:** Microsoft Defender attack surface reduction rules, or another EDR agent that blocks or
  quarantines unknown binaries, or kills a process that generates code at run time (QEMU's TCG does).
- **Downloaded-file checks:** SmartScreen and the Mark of the Web on Windows, Gatekeeper and quarantine on macOS.
  Release binaries will be signed (#39); until then expect these to complain.
- **Mount options:** a home or temp directory mounted `noexec` on Linux.

The checklist below is for trying oci-hermit on such a device. Results from real managed devices are tracked in #63.

## Checklist for a managed device

Use your normal account, with no elevation and no help from IT. Record what you see at each step: the exact message,
and any notification from the security software. Stop at the first step that fails; the later steps depend on it.

Before you start, check that this is allowed. Running an emulator may go against the intent of a policy that blocks
Docker or Podman, even though it needs no admin rights. See [When to use oci-hermit](when-to-use.md).

### Every operating system

- [ ] **Download and unpack** the release archive into a folder in your home directory. Note any warning from the
      browser or the operating system.
- [ ] **Start it:** `oci-hermit version` prints the version and the control protocol version.
- [ ] **Check the bundled QEMU:** `oci-hermit machine info` lists both emulators with `accel tcg`.
- [ ] **Boot a sandbox:** `oci-hermit machine boot` prints `agent ready after …s` and then `guest powered off`.
- [ ] **Run an image:** `oci-hermit run debian.tar sh -c 'echo hello; exit 7'` prints `hello`, and the exit code is 7
      (`echo $?`, or `echo %ERRORLEVEL%` in `cmd`).
- [ ] **Use a terminal:** `oci-hermit run -it debian.tar bash` gives a prompt. Type `ls /` and `exit`.
- [ ] **Check the timing:** with `OCI_HERMIT_TIMINGS=1` set, `run` prints how long each phase took. A boot much slower
      than in [Measured start times](design.md#measured-start-times) can mean security software is inspecting QEMU.

### Windows

- [ ] **Mark of the Web:** if the archive came from a browser, check whether SmartScreen blocks `oci-hermit.exe` or
      `qemu-system-*.exe`, and whether unblocking (file Properties, "Unblock") is allowed.
- [ ] **Allow-listing:** if AppLocker or App Control is on, note whether it blocks `oci-hermit.exe`, the QEMU
      executables or their DLLs. The Event Viewer logs under *Applications and Services Logs > Microsoft > Windows >
      AppLocker* or *CodeIntegrity* name the blocked file.
- [ ] **Defender:** check *Windows Security > Protection history* for blocks or quarantines after each step.
- [ ] **Location:** if the first try was blocked, try again from a folder IT allows for tools, if there is one.
- [ ] **Console:** run `oci-hermit run -it debian.tar bash` in Windows Terminal and in the older console host. Check
      that arrow keys, Ctrl-C and resizing behave.

### macOS

- [ ] **Gatekeeper:** if the archive came from a browser, check whether macOS refuses to open the binaries, and
      whether *System Settings > Privacy & Security > Open Anyway* is offered or hidden by your organisation.
- [ ] **Quarantine:** check whether `xattr -dr com.apple.quarantine <folder>` is allowed. If it is, retry.
- [ ] **Allow-listing:** note any Santa or MDM message naming `oci-hermit` or `qemu-system-*`.
- [ ] **Endpoint protection:** note any alert about a process writing executable memory. QEMU generates code at run
      time.

### Linux

- [ ] **`noexec` mounts:** run `findmnt -no OPTIONS --target ~` and `findmnt -no OPTIONS --target "${TMPDIR:-/tmp}"`.
      `noexec` on your home blocks the binaries. oci-hermit only reads from the temp directory, so `noexec` there is
      fine.
- [ ] **Policy:** if `run` fails with "permission denied" on a binary that is executable, check `fapolicyd`, SELinux
      (`ausearch -m avc -ts recent`) or AppArmor (`journalctl -k | grep apparmor`) for a denial.

### Reporting

Add the results to #63, or open a new issue for each blocker. Include the operating system and version, the security
software if you know it, the step that failed, and the exact message. Leave out anything your organisation treats as
confidential, such as host names, policy names or internal paths.
