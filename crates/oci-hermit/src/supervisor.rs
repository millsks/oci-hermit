//! Launching and supervising one sandbox VM.
//!
//! Each sandbox is one QEMU process, started as the current user with software emulation only and a fixed, minimal
//! device set: no default devices, no user config, no display, no network (#26), the serial console written to a
//! log file, and one virtio-serial port, [`hermit_proto::CONTROL_PORT`], for the control channel. That port is wired
//! to QEMU's own stdin and stdout, which oci-hermit holds as an anonymous pipe pair, so nothing on the filesystem or
//! the network reaches it. QEMU's diagnostics go to its stderr, kept in a log file.
//!
//! QEMU exits when this process exits, whether it stops cleanly or is killed: through QEMU's `exit-with-parent` on
//! Linux and macOS, and through a kill-on-close Job Object on Windows, where QEMU has no such option. A [`Sandbox`]
//! that is dropped kills its QEMU as well.

use crate::qemu::{self, GuestArch};
use hermit_proto::{Frame, Framer, GuestMessage, HostMessage, MAIN, PROTOCOL_VERSION, Process, Stream};
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The x86_64 guest CPU model. The agent's static glibc needs x86-64-v3 (its `init_cpu_features` uses AVX, so
/// `qemu64` dies with SIGILL), and this QEMU has no `x86-64-v3` model: Haswell-v4 is that level with a fixed feature
/// set, unlike `max`. The four features TCG cannot emulate are turned off so QEMU does not warn about them (#54).
pub const X86_64_CPU: &str = "Haswell-v4,pcid=off,tsc-deadline=off,invpcid=off,spec-ctrl=off";
/// The aarch64 guest CPU model.
pub const AARCH64_CPU: &str = "max";

/// The size of a sandbox VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSpec {
    pub arch: GuestArch,
    pub memory_mib: u32,
    pub cpus: u32,
}

/// Everything one QEMU launch needs.
#[derive(Debug)]
pub struct Launch<'a> {
    pub bundle: &'a Path,
    pub kernel: &'a Path,
    pub initrd: &'a Path,
    pub console_log: &'a Path,
    pub spec: MachineSpec,
    /// The image's layer disks, bottom first (#17): attached read-only, as `layer0`, `layer1` and so on.
    pub disks: &'a [PathBuf],
}

/// The most layer disks one sandbox takes. x86_64's microvm has 24 virtio transports and arm64's virt 32; the
/// control port takes one, and the rest leave room for devices to come. An image with more layers runs from the
/// initramfs instead.
pub const MAX_DISKS: usize = 20;

/// The serial number the agent finds disk `index` by.
pub fn disk_serial(index: usize) -> String {
    format!("layer{index}")
}

/// Why a sandbox could not be started, did not come up, or did not stop.
#[derive(Debug, PartialEq, Eq)]
pub enum SupervisorError {
    /// A path QEMU takes inside an option string is not valid UTF-8.
    NonUtf8Path(PathBuf),
    /// Starting, talking to or stopping QEMU failed.
    Io(String),
    /// QEMU exited while oci-hermit was waiting for the agent.
    Exited { status: String },
    /// The agent did not answer in time.
    TimedOut { after: Duration },
    /// The agent answered with an error, or with a protocol version this host does not speak.
    Refused { message: String },
    /// The agent sent something that is not a control message.
    Protocol(String),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupervisorError::NonUtf8Path(path) => write!(f, "path is not valid UTF-8: {}", path.display()),
            SupervisorError::Io(e) => write!(f, "{e}"),
            SupervisorError::Exited { status } => write!(f, "QEMU exited before the agent answered ({status})"),
            SupervisorError::TimedOut { after } => {
                write!(f, "the agent did not answer within {}s", after.as_secs_f64())
            }
            SupervisorError::Refused { message } => write!(f, "the agent refused: {message}"),
            SupervisorError::Protocol(what) => write!(f, "the agent broke the control protocol: {what}"),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<io::Error> for SupervisorError {
    fn from(e: io::Error) -> Self {
        SupervisorError::Io(e.to_string())
    }
}

/// A value inside a QEMU option string, where a comma separates options and `,,` is a literal comma.
fn option_value(path: &Path) -> Result<String, SupervisorError> {
    let text = path
        .to_str()
        .ok_or_else(|| SupervisorError::NonUtf8Path(path.to_path_buf()))?;
    Ok(text.replace(',', ",,"))
}

/// The kernel command line: the machine's serial console, and on a panic, exit at once (QEMU runs `-no-reboot`).
///
/// An x86_64 guest is also told its TSC frequency. The bundled QEMU's TSC counts nanoseconds of virtual time on
/// every host (`qemu/patches/0002-…`), so it runs at exactly 1 GHz. Calibrating it against the PIT instead is slow
/// under emulation and fails on loaded hosts, and microvm has no HPET or ACPI PM timer to fall back on, so the guest
/// hung without a clock (#61).
pub fn kernel_cmdline(arch: GuestArch) -> &'static str {
    match arch {
        GuestArch::X86_64 => "console=ttyS0 panic=-1 tsc_early_khz=1000000",
        GuestArch::Aarch64 => "console=ttyAMA0 panic=-1",
    }
}

