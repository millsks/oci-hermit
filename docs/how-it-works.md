---
# The diagrams need the width the table of contents would take.
hide:
  - toc
---

# How it works

This page describes what oci-hermit does today, and marks what is planned. The full design, including the parts not
built yet, is in the [design document](design.md).

## The short version

Each `oci-hermit run` is one small virtual machine, started for that command and powered off when it ends:

1. **On the host,** oci-hermit finds the image: a local OCI layout or `.tar`, else an image in its store, else it
   pulls the image from the registry. Every manifest, config and layer is checked against its sha256 digest.
2. It turns each of the image's layers into a read-only EROFS disk, once per layer (later runs reuse them), works
   out the user to run as from the image's own `/etc/passwd` on those disks, and writes an initramfs with the guest
   agent as `/init`. Nothing is mounted on the host and nothing needs privileges. (An image run straight from a
   file has its files put in the initramfs under `/rootfs` instead.)
3. It starts the bundled QEMU as a normal child process, with a software CPU (`-accel tcg`), no network device, and
   the layer disks attached read-only, and boots oci-hermit's own Linux kernel with that initramfs.
4. **In the guest,** the agent runs as PID 1. It moves its root off the initramfs onto a tmpfs (so crun can
   `pivot_root` later), mounts `/proc`, `/sys`, the cgroup filesystem and `/dev`, says it is ready, and waits for
   instructions on a private serial channel.
5. oci-hermit sends the command and the disks' serial numbers. The agent mounts each disk read-only and stacks them
   with overlayfs on `/rootfs`, under a writable layer in the guest's memory that is thrown away with the sandbox,
   so nothing the command writes reaches the disks. It writes an OCI runtime bundle for the command (its user,
   environment, working directory and terminal) and starts it in a container with crun: its own PID, mount, IPC,
   UTS, network and cgroup namespaces, Docker's default capabilities, a read-only `/sys` and only the usual devices.
   The agent relays the container's input, output and exit code.
6. When the command exits, oci-hermit asks the agent to power off, and exits with the command's exit code.

## Components

<figure class="oh-diagram">
<div class="oh-scroll">
<svg viewBox="0 0 1000 760" role="img" aria-labelledby="oh-comp-title oh-comp-desc">
<title id="oh-comp-title">oci-hermit components</title>
<desc id="oh-comp-desc">The host tool's modules, the QEMU helper process with its control port and serial console, and the guest VM with its kernel, hermit-agent as PID 1, and the command running in a crun container as the image's user. A dashed box lists the planned parts: a seccomp profile, networking, bind mounts and an OS sandbox around QEMU.</desc>
<defs>
<marker id="oh-ah" class="ah" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
<marker id="oh-ah-acc" class="ah-acc" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
<marker id="oh-ah-mut" class="ah-mut" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
</defs>
<rect class="r-host" x="10" y="10" width="410" height="740" rx="12"/>
<text x="26" y="34" class="t lbl-host">HOST &#183; your user account, no root, no admin</text>
<rect class="r-host-box" x="30" y="46" width="370" height="34" rx="6"/>
<text x="44" y="68" class="mono">$ oci-hermit run -it IMAGE CMD</text>
<rect class="r-host-box" x="30" y="92" width="116" height="46" rx="6"/>
<text x="88" y="111" text-anchor="middle" class="b">images</text>
<text x="88" y="128" text-anchor="middle" class="m small">store, .tar or dir</text>
<rect class="r-host-box" x="157" y="92" width="116" height="46" rx="6"/>
<text x="215" y="111" text-anchor="middle" class="b">QEMU bundle</text>
<text x="215" y="128" text-anchor="middle" class="m small">qemu-system-*</text>
<rect class="r-host-box" x="284" y="92" width="116" height="46" rx="6"/>
<text x="342" y="111" text-anchor="middle" class="b">guest dir</text>
<text x="342" y="128" text-anchor="middle" class="m small">kernel + agent</text>
<path class="ln-muted" d="M88,138 V151" marker-end="url(#oh-ah-mut)"/>
<path class="ln-muted" d="M215,138 V151" marker-end="url(#oh-ah-mut)"/>
<path class="ln-muted" d="M342,138 V151" marker-end="url(#oh-ah-mut)"/>
<rect class="r-host-box" x="30" y="152" width="370" height="452" rx="10"/>
<text x="44" y="173" class="b">oci-hermit &#183; Rust host tool</text>
<rect class="r-mod" x="42" y="182" width="346" height="28" rx="5"/>
<text x="54" y="201" class="mono b">cli.rs</text><text x="166" y="201" class="m">clap; Docker-style commands</text>
<rect class="r-mod" x="42" y="214" width="346" height="28" rx="5"/>
<text x="54" y="233" class="mono b">main.rs</text><text x="166" y="233" class="m">dispatch; run; machine boot, info</text>
<rect class="r-mod" x="42" y="246" width="346" height="28" rx="5"/>
<text x="54" y="265" class="mono b">registry.rs</text><text x="166" y="265" class="m">pull on the host; logins</text>
<rect class="r-mod" x="42" y="278" width="346" height="28" rx="5"/>
<text x="54" y="297" class="mono b">load.rs</text><text x="166" y="297" class="m">load -i: docker save, OCI</text>
<rect class="r-mod" x="42" y="310" width="346" height="28" rx="5"/>
<text x="54" y="329" class="mono b">store.rs</text><text x="166" y="329" class="m">image store (OCI layout), lock</text>
<rect class="r-mod" x="42" y="342" width="346" height="28" rx="5"/>
<text x="54" y="361" class="mono b">images.rs</text><text x="166" y="361" class="m">images, rmi, inspect, tag, save</text>
<rect class="r-mod" x="42" y="374" width="346" height="28" rx="5"/>
<text x="54" y="393" class="mono b">image.rs</text><text x="166" y="393" class="m">manifest per arch; sha256 checks</text>
<rect class="r-mod" x="42" y="406" width="346" height="28" rx="5"/>
<text x="54" y="425" class="mono b">rootfs.rs</text><text x="166" y="425" class="m">apply layers in memory</text>
<rect class="r-mod" x="42" y="438" width="346" height="28" rx="5"/>
<text x="54" y="457" class="mono b">users.rs</text><text x="166" y="457" class="m">User from the image's /etc/passwd</text>
<rect class="r-mod" x="42" y="470" width="346" height="28" rx="5"/>
<text x="54" y="489" class="mono b">qemu.rs</text><text x="166" y="489" class="m">locate + verify the bundle</text>
<rect class="r-mod" x="42" y="502" width="346" height="28" rx="5"/>
<text x="54" y="521" class="mono b">guest.rs</text><text x="166" y="521" class="m">kernel + agent per arch</text>
<rect class="r-mod" x="42" y="534" width="346" height="28" rx="5"/>
<text x="54" y="553" class="mono b">initramfs.rs</text><text x="166" y="553" class="m">newc cpio, no privileges</text>
<rect class="r-mod" x="42" y="566" width="346" height="28" rx="5"/>
<text x="54" y="585" class="mono b">supervisor.rs</text><text x="166" y="585" class="m">QEMU argv, Sandbox, protocol</text>
<path class="ln-muted" d="M30,63 H20 V196 H40" marker-end="url(#oh-ah-mut)"/>
<rect class="r-box" x="30" y="622" width="370" height="114" rx="8"/>
<rect class="r-mod" x="42" y="632" width="104" height="58" rx="5"/>
<text x="94" y="655" text-anchor="middle" class="mono b">initrd.cpio</text>
<text x="94" y="675" text-anchor="middle" class="m small">/init + /rootfs</text>
<rect class="r-mod" x="164" y="632" width="104" height="58" rx="5"/>
<text x="216" y="655" text-anchor="middle" class="mono b">console.log</text>
<text x="216" y="675" text-anchor="middle" class="m small">serial console</text>
<rect class="r-mod" x="286" y="632" width="104" height="58" rx="5"/>
<text x="338" y="655" text-anchor="middle" class="mono b">qemu.log</text>
<text x="338" y="675" text-anchor="middle" class="m small">QEMU stderr</text>
<text x="44" y="718" class="mono m">$TMPDIR/oci-hermit/sandbox-&lt;pid&gt;-&lt;ns&gt;/</text>
<path class="ln-muted" d="M94,604 V631" marker-end="url(#oh-ah-mut)"/>
<path class="ln-accent" d="M388,580 H446 V97 H499" marker-start="url(#oh-ah-acc)" marker-end="url(#oh-ah-acc)"/>
<text transform="translate(463 340) rotate(-90)" text-anchor="middle" class="b small lbl-accent">stdin / stdout pipes &#183; NDJSON</text>
<path class="ln-muted" d="M400,656 H479" marker-end="url(#oh-ah-mut)"/>
<text x="440" y="649" text-anchor="middle" class="mono small m">-initrd</text>
<path class="ln-muted ln-dash" d="M479,706 H401" marker-end="url(#oh-ah-mut)"/>
<text x="440" y="699" text-anchor="middle" class="small m">logs</text>
<rect class="r-qemu" x="480" y="10" width="510" height="740" rx="12"/>
<text x="496" y="34" class="t lbl-qemu">QEMU helper &#183; separate GPL-2.0 process</text>
<text x="496" y="54" class="mono small m">-accel tcg -nodefaults -no-user-config -display none</text>
<rect class="r-qemu-box" x="500" y="68" width="232" height="60" rx="6"/>
<text x="512" y="88" class="b">virtio-serial port</text>
<text x="512" y="105" class="mono small">org.oci-hermit.control</text>
<text x="512" y="121" class="m small">chardev stdio, signal=off</text>
<rect class="r-qemu-box" x="748" y="68" width="222" height="60" rx="6"/>
<text x="760" y="88" class="b">serial console</text>
<text x="760" y="105" class="mono small">ttyS0 / ttyAMA0</text>
<text x="760" y="121" class="m small">chardev file &#8594; console.log</text>
<rect class="r-guest" x="500" y="142" width="470" height="396" rx="10"/>
<text x="740" y="163" text-anchor="middle" class="b lbl-guest">guest VM &#183; microvm (x86_64) / virt (aarch64)</text>
<rect class="r-guest-box" x="620" y="174" width="240" height="52" rx="6"/>
<text x="740" y="195" text-anchor="middle" class="b">Linux 6.18 LTS kernel</text>
<text x="740" y="214" text-anchor="middle" class="m small">tinyconfig + fragments, no modules</text>
<rect class="r-guest-box" x="520" y="238" width="430" height="140" rx="8"/>
<text x="534" y="260" class="b">hermit-agent &#183; PID 1 (/init) &#183; static glibc</text>
<text x="534" y="282" class="small">&#8226; mounts /proc, /sys, /dev, /dev/pts, /run, /tmp</text>
<text x="534" y="301" class="small">&#8226; signalfd (TERM, INT, CHLD) + poll on the control port</text>
<text x="534" y="320" class="small">&#8226; Framer: lines &#8804; 64 KiB; bad input gets an error reply</text>
<text x="534" y="339" class="small">&#8226; one Run per sandbox; relays I/O; reaps children</text>
<text x="534" y="358" class="small">&#8226; never exits: sync, then reboot(POWER_OFF)</text>
<path class="ln-accent" d="M580,128 V237" marker-start="url(#oh-ah-acc)" marker-end="url(#oh-ah-acc)"/>
<path class="ln-muted" d="M910,238 V129" marker-end="url(#oh-ah-mut)"/>
<rect class="r-chroot" x="520" y="392" width="430" height="134" rx="8"/>
<text x="534" y="412" class="b lbl-cmd small">crun container on /rootfs &#183; own pid, mnt, ipc, uts, net, cgroup ns</text>
<rect class="r-cmd" x="540" y="424" width="396" height="92" rx="6"/>
<text x="554" y="444" class="b">container command</text>
<text x="554" y="464" class="small">crun run --bundle /run/oci-hermit &#183; Docker's capabilities</text>
<text x="554" y="482" class="small">-t: pty from crun's console socket &#183; otherwise 3 pipes</text>
<text x="554" y="500" class="small">PID 1 of its namespace, as the image's User, or -u</text>
<path class="ln" d="M910,378 V423" marker-end="url(#oh-ah)"/>
<text x="902" y="408" text-anchor="end" class="mono small">Run</text>
<rect class="r-plan" x="500" y="556" width="470" height="154" rx="8"/>
<text x="514" y="578" class="b lbl-plan small">PLANNED &#183; not in the code yet</text>
<text x="514" y="602" class="small"><tspan class="mono b">M3</tspan>  Docker's default seccomp profile in containers (#102)</text>
<text x="514" y="624" class="small"><tspan class="mono b">M4</tspan>  libslirp NIC, restrict=on; -p on 127.0.0.1 only (#26, #27)</text>
<text x="514" y="646" class="small"><tspan class="mono b">M5</tspan>  9P server over virtio-serial; mounts ro by default (#30&#8211;#33)</text>
<text x="514" y="668" class="small"><tspan class="mono b">M7</tspan>  OS sandbox around QEMU: seccomp, App Sandbox, AppContainer (#38)</text>
<text x="514" y="716" class="small m">Today the VM has no network device.</text>
</svg>
</div>
<figcaption>The green arrows are the control channel. oci-hermit holds QEMU's stdin and stdout as an anonymous pipe pair, QEMU's <code>-chardev stdio</code> connects them to the virtio-serial port, and the agent finds the port through <code>/sys/class/virtio-ports</code>. Nothing on the filesystem or the network can reach it. QEMU must never print to stdout, so its diagnostics go to <code>qemu.log</code>. Registry pulls, logins and digest checks all happen in the host tool; the guest never sees a registry or a credential.</figcaption>
</figure>

| Part | What it is |
| --- | --- |
| Host tool | `oci-hermit`, one Rust binary. The only process you start. It reads images, talks to registries, builds the initramfs and supervises QEMU. |
| QEMU | The bundled emulator (`qemu/` in the package), started as a separate process. oci-hermit builds it itself, with only the two guest targets and no hypervisor accelerators, and refuses any other QEMU: its `--version` must be exactly `11.1.3 (oci-hermit-qemu-4)` and it must offer the `tcg` accelerator. See [Building QEMU](building-qemu.md). |
| Guest kernel | Linux 6.18 LTS, built by the project from `tinyconfig` plus a short list of options. x86_64 guests use QEMU's `microvm` machine; aarch64 guests use `virt`. See [Building the guest](building-guest.md). |
| Guest agent | `hermit-agent`, a static Rust binary that runs as PID 1. It never exits: on a shutdown request or any fatal error it syncs and powers the VM off. |
| Container runtime | crun 1.30, built statically by the project with json-c, libseccomp and libcap. The agent hands it an OCI runtime bundle per command. |
| Control protocol | Newline-delimited JSON over the virtio-serial port `org.oci-hermit.control`, at most 64 KiB a line. Version 1.4. |

## What happens when you run

<figure class="oh-diagram">
<div class="oh-scroll">
<svg viewBox="0 0 1000 905" role="img" aria-labelledby="oh-seq-title oh-seq-desc">
<title id="oh-seq-title">Sequence of oci-hermit run -it</title>
<desc id="oh-seq-desc">Prepare on the host: choose the architecture, locate QEMU, find or pull the image with digest checks, locate the guest, apply the layers and resolve the user, write the initramfs. Boot QEMU, then the Hello and Ready handshake. Run and Started, a loop of Stdin and Output, Exited. Then Shutdown and ShuttingDown, and the guest powers off.</desc>
<defs>
<marker id="oh-ah2" class="ah" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
<marker id="oh-ah-host" class="ah-host" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
<marker id="oh-ah-guest" class="ah-guest" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z"/></marker>
</defs>
<rect class="band-a" x="0" y="66" width="1000" height="230"/>
<rect class="band-b" x="0" y="300" width="1000" height="152"/>
<rect class="band-a" x="0" y="456" width="1000" height="282"/>
<rect class="band-b" x="0" y="742" width="1000" height="160"/>
<text x="992" y="84" text-anchor="end" class="mono small m">PREPARE</text>
<text x="992" y="318" text-anchor="end" class="mono small m">BOOT</text>
<text x="992" y="474" text-anchor="end" class="mono small m">COMMAND</text>
<text x="992" y="760" text-anchor="end" class="mono small m">STOP</text>
<line class="life" x1="70" y1="58" x2="70" y2="895"/>
<line class="life" x1="225" y1="58" x2="225" y2="895"/>
<line class="life" x1="390" y1="58" x2="390" y2="895"/>
<line class="life" x1="560" y1="58" x2="560" y2="895"/>
<line class="life" x1="730" y1="58" x2="730" y2="895"/>
<line class="life" x1="905" y1="58" x2="905" y2="895"/>
<rect class="r-box" x="10" y="12" width="120" height="46" rx="6"/>
<text x="70" y="33" text-anchor="middle" class="b">you</text><text x="70" y="50" text-anchor="middle" class="m small">terminal</text>
<rect class="r-host" x="160" y="12" width="130" height="46" rx="6"/>
<text x="225" y="33" text-anchor="middle" class="b">oci-hermit</text><text x="225" y="50" text-anchor="middle" class="m small">main + supervisor</text>
<rect class="r-host-box" x="325" y="12" width="130" height="46" rx="6"/>
<text x="390" y="33" text-anchor="middle" class="b">host files</text><text x="390" y="50" text-anchor="middle" class="m small">bundle, guest, store</text>
<rect class="r-qemu" x="495" y="12" width="130" height="46" rx="6"/>
<text x="560" y="33" text-anchor="middle" class="b">QEMU (TCG)</text><text x="560" y="50" text-anchor="middle" class="m small">helper process</text>
<rect class="r-guest" x="665" y="12" width="130" height="46" rx="6"/>
<text x="730" y="33" text-anchor="middle" class="b">hermit-agent</text><text x="730" y="50" text-anchor="middle" class="m small">PID 1</text>
<rect class="r-cmd" x="845" y="12" width="120" height="46" rx="6"/>
<text x="905" y="33" text-anchor="middle" class="b">command</text><text x="905" y="50" text-anchor="middle" class="m small">crun container</text>
<line class="ln" x1="70" y1="92" x2="223" y2="92" marker-end="url(#oh-ah2)"/>
<text x="147" y="86" text-anchor="middle" class="mono small halo">run -it IMAGE CMD</text>
<path class="ln" d="M225,114 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="262" y="125" class="small halo">arch: --platform, else the host's (then the other one, with a warning)</text>
<line class="ln" x1="225" y1="152" x2="388" y2="152" marker-end="url(#oh-ah2)"/>
<text x="307" y="146" text-anchor="middle" class="mono small halo">qemu::locate</text>
<text x="400" y="156" class="small m halo">$OCI_HERMIT_QEMU_DIR, then the install layouts beside oci-hermit</text>
<line class="ln" x1="225" y1="182" x2="388" y2="182" marker-end="url(#oh-ah2)"/>
<text x="307" y="176" text-anchor="middle" class="mono small halo">open_image</text>
<text x="400" y="186" class="small m halo">a path, else a stored name or ID, else pulled; every blob checked against its digest</text>
<line class="ln" x1="225" y1="212" x2="388" y2="212" marker-end="url(#oh-ah2)"/>
<text x="307" y="206" text-anchor="middle" class="mono small halo">verify, guest::locate</text>
<text x="400" y="216" class="small m halo">"11.1.3 (oci-hermit-qemu-4)", tcg; &lt;arch&gt;/kernel, hermit-agent and crun</text>
<path class="ln" d="M225,234 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="262" y="245" class="small halo">rootfs(): layers in order; whiteouts, opaque dirs; ".." refused; User from /etc/passwd</text>
<path class="ln" d="M225,264 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="262" y="275" class="small halo">initrd.cpio = agent as /init + /dev/console + /rootfs; RAM = 256 MiB + 2 x image (min 512)</text>
<line class="ln" x1="225" y1="324" x2="558" y2="324" marker-end="url(#oh-ah2)"/>
<text x="392" y="318" text-anchor="middle" class="small halo">spawn qemu-system-&lt;arch&gt;; stdin/stdout piped, stderr &#8594; qemu.log</text>
<line class="ln" x1="560" y1="352" x2="728" y2="352" marker-end="url(#oh-ah2)"/>
<text x="644" y="346" text-anchor="middle" class="small halo">boot kernel, exec /init</text>
<path class="ln" d="M730,368 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="766" y="379" class="small halo">mounts, open port, "ready"</text>
<line class="ln-host-msg" x1="225" y1="410" x2="728" y2="410" marker-end="url(#oh-ah-host)"/>
<text x="476" y="404" text-anchor="middle" class="mono small b halo">Hello {version: [1,4]}</text>
<line class="ln-guest-msg" x1="730" y1="438" x2="227" y2="438" marker-end="url(#oh-ah-guest)"/>
<text x="476" y="432" text-anchor="middle" class="mono small b halo">Ready {version: [1,4]}</text>
<text x="740" y="442" class="small m halo">major must match; host waits up to 120 s</text>
<path class="ln" d="M225,478 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="262" y="489" class="small halo">raw mode (-t on a terminal); stdin reader thread (-i)</text>
<line class="ln-host-msg" x1="225" y1="516" x2="728" y2="516" marker-end="url(#oh-ah-host)"/>
<text x="476" y="510" text-anchor="middle" class="mono small b halo">Run {argv, env, cwd, tty, user, layers, hostname}</text>
<line class="ln" x1="730" y1="542" x2="903" y2="542" marker-end="url(#oh-ah2)"/>
<text x="817" y="536" text-anchor="middle" class="small halo">bundle; crun run, as the user</text>
<line class="ln-guest-msg" x1="730" y1="568" x2="227" y2="568" marker-end="url(#oh-ah-guest)"/>
<text x="476" y="562" text-anchor="middle" class="mono small b halo">Started {pid}</text>
<text x="740" y="572" class="small m halo">within 60 s</text>
<rect class="loop" x="40" y="584" width="900" height="122" rx="4"/>
<rect class="loop-tab" x="40" y="584" width="150" height="20" rx="3"/>
<text x="48" y="599" class="mono small">loop until exit</text>
<line class="ln" x1="70" y1="620" x2="223" y2="620" marker-end="url(#oh-ah2)"/>
<text x="147" y="614" text-anchor="middle" class="small halo">stdin bytes</text>
<line class="ln-host-msg" x1="225" y1="646" x2="728" y2="646" marker-end="url(#oh-ah-host)"/>
<text x="476" y="640" text-anchor="middle" class="mono small b halo">Stdin {data}  (base64, &#8804; 16 KiB raw)</text>
<line class="ln" x1="730" y1="646" x2="903" y2="646" marker-end="url(#oh-ah2)"/>
<text x="817" y="640" text-anchor="middle" class="small halo">pty / pipe</text>
<line class="ln" x1="905" y1="672" x2="732" y2="672" marker-end="url(#oh-ah2)"/>
<text x="817" y="666" text-anchor="middle" class="small halo">output</text>
<line class="ln-guest-msg" x1="730" y1="672" x2="227" y2="672" marker-end="url(#oh-ah-guest)"/>
<text x="476" y="666" text-anchor="middle" class="mono small b halo">Output {stream, data}</text>
<line class="ln" x1="225" y1="696" x2="72" y2="696" marker-end="url(#oh-ah2)"/>
<text x="147" y="690" text-anchor="middle" class="small halo">stdout / stderr</text>
<line class="ln" x1="905" y1="726" x2="732" y2="726" marker-end="url(#oh-ah2)"/>
<text x="817" y="720" text-anchor="middle" class="small halo">exit &#8594; SIGCHLD, reap</text>
<line class="ln-guest-msg" x1="730" y1="726" x2="227" y2="726" marker-end="url(#oh-ah-guest)"/>
<text x="476" y="720" text-anchor="middle" class="mono small b halo">Exited {code}</text>
<line class="ln-host-msg" x1="225" y1="770" x2="728" y2="770" marker-end="url(#oh-ah-host)"/>
<text x="476" y="764" text-anchor="middle" class="mono small b halo">Shutdown</text>
<line class="ln-guest-msg" x1="730" y1="796" x2="227" y2="796" marker-end="url(#oh-ah-guest)"/>
<text x="476" y="790" text-anchor="middle" class="mono small b halo">ShuttingDown</text>
<text x="740" y="800" class="small m halo">within 30 s, else QEMU is killed</text>
<path class="ln" d="M730,814 h28 v14 h-26" marker-end="url(#oh-ah2)"/>
<text x="766" y="825" class="small halo">sync; reboot(RB_POWER_OFF)</text>
<line class="ln ln-dash" x1="730" y1="852" x2="562" y2="852" marker-end="url(#oh-ah2)"/>
<text x="644" y="846" text-anchor="middle" class="small halo">power off; QEMU exits</text>
<line class="ln" x1="225" y1="880" x2="72" y2="880" marker-end="url(#oh-ah2)"/>
<text x="235" y="884" class="small halo">exit with the command's code (125 if oci-hermit itself fails)</text>
</svg>
</div>
<figcaption>Green: host to guest. Blue: guest to host. Without <code>-i</code>, the host sends <code>StdinClosed</code> right after <code>Started</code>; with <code>-i</code> it sends it when your stdin reaches end of file. With a terminal (<code>-t</code>), all output arrives as stdout and the terminal stays open. The terminal size is sent once, in <code>Run</code>; resizing the window afterwards is not passed on yet. Set <code>OCI_HERMIT_TIMINGS=1</code> to print how long prepare, boot, command and stop took.</figcaption>
</figure>

The host side of this is split in two processes. `run` does the preparing, then starts the container's **monitor**,
a background `oci-hermit __monitor` of its own (as Podman's `conmon`; there is no daemon). The monitor takes the
container's running lock, starts QEMU (whose lifetime is tied to the monitor's), does the handshake, `Run` and
`Shutdown`, appends the output to the container's log and records the exit code. `run` talks to it over a local
channel, a Unix socket in the data folder that only you can open or a named pipe on Windows, in the same kind of
JSON lines: it attaches, gets `Booted`, `Started`, the output and `Exited`, and sends your stdin. Without `-d` the
monitor stops the sandbox if `run` goes away; with `-d`, `run` prints the container's ID after `Started` and returns,
and `logs` reads the log while `stop` asks the monitor over the same channel to power the sandbox off. `exec` asks
it to start another command in the container: the monitor sends the agent `Exec` (protocol 1.5) with a process
number, the agent runs it with `crun exec`, and that command's input, output and exit code travel tagged with its
number, to that `exec` alone.

## Isolation

The security boundary is the emulator process. Guest code never runs directly on your CPU: QEMU translates every
instruction, and the guest can touch only the devices QEMU gives it. The layers, outermost first:

| Layer | Today | Planned |
| --- | --- | --- |
| OS sandbox around QEMU | **None yet.** QEMU runs as you, with your file access. | seccomp and Landlock on Linux, App Sandbox on macOS, an AppContainer or low-integrity token on Windows (#38). |
| Process lifetime | QEMU ends when oci-hermit does: `-run-with exit-with-parent=on` on Linux and macOS, a kill-on-close Job Object on Windows. | |
| Software CPU | TCG only. The bundle is built without KVM, HVF, WHPX or Xen, and any other QEMU is refused. | |
| Device model | `-nodefaults`: one serial console and one virtio-serial port. No network card, disks, USB, display or sound. | Read-only layer disks (#16), one restricted network card (#26), a file channel for bind mounts (#30). |
| Network | **Sealed.** No network device at all: no way out and no way in. | libslirp with `restrict=on` (#26); `-p` bound to 127.0.0.1 only, even when `0.0.0.0` is asked for (#27); an egress test suite (#28). |
| Control channel | Anonymous pipes, no socket or file. Lines are capped at 64 KiB, anything that is not a message is refused, and every wait has a timeout: the host treats the guest as untrusted. | |
| Image input | Every manifest, config and layer is checked against its sha256 digest when it is pulled, stored and read. Paths that climb out with `..` are refused, device nodes are skipped. | Signature verification with cosign or notation (#42). |
| Inside the guest | A crun container: its own PID, mount, IPC, UTS, network and cgroup namespaces (no user namespace), Docker's default capabilities, masked and read-only kernel paths, a device allow-list, run as the image's `User` (or `-u`). The VM stays the main boundary. | Docker's default seccomp profile (#102). |
| Host code | Rust, with `unsafe` code denied in the host crates; `cargo deny` checks advisories and licences; the QEMU and kernel sources are pinned by sha256. | Code-signed Windows and macOS releases (#39). |

## No network, on purpose

The sandbox has no network device. A command that tries to reach anything fails:

```console
$ oci-hermit run public.ecr.aws/docker/library/alpine:3.20 wget -q -T 5 -O- http://example.com
wget: bad address 'example.com'
```

Pulls happen in the host tool, so the guest never sees a registry, a credential or a route out. An image that
downloads things when it starts fails here, which shows it is not self-contained. Published ports, bound to
`127.0.0.1` only, are planned (#27); outbound access is not planned at all.

## The image store

Pulled and loaded images live in one [OCI image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md)
in your data folder (see [Configuration](configuration.md#where-things-are-kept)). Blobs are shared between images and
written only after their digest checks out. Each image is an entry in the layout's `index.json`, named by its full
reference (`docker.io/library/alpine:latest`) and its platform; an image loaded without a name goes by its image ID.

`pull`, `load` and `run` share the store and can run side by side. `rmi` and `image prune` delete blobs, so they need
the store to themselves and refuse to start while another oci-hermit is reading it.

## The control protocol

One JSON object per line, tagged by `type`. A different major version is refused; a newer minor version only adds
messages or fields.

```json
{"type":"hello","version":[1,2]}
{"type":"run","argv":["sh"],"env":["PATH=/usr/bin:/bin","TERM=xterm-256color"],"cwd":"/","tty":true,"rows":40,"cols":120}
{"type":"output","stream":"stdout","data":"aGkK"}
{"type":"exited","code":0}
```

| Direction | `type` | Since | Meaning |
| --- | --- | --- | --- |
| host to agent | `hello` | 1.0 | First message on the channel, with the host's protocol version. |
| host to agent | `shutdown` | 1.0 | Sync filesystems and power the guest off. |
| host to agent | `run` | 1.1 | Start one command: `argv`, `env`, `cwd`, `tty`, and the terminal's `rows` and `cols`. `user` (uid, gid, groups) since 1.2. One per session. |
| host to agent | `stdin` | 1.1 | Bytes for the command's stdin, base64, at most 16 KiB raw per message. |
| host to agent | `stdin_closed` | 1.1 | The host's stdin reached end of file. |
| agent to host | `ready` | 1.0 | Reply to a compatible `hello`. |
| agent to host | `shutting_down` | 1.0 | Reply to `shutdown`, before the power-off. |
| agent to host | `error` | 1.0 | A refused or failed message. |
| agent to host | `started` | 1.1 | The command started, with its PID. |
| agent to host | `output` | 1.1 | Output, base64, on `stdout` or `stderr`. With a terminal everything is `stdout`. |
| agent to host | `exited` | 1.1 | The exit status, or 128 plus the signal that killed the command. |

The host sends a `user` only to an agent that speaks 1.2 or later, because an older agent would ignore it and run the
command as root. `oci-hermit version` prints the protocol version the host speaks.

## The QEMU command line

| Argument | Why |
| --- | --- |
| `-accel tcg` | Software CPU emulation only. Never KVM, HVF, WHPX or Hyper-V. |
| `-nodefaults -no-user-config -display none -no-reboot` | The smallest device set, no host config files, no display. A guest reboot or panic ends QEMU. |
| `-machine microvm -cpu Haswell-v4,…` | x86_64 guests. The agent's static glibc needs an x86-64-v3 CPU; `qemu64` would stop it with SIGILL. |
| `-machine virt -cpu max` | aarch64 guests. |
| `-m <MiB> -smp <cpus>` | Memory and CPUs: see [Performance and limits](performance.md#memory-and-cpus). |
| `-run-with exit-with-parent=on` | Linux and macOS only. On Windows a Job Object does the same job. |
| `-chardev file,… -serial chardev:console` | The guest console goes to `console.log`, for people; oci-hermit never parses it. |
| `-chardev stdio,…,signal=off` and a `virtserialport` named `org.oci-hermit.control` | The control channel on QEMU's stdin and stdout. `signal=off`, so a byte is never taken as Ctrl-C. |
| `-append "console=ttyS0 panic=-1 tsc_early_khz=1000000"` or `"console=ttyAMA0 panic=-1"` | The kernel command line. The x86_64 guest's clock runs at a fixed 1 GHz on every host, and the kernel is told so. |
| `-drive …,readonly=on` and `-device virtio-blk-device,serial=layerN` per layer | The image's layer disks, read-only, found by the agent by serial number. No network device. |

## What changes later

- **A seccomp profile** (#102), as Docker applies by default.
- **Networking** (#26 to #29) and **bind mounts** (#30 to #34), with the defaults described in
  [oci-hermit, Docker and Podman](docker-and-podman.md).