/// The QEMU binary and arguments for one launch.
pub fn qemu_command(launch: &Launch<'_>) -> Result<(PathBuf, Vec<OsString>), SupervisorError> {
    let arch = launch.spec.arch;
    let binary = launch.bundle.join("bin").join(arch.binary_name());
    let (machine, cpu) = match arch {
        GuestArch::X86_64 => ("microvm", X86_64_CPU),
        GuestArch::Aarch64 => ("virt", AARCH64_CPU),
    };
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(OsString::from));
    push(&["-nodefaults", "-no-user-config", "-display", "none", "-no-reboot"]);
    push(&["-accel", "tcg", "-machine", machine, "-cpu", cpu]);
    push(&[
        "-m",
        &launch.spec.memory_mib.to_string(),
        "-smp",
        &launch.spec.cpus.to_string(),
    ]);
    // QEMU accepts -run-with only on POSIX hosts; on Windows the Job Object in `Sandbox::spawn` does this.
    if cfg!(unix) {
        push(&["-run-with", "exit-with-parent=on"]);
    }
    let console = format!("file,id=console,path={}", option_value(launch.console_log)?);
    push(&["-chardev", &console, "-serial", "chardev:console"]);
    // The control channel: a virtio-serial port on QEMU's stdin and stdout. `signal=off` keeps QEMU from treating
    // a byte on that pipe as Ctrl-C.
    let port = format!("virtserialport,chardev=control,name={}", hermit_proto::CONTROL_PORT);
    push(&["-chardev", "stdio,id=control,signal=off"]);
    push(&["-device", "virtio-serial-device", "-device", &port]);
    // Layer disks, read-only twice over: QEMU refuses writes, and EROFS has none to make.
    for (i, disk) in launch.disks.iter().take(MAX_DISKS).enumerate() {
        let serial = disk_serial(i);
        let drive = format!(
            "file={},if=none,id={serial},format=raw,readonly=on",
            option_value(disk)?
        );
        push(&["-drive", &drive]);
        push(&["-device", &format!("virtio-blk-device,drive={serial},serial={serial}")]);
    }
    push(&["-append", kernel_cmdline(arch)]);
    args.extend([
        OsString::from("-L"),
        qemu::firmware_dir(launch.bundle).into_os_string(),
        OsString::from("-kernel"),
        launch.kernel.as_os_str().to_owned(),
        OsString::from("-initrd"),
        launch.initrd.as_os_str().to_owned(),
    ]);
    Ok((binary, args))
}

/// A fresh directory for one sandbox's files: its initramfs, console log and QEMU's own error output.
pub fn run_dir(base: &Path) -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = base.join(format!("sandbox-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A command to run in the guest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Command {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub tty: bool,
    /// The host terminal's `(rows, cols)`, for a command with a terminal.
    pub size: Option<(u16, u16)>,
    /// Who to run the command as; root when `None`.
    pub user: Option<hermit_proto::User>,
    /// The serial numbers of the image's layer disks, bottom first; empty when the image is in the initramfs.
    pub layers: Vec<String>,
    /// The container's hostname; the agent picks one when `None`.
    pub hostname: Option<String>,
    /// The container's CPU and memory limits (protocol 1.7); none by default.
    #[serde(default, skip_serializing_if = "hermit_proto::Limits::is_none")]
    pub limits: hermit_proto::Limits,
}

/// What [`Sandbox::relay`] takes while the container runs: bytes for a command's stdin, the end of that, another
/// command to start in the container, or a request to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Data(Process, Vec<u8>),
    Eof(Process),
    /// Start `Command` in the running container as this process (protocol 1.5); its `layers` and `hostname` are
    /// the container's and are not sent.
    Exec(Process, Command),
    /// Send a signal to a command (protocol 1.6); an older agent cannot take one, and it is dropped.
    Signal(Process, i32),
    /// Copy what this path names out of the container (protocol 1.8), as [`Relayed::Entry`]s.
    CopyOut(Process, String),
    /// Copy into the container (protocol 1.8); the entries follow as `Put`, `PutData` and `PutDone`.
    CopyIn(Process, CopyInto),
    Put(Process, hermit_proto::Entry),
    PutData(Process, Vec<u8>),
    PutDone(Process),
    /// Stop waiting for the command: [`Sandbox::relay`] returns `None` and the caller stops the sandbox.
    Stop,
}

/// Where a copy into the container goes: see [`HostMessage::CopyIn`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyInto {
    pub path: String,
    pub name: String,
    pub dir: bool,
    pub contents: bool,
}

/// What [`Sandbox::relay`] reports while the container runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relayed<'a> {
    Output {
        process: Process,
        stream: Stream,
        bytes: &'a [u8],
    },
    /// A command from [`Input::Exec`] started.
    Started {
        process: Process,
    },
    /// A command from [`Input::Exec`] ended; the container's own command ends the relay instead.
    Exited {
        process: Process,
        code: i32,
    },
    /// A command from [`Input::Exec`] could not be started, or a copy failed.
    Failed {
        process: Process,
        message: String,
    },
    /// One entry of a copy out of the container, and a file's bytes after it.
    Entry {
        process: Process,
        entry: hermit_proto::Entry,
    },
    EntryData {
        process: Process,
        bytes: &'a [u8],
    },
}

/// What the control reader thread hands over: a frame from the agent, or `None` once QEMU's stdout closed.
type Incoming = Option<Frame>;

/// One running QEMU process and the control channel to its agent.
#[derive(Debug)]
pub struct Sandbox {
    child: Option<Child>,
    to_agent: Option<ChildStdin>,
    from_agent: Receiver<Incoming>,
    /// The protocol the agent speaks, once it answered.
    version: (u16, u16),
    /// Windows closes this job's handle when oci-hermit exits, however it exits, and that kills QEMU.
    #[cfg(windows)]
    _job: win32job::Job,
}

/// Read QEMU's stdout, which carries the agent's side of the control channel, and pass on whole frames.
fn read_frames(mut from_qemu: impl Read, frames: mpsc::Sender<Incoming>) {
    let mut framer = Framer::new();
    let mut buf = [0u8; 4096];
    let mut cut = Vec::new();
    loop {
        match from_qemu.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                framer.push(&buf[..n], &mut cut);
                for frame in cut.drain(..) {
                    if frames.send(Some(frame)).is_err() {
                        return;
                    }
                }
            }
        }
    }
    let _ = frames.send(None);
}

impl Sandbox {
    /// Start `binary` with `args`. QEMU's stdin and stdout become the control channel; its stderr goes to
    /// `qemu_log`.
    pub fn spawn(binary: &Path, args: &[OsString], qemu_log: &Path) -> io::Result<Sandbox> {
        let log = File::create(qemu_log)?;
        let mut child = std::process::Command::new(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()?;
        #[cfg(windows)]
        let job = match kill_on_close(&child) {
            Ok(job) => job,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let to_agent = child.stdin.take();
        let from_qemu = child.stdout.take().expect("stdout is piped");
        let (frames, from_agent) = mpsc::channel();
        std::thread::spawn(move || read_frames(from_qemu, frames));
        Ok(Sandbox {
            child: Some(child),
            to_agent,
            from_agent,
            version: (0, 0),
            #[cfg(windows)]
            _job: job,
        })
    }

    /// Send one message to the agent.
    fn send(&mut self, msg: &HostMessage) -> Result<(), SupervisorError> {
        let line = hermit_proto::encode(msg).map_err(|e| SupervisorError::Io(e.to_string()))?;
        let pipe = self
            .to_agent
            .as_mut()
            .ok_or_else(|| SupervisorError::Io("the control channel is closed".into()))?;
        if let Err(e) = pipe.write_all(line.as_bytes()).and_then(|()| pipe.flush()) {
            // A broken pipe usually means QEMU is exiting; give it a moment so its exit is what gets reported.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(exited) = self.exit_status()? {
                    return Err(exited);
                }
                if Instant::now() >= deadline {
                    return Err(SupervisorError::Io(e.to_string()));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }

    /// QEMU's exit, as an error, if it has exited.
    fn exit_status(&mut self) -> Result<Option<SupervisorError>, SupervisorError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(Some(SupervisorError::Exited {
                status: "already stopped".into(),
            }));
        };
        Ok(child.try_wait()?.map(|status| SupervisorError::Exited {
            status: status.to_string(),
        }))
    }

    /// The agent's next message, waiting until `deadline` at most.
    fn receive(&mut self, deadline: Instant, timeout: Duration) -> Result<GuestMessage, SupervisorError> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.from_agent.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(Some(Frame::Line(line))) => {
                    return hermit_proto::decode(&line)
                        .map_err(|e| SupervisorError::Protocol(format!("not a control message ({e})")));
                }
                Ok(Some(Frame::TooLong)) => {
                    return Err(SupervisorError::Protocol(format!(
                        "a line longer than {} bytes",
                        hermit_proto::MAX_LINE
                    )));
                }
                Ok(Some(Frame::NotUtf8)) => return Err(SupervisorError::Protocol("a line that is not UTF-8".into())),
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    // The channel closed: QEMU is exiting. Give it a moment to be reaped.
                    let child = self
                        .child
                        .as_mut()
                        .ok_or_else(|| SupervisorError::Io("no QEMU".into()))?;
                    let status = loop {
                        if let Some(status) = child.try_wait()? {
                            break status.to_string();
                        }
                        if Instant::now() >= deadline {
                            break "its stdout closed".into();
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    };
                    return Err(SupervisorError::Exited { status });
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(exited) = self.exit_status()? {
                        return Err(exited);
                    }
                    if Instant::now() >= deadline {
                        return Err(SupervisorError::TimedOut { after: timeout });
                    }
                }
            }
        }
    }

    /// Open a session with the agent: send `Hello` and wait for `Ready`. Returns how long that took and the
    /// agent's protocol version.
    pub fn handshake(&mut self, timeout: Duration) -> Result<(Duration, (u16, u16)), SupervisorError> {
        let start = Instant::now();
        let deadline = start + timeout;
        self.send(&HostMessage::Hello {
            version: PROTOCOL_VERSION,
        })?;
        match self.receive(deadline, timeout)? {
            GuestMessage::Ready { version } if hermit_proto::compatible(version) => {
                self.version = version;
                Ok((start.elapsed(), version))
            }
            GuestMessage::Ready { version } => Err(SupervisorError::Refused {
                message: format!(
                    "it speaks control protocol {}.{}; this host speaks {}.{}",
                    version.0, version.1, PROTOCOL_VERSION.0, PROTOCOL_VERSION.1
                ),
            }),
            GuestMessage::Error { message, .. } => Err(SupervisorError::Refused { message }),
            other => Err(SupervisorError::Protocol(format!("{other:?} instead of ready"))),
        }
    }

    /// Start one command in the guest, waiting up to `start_timeout` for the agent to say it started.
    pub fn start(&mut self, command: &Command, start_timeout: Duration) -> Result<(), SupervisorError> {
        // An older agent would ignore the limits and run the container without them.
        if !command.limits.is_none() && self.version < (1, 7) {
            return Err(SupervisorError::Refused {
                message: format!(
                    "this guest agent cannot limit a container's CPUs or memory (it speaks control protocol {}.{}, \
                     and that needs 1.7); run without --cpus and --memory, or update oci-hermit's guest files",
                    self.version.0, self.version.1
                ),
            });
        }
        self.send(&HostMessage::Run {
            argv: command.argv.clone(),
            env: command.env.clone(),
            cwd: command.cwd.clone(),
            tty: command.tty,
            rows: command.size.map(|s| s.0),
            cols: command.size.map(|s| s.1),
            user: command.user.clone(),
            layers: command.layers.clone(),
            hostname: command.hostname.clone(),
            limits: command.limits,
        })?;
        let deadline = Instant::now() + start_timeout;
        match self.receive(deadline, start_timeout)? {
            GuestMessage::Started { .. } => Ok(()),
            GuestMessage::Error { message, .. } => Err(SupervisorError::Refused { message }),
            other => Err(SupervisorError::Protocol(format!("{other:?} instead of started"))),
        }
    }

    /// Relay the container [`Sandbox::start`] started until its command ends: from `input` its stdin when
    /// `interactive` (otherwise stdin is closed at once), more commands to start in it and any request to stop, and
    /// to `on` what they all write and how the others went. Returns the command's exit code, or `None` when asked to
    /// stop first. There is no overall timeout: an interactive shell may sit idle. A QEMU that exits ends the
    /// session with an error.
    pub fn relay(
        &mut self,
        input: &Receiver<Input>,
        interactive: bool,
        on: &mut dyn FnMut(Relayed<'_>) -> io::Result<()>,
    ) -> Result<Option<i32>, SupervisorError> {
        if !interactive {
            self.send(&HostMessage::StdinClosed { process: MAIN })?;
        }
        // The processes whose stdin is still open; an exec's opens with it.
        let mut stdin_open: std::collections::BTreeSet<Process> = std::collections::BTreeSet::new();
        if interactive {
            stdin_open.insert(MAIN);
        }
        loop {
            while let Ok(next) = input.try_recv() {
                match next {
                    Input::Stop => return Ok(None),
                    Input::Data(process, bytes) if stdin_open.contains(&process) => {
                        for piece in bytes.chunks(hermit_proto::CHUNK) {
                            self.send(&HostMessage::Stdin {
                                process,
                                data: hermit_proto::b64::encode(piece),
                            })?;
                        }
                    }
                    Input::Eof(process) if stdin_open.remove(&process) => {
                        self.send(&HostMessage::StdinClosed { process })?;
                    }
                    Input::Data(..) | Input::Eof(_) => {}
                    Input::Signal(process, signal) if self.version >= (1, 6) => {
                        self.send(&HostMessage::Signal { process, signal })?;
                    }
                    Input::Signal(..) => {}
                    Input::Exec(process, _) if process == MAIN => on(Relayed::Failed {
                        process,
                        message: "process 0 is the container's own command".into(),
                    })?,
                    Input::Exec(process, _) if self.version < (1, 5) => on(Relayed::Failed {
                        process,
                        message: format!(
                            "the guest agent speaks control protocol {}.{}, which has no exec; it needs 1.5 or later \
                             (use the guest that came with this oci-hermit)",
                            self.version.0, self.version.1
                        ),
                    })?,
                    Input::CopyOut(process, _) | Input::CopyIn(process, _) if self.version < (1, 8) => {
                        on(Relayed::Failed {
                            process,
                            message: format!(
                                "the guest agent speaks control protocol {}.{}, which has no cp; it needs 1.8 or later \
                                 (use the guest that came with this oci-hermit)",
                                self.version.0, self.version.1
                            ),
                        })?;
                    }
                    Input::CopyOut(process, path) => self.send(&HostMessage::CopyOut { process, path })?,
                    Input::CopyIn(process, into) => self.send(&HostMessage::CopyIn {
                        process,
                        path: into.path,
                        name: into.name,
                        dir: into.dir,
                        contents: into.contents,
                    })?,
                    Input::Put(process, entry) => self.send(&HostMessage::Put { process, entry })?,
                    Input::PutData(process, bytes) => {
                        for piece in bytes.chunks(hermit_proto::CHUNK) {
                            self.send(&HostMessage::PutData {
                                process,
                                data: hermit_proto::b64::encode(piece),
                            })?;
                        }
                    }
                    Input::PutDone(process) => self.send(&HostMessage::PutDone { process })?,
                    Input::Exec(process, command) => {
                        self.send(&HostMessage::Exec {
                            process,
                            argv: command.argv,
                            env: command.env,
                            cwd: command.cwd,
                            tty: command.tty,
                            rows: command.size.map(|s| s.0),
                            cols: command.size.map(|s| s.1),
                            user: command.user,
                        })?;
                        stdin_open.insert(process);
                    }
                }
            }
            let message = match self.from_agent.recv_timeout(Duration::from_millis(20)) {
                Ok(Some(Frame::Line(line))) => hermit_proto::decode::<GuestMessage>(&line)
                    .map_err(|e| SupervisorError::Protocol(format!("not a control message ({e})")))?,
                Ok(Some(Frame::TooLong)) => {
                    return Err(SupervisorError::Protocol(format!(
                        "a line longer than {} bytes",
                        hermit_proto::MAX_LINE
                    )));
                }
                Ok(Some(Frame::NotUtf8)) => return Err(SupervisorError::Protocol("a line that is not UTF-8".into())),
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    let now = Instant::now();
                    return Err(self
                        .receive(now, Duration::ZERO)
                        .err()
                        .unwrap_or(SupervisorError::Exited {
                            status: "the control channel closed".into(),
                        }));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(exited) = self.exit_status()? {
                        return Err(exited);
                    }
                    continue;
                }
            };
            match message {
                GuestMessage::Output { process, stream, data } => {
                    let bytes = hermit_proto::b64::decode(&data)
                        .ok_or_else(|| SupervisorError::Protocol("output that is not base64".into()))?;
                    on(Relayed::Output {
                        process,
                        stream,
                        bytes: &bytes,
                    })?;
                }
                GuestMessage::Exited { process: MAIN, code } => return Ok(Some(code)),
                GuestMessage::Exited { process, code } => {
                    stdin_open.remove(&process);
                    on(Relayed::Exited { process, code })?;
                }
                GuestMessage::Started { process, .. } if process != MAIN => on(Relayed::Started { process })?,
                GuestMessage::Entry { process, entry } => on(Relayed::Entry { process, entry })?,
                GuestMessage::EntryData { process, data } => {
                    let bytes = hermit_proto::b64::decode(&data)
                        .ok_or_else(|| SupervisorError::Protocol("copied data that is not base64".into()))?;
                    on(Relayed::EntryData { process, bytes: &bytes })?;
                }
                GuestMessage::Error { process: MAIN, message } => return Err(SupervisorError::Refused { message }),
                GuestMessage::Error { process, message } => {
                    stdin_open.remove(&process);
                    on(Relayed::Failed { process, message })?;
                }
                other => return Err(SupervisorError::Protocol(format!("{other:?} during a run"))),
            }
        }
    }

    /// Ask the agent to power the guest off, and wait for QEMU to exit. If it does not exit in time, QEMU is
    /// killed and the timeout is reported.
    pub fn shutdown(&mut self, timeout: Duration) -> Result<(), SupervisorError> {
        let deadline = Instant::now() + timeout;
        self.send(&HostMessage::Shutdown)?;
        match self.receive(deadline, timeout) {
            Ok(GuestMessage::ShuttingDown) | Err(SupervisorError::Exited { .. }) => {}
            Ok(GuestMessage::Error { message, .. }) => return Err(SupervisorError::Refused { message }),
            Ok(other) => return Err(SupervisorError::Protocol(format!("{other:?} instead of shutting down"))),
            Err(e) => {
                self.stop()?;
                return Err(e);
            }
        }
        while self.exit_status()?.is_none() {
            if Instant::now() >= deadline {
                self.stop()?;
                return Err(SupervisorError::TimedOut { after: timeout });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.child = None;
        Ok(())
    }

    /// Kill QEMU and wait for it to exit. Stopping a sandbox that already stopped does nothing.
    pub fn stop(&mut self) -> io::Result<()> {
        self.to_agent = None;
        if let Some(mut child) = self.child.take()
            && child.try_wait()?.is_none()
        {
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }
}

/// A Job Object holding `child` that Windows kills, with everything in it, once its last handle closes.
#[cfg(windows)]
fn kill_on_close(child: &Child) -> io::Result<win32job::Job> {
    use std::os::windows::io::AsRawHandle;
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info).map_err(io::Error::other)?;
    job.assign_process(child.as_raw_handle() as isize)
        .map_err(io::Error::other)?;
    Ok(job)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    fn args_of(arch: GuestArch, console: &str) -> (PathBuf, Vec<String>) {
        let launch = Launch {
            bundle: Path::new("/b"),
            kernel: Path::new("/g/kernel"),
            initrd: Path::new("/r/initrd.cpio"),
            console_log: Path::new(console),
            spec: MachineSpec {
                arch,
                memory_mib: 256,
                cpus: 2,
            },
            disks: &[],
        };
        let (binary, args) = qemu_command(&launch).unwrap();
        (binary, args.into_iter().map(|a| a.into_string().unwrap()).collect())
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag}"));
        &args[i + 1]
    }

    #[test]
    fn launches_software_emulation_only() {
        for arch in GuestArch::ALL {
            let (_, args) = args_of(arch, "/r/console.log");
            assert_eq!(value_after(&args, "-accel"), "tcg");
            assert_eq!(args.iter().filter(|a| *a == "-accel").count(), 1);
            let joined = args.join(" ");
            for accelerator in ["kvm", "hvf", "whpx", "xen", "nvmm"] {
                assert!(!joined.contains(accelerator), "{accelerator} in {joined}");
            }
        }
    }

    #[test]
    fn has_no_default_devices_display_or_network() {
        let (_, args) = args_of(GuestArch::X86_64, "/r/console.log");
        for flag in ["-nodefaults", "-no-user-config", "-no-reboot"] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}");
        }
        assert_eq!(value_after(&args, "-display"), "none");
        for flag in ["-netdev", "-nic", "-net", "-drive", "-usb"] {
            assert!(!args.iter().any(|a| a == flag), "unexpected {flag}");
        }
        // The only devices are the control channel's (`wires_the_control_port_to_qemus_stdio`).
        let devices = args.iter().filter(|a| *a == "-device").count();
        assert_eq!(devices, 2);
    }

    #[test]
    fn attaches_layer_disks_read_only_and_no_more_than_fit() {
        let disks: Vec<PathBuf> = (0..MAX_DISKS + 3)
            .map(|i| PathBuf::from(format!("/c/a,b/{i}.erofs")))
            .collect();
        let launch = Launch {
            bundle: Path::new("/b"),
            kernel: Path::new("/g/kernel"),
            initrd: Path::new("/r/initrd.cpio"),
            console_log: Path::new("/r/console.log"),
            spec: MachineSpec {
                arch: GuestArch::X86_64,
                memory_mib: 512,
                cpus: 1,
            },
            disks: &disks[..2],
        };
        let args: Vec<String> = qemu_command(&launch)
            .unwrap()
            .1
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let drives: Vec<&String> = args
            .iter()
            .zip(&args[1..])
            .filter(|(f, _)| *f == "-drive")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            drives,
            [
                "file=/c/a,,b/0.erofs,if=none,id=layer0,format=raw,readonly=on",
                "file=/c/a,,b/1.erofs,if=none,id=layer1,format=raw,readonly=on"
            ]
        );
        assert!(args.contains(&"virtio-blk-device,drive=layer1,serial=layer1".to_string()));
        let all = Launch {
            disks: &disks,
            ..launch
        };
        let count = qemu_command(&all).unwrap().1.iter().filter(|a| *a == "-drive").count();
        assert_eq!(count, MAX_DISKS);
        assert_eq!(disk_serial(3), "layer3");
    }

    #[test]
    fn exits_with_the_host_tool() {
        let (_, args) = args_of(GuestArch::Aarch64, "/r/console.log");
        if cfg!(unix) {
            assert_eq!(value_after(&args, "-run-with"), "exit-with-parent=on");
        } else {
            assert!(
                !args.iter().any(|a| a == "-run-with"),
                "QEMU rejects -run-with on Windows"
            );
        }
    }

    #[test]
    fn writes_the_console_to_the_log() {
        let (_, args) = args_of(GuestArch::Aarch64, "/r/console.log");
        assert_eq!(value_after(&args, "-chardev"), "file,id=console,path=/r/console.log");
        assert_eq!(value_after(&args, "-serial"), "chardev:console");
        assert_eq!(value_after(&args, "-append"), "console=ttyAMA0 panic=-1");
    }

    #[test]
    fn escapes_commas_in_the_console_path() {
        let (_, args) = args_of(GuestArch::X86_64, "/r/a,b/console.log");
        assert_eq!(
            value_after(&args, "-chardev"),
            "file,id=console,path=/r/a,,b/console.log"
        );
    }

    #[test]
    fn picks_the_machine_cpu_and_emulator_per_arch() {
        let (binary, args) = args_of(GuestArch::X86_64, "/r/c");
        assert!(binary.ends_with(GuestArch::X86_64.binary_name()));
        assert_eq!(value_after(&args, "-machine"), "microvm");
        assert_eq!(value_after(&args, "-cpu"), X86_64_CPU);
        assert_eq!(
            value_after(&args, "-append"),
            "console=ttyS0 panic=-1 tsc_early_khz=1000000"
        );
        assert_eq!(Path::new(value_after(&args, "-L")), qemu::firmware_dir(Path::new("/b")));
        let (binary, args) = args_of(GuestArch::Aarch64, "/r/c");
        assert!(binary.ends_with(GuestArch::Aarch64.binary_name()));
        assert_eq!(value_after(&args, "-machine"), "virt");
        assert_eq!(value_after(&args, "-cpu"), AARCH64_CPU);
        assert_eq!(value_after(&args, "-m"), "256");
        assert_eq!(value_after(&args, "-smp"), "2");
        assert_eq!(value_after(&args, "-kernel"), "/g/kernel");
        assert_eq!(value_after(&args, "-initrd"), "/r/initrd.cpio");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_utf8_console_path() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/r/\xff"));
        let err = option_value(path).unwrap_err();
        assert!(matches!(err, SupervisorError::NonUtf8Path(_)));
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn run_dirs_are_fresh() {
        let base = scratch("run-dirs");
        let a = run_dir(&base).unwrap();
        let b = run_dir(&base).unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn errors_say_what_happened() {
        let cases = [
            (
                SupervisorError::Exited {
                    status: "exit status: 1".into(),
                },
                "QEMU exited before the agent answered (exit status: 1)",
            ),
            (
                SupervisorError::TimedOut {
                    after: Duration::from_millis(1500),
                },
                "the agent did not answer within 1.5s",
            ),
            (
                SupervisorError::Refused { message: "no".into() },
                "the agent refused: no",
            ),
            (
                SupervisorError::Protocol("noise".into()),
                "the agent broke the control protocol: noise",
            ),
            (io::Error::other("denied").into(), "denied"),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    #[test]
    fn wires_the_control_port_to_qemus_stdio() {
        let (_, args) = args_of(GuestArch::X86_64, "/r/c");
        let chardevs: Vec<_> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-chardev")
            .map(|(_, value)| value.as_str())
            .collect();
        assert!(chardevs.contains(&"stdio,id=control,signal=off"), "{chardevs:?}");
        let devices: Vec<_> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-device")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(
            devices,
            [
                "virtio-serial-device",
                "virtserialport,chardev=control,name=org.oci-hermit.control"
            ]
        );
    }

    /// What happens when oci-hermit dies on Windows: its handle to the job closes, and Windows kills QEMU.
    #[cfg(windows)]
    #[test]
    fn closing_the_job_kills_the_process() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let job = kill_on_close(&child).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "still running while the job is open"
        );
        drop(job);
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "closing the job did not kill the process"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use crate::qemu::testing::exec_lock;
        use std::os::unix::fs::PermissionsExt;

        const READY: &str = r#"echo '{"type":"ready","version":[1,0]}'"#;
        const SHUTTING_DOWN: &str = r#"echo '{"type":"shutting_down"}'"#;

        /// A shell script standing in for QEMU, speaking the agent's side of the control channel on its stdio.
        fn spawn(name: &str, body: &str) -> (Sandbox, PathBuf) {
            let dir = scratch(name);
            let binary = dir.join("fake-qemu");
            std::fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let sandbox = Sandbox::spawn(&binary, &[], &dir.join("qemu.log")).unwrap();
            (sandbox, dir)
        }

        #[test]
        fn handshakes_then_shuts_down_cleanly() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn(
                "sup-ok",
                &format!("read hello; {READY}; read bye; {SHUTTING_DOWN}; exit 0"),
            );
            let (_, version) = sandbox.handshake(Duration::from_secs(10)).unwrap();
            assert_eq!(version, (1, 0));
            sandbox.shutdown(Duration::from_secs(10)).unwrap();
            sandbox.stop().unwrap();
        }

        #[test]
        fn sends_hello_with_this_hosts_version() {
            let _guard = exec_lock();
            let (mut sandbox, dir) = spawn(
                "sup-hello",
                &format!("cd \"$(dirname \"$0\")\"; read hello; echo \"$hello\" > hello.seen; {READY}; sleep 30"),
            );
            sandbox.handshake(Duration::from_secs(10)).unwrap();
            let seen = std::fs::read_to_string(dir.join("hello.seen")).unwrap();
            assert_eq!(
                hermit_proto::decode::<HostMessage>(&seen).unwrap(),
                HostMessage::Hello {
                    version: PROTOCOL_VERSION
                }
            );
        }

        #[test]
        fn reports_an_agent_that_refuses() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn(
                "sup-refuse",
                r#"read hello; echo '{"type":"error","message":"nope"}'; sleep 30"#,
            );
            let err = sandbox.handshake(Duration::from_secs(10)).unwrap_err();
            assert_eq!(err, SupervisorError::Refused { message: "nope".into() });
        }

        #[test]
        fn refuses_an_agent_of_another_major_version() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn(
                "sup-major",
                r#"read hello; echo '{"type":"ready","version":[9,0]}'; sleep 30"#,
            );
            let err = sandbox.handshake(Duration::from_secs(10)).unwrap_err();
            assert!(err.to_string().contains("control protocol 9.0"), "{err}");
        }

        #[test]
        fn rejects_garbage_from_the_guest() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn("sup-garbage", "read hello; echo 'rm -rf /'; sleep 30");
            let err = sandbox.handshake(Duration::from_secs(10)).unwrap_err();
            assert!(matches!(err, SupervisorError::Protocol(_)), "{err}");
        }

        #[test]
        fn rejects_an_endless_line_from_the_guest() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn(
                "sup-endless",
                "read hello; head -c 200000 /dev/zero | tr '\\0' x; echo; sleep 30",
            );
            let err = sandbox.handshake(Duration::from_secs(10)).unwrap_err();
            assert!(err.to_string().contains("longer than"), "{err}");
        }

        #[test]
        fn reports_qemu_exiting_first() {
            let _guard = exec_lock();
            let (mut sandbox, dir) = spawn("sup-exit", "echo 'bad option' >&2; exit 1");
            let err = sandbox.handshake(Duration::from_secs(10)).unwrap_err();
            assert!(matches!(err, SupervisorError::Exited { .. }), "{err}");
            let qemu_log = std::fs::read_to_string(dir.join("qemu.log")).unwrap();
            assert!(qemu_log.contains("bad option"));
        }

        #[test]
        fn times_out_and_drop_kills_qemu() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn("sup-timeout", "sleep 30");
            let err = sandbox.handshake(Duration::from_millis(300)).unwrap_err();
            assert!(matches!(err, SupervisorError::TimedOut { .. }));
            let start = Instant::now();
            drop(sandbox);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "drop kills QEMU instead of waiting for it"
            );
        }

        #[test]
        fn kills_a_guest_that_does_not_power_off() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn(
                "sup-linger",
                &format!("read hello; {READY}; read bye; {SHUTTING_DOWN}; sleep 30"),
            );
            sandbox.handshake(Duration::from_secs(10)).unwrap();
            let start = Instant::now();
            let err = sandbox.shutdown(Duration::from_millis(500)).unwrap_err();
            assert!(matches!(err, SupervisorError::TimedOut { .. }), "{err}");
            assert!(start.elapsed() < Duration::from_secs(5));
            assert!(sandbox.send(&HostMessage::Shutdown).is_err(), "QEMU was stopped");
        }
    }
}
