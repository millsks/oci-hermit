//! The real system calls, for the Linux guest.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::path::Path;

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use hermit_proto::{CONTROL_PORT, Framer, Stream};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::mount::MsFlags;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};
use nix::spawn::{PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags, posix_spawn};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::signal::{SigSet, Signal as NixSignal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, chdir, chroot, pipe2, setsid, tcsetpgrp, ttyname};

use crate::init::{self, Event, Mount, Reaped, RunSpec, Signal, System};

/// Where `oci-hermit run` puts the image's root filesystem in the initramfs.
const ROOTFS: &str = "/rootfs";

/// `PATH` when the image's environment has none, as Docker uses.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The filesystems a command expects, mounted inside the image root before it starts.
const ROOTFS_MOUNTS: [Mount; 4] = [
    Mount {
        source: "proc",
        target: "/rootfs/proc",
        fstype: "proc",
        nosuid: true,
        nodev: true,
        noexec: true,
        data: None,
    },
    Mount {
        source: "sysfs",
        target: "/rootfs/sys",
        fstype: "sysfs",
        nosuid: true,
        nodev: true,
        noexec: true,
        data: None,
    },
    Mount {
        source: "devtmpfs",
        target: "/rootfs/dev",
        fstype: "devtmpfs",
        nosuid: true,
        nodev: false,
        noexec: false,
        data: Some("mode=0755"),
    },
    Mount {
        source: "devpts",
        target: "/rootfs/dev/pts",
        fstype: "devpts",
        nosuid: true,
        nodev: false,
        noexec: true,
        data: Some("mode=0620,ptmxmode=0666"),
    },
];

/// The command a `Run` started: where its input goes and where its output comes from.
struct Running {
    tty: bool,
    stdin: Option<File>,
    outputs: Vec<(Stream, File)>,
    /// The terminal side of the pty, kept open so the master never reports a hangup mid-run.
    _terminal: Option<OwnedFd>,
}

fn context(what: &str) -> impl Fn(Errno) -> io::Error + '_ {
    move |e| io::Error::new(io::Error::from(e).kind(), format!("{what}: {e}"))
}

/// `argv0` looked up in the `PATH` from `env`, inside the current root.
fn resolve(argv0: &str, env: &[String]) -> io::Result<PathBuf> {
    if argv0.contains('/') {
        return Ok(PathBuf::from(argv0));
    }
    let path = env
        .iter()
        .rev()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or(DEFAULT_PATH);
    for dir in path.split(':') {
        let candidate = Path::new(if dir.is_empty() { "." } else { dir }).join(argv0);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        "executable file not found in $PATH",
    ))
}

fn cstrings(items: &[String]) -> io::Result<Vec<CString>> {
    items
        .iter()
        .map(|s| {
            CString::new(s.as_str()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "a NUL byte in the command"))
        })
        .collect()
}

/// Where the kernel lists virtio-serial ports, each with a `name` file.
const VIRTIO_PORTS: &str = "/sys/class/virtio-ports";

/// Whether this process is PID 1.
pub fn is_pid1() -> bool {
    nix::unistd::getpid().as_raw() == 1
}

/// Linux implementation of [`System`]: the blocked signals arrive on a signalfd, and the control port is polled
/// alongside it.
#[derive(Default)]
pub struct Linux {
    signals: Option<SignalFd>,
    control: Option<File>,
    framer: Framer,
    pending: VecDeque<Event>,
    run: Option<Running>,
}

impl Linux {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the command's outputs that poll found ready; one that ends (EOF, or EIO from a terminal) is dropped.
    fn read_outputs(&mut self, ready: &[bool]) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let mut ended = Vec::new();
        for (i, (stream, file)) in run.outputs.iter_mut().enumerate() {
            if !ready.get(i).copied().unwrap_or(false) {
                continue;
            }
            match file.read(&mut buf) {
                Ok(0) => ended.push(i),
                Ok(n) => self.pending.push_back(Event::Output(*stream, buf[..n].to_vec())),
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Err(_) => ended.push(i),
            }
        }
        for i in ended.into_iter().rev() {
            run.outputs.remove(i);
        }
    }

    /// Read what the control port has, turning it into events.
    fn read_control(&mut self) {
        let Some(port) = self.control.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        match port.read(&mut buf) {
            Ok(0) => {
                self.control = None;
                self.pending.push_back(Event::ControlClosed);
            }
            Ok(n) => {
                let mut frames = Vec::new();
                self.framer.push(&buf[..n], &mut frames);
                self.pending.extend(frames.into_iter().map(Event::Control));
            }
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
            Err(_) => {
                self.control = None;
                self.pending.push_back(Event::ControlClosed);
            }
        }
    }
}

/// The device node of the virtio-serial port called `name`, found through sysfs.
fn find_port(ports: &Path, devices: &Path, name: &str) -> io::Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(ports)? {
        let entry = entry?;
        let port_name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if port_name.trim_end() == name {
            return Ok(devices.join(entry.file_name()));
        }
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        format!("no virtio-serial port named {name}"),
    ))
}

fn shutdown_signals() -> SigSet {
    let mut set = SigSet::empty();
    for sig in [NixSignal::SIGTERM, NixSignal::SIGINT, NixSignal::SIGCHLD] {
        set.add(sig);
    }
    set
}

impl System for Linux {
    fn mkdir_all(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn mount(&mut self, m: &Mount) -> io::Result<()> {
        let mut flags = MsFlags::empty();
        flags.set(MsFlags::MS_NOSUID, m.nosuid);
        flags.set(MsFlags::MS_NODEV, m.nodev);
        flags.set(MsFlags::MS_NOEXEC, m.noexec);
        nix::mount::mount(Some(m.source), m.target, Some(m.fstype), flags, m.data)?;
        Ok(())
    }

    fn block_signals(&mut self) -> io::Result<()> {
        let set = shutdown_signals();
        set.thread_block()?;
        self.signals = Some(SignalFd::with_flags(&set, SfdFlags::SFD_CLOEXEC)?);
        Ok(())
    }

    fn open_control(&mut self) -> io::Result<()> {
        let device = find_port(Path::new(VIRTIO_PORTS), Path::new("/dev"), CONTROL_PORT)?;
        self.control = Some(File::options().read(true).write(true).open(device)?);
        Ok(())
    }

    fn next_event(&mut self) -> io::Result<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            let signals = self
                .signals
                .as_ref()
                .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "signals are not blocked"))?;
            let (signal_ready, control_ready, outputs_ready) = {
                let mut fds = vec![PollFd::new(signals.as_fd(), PollFlags::POLLIN)];
                let has_control = self.control.is_some();
                if let Some(port) = &self.control {
                    fds.push(PollFd::new(port.as_fd(), PollFlags::POLLIN));
                }
                let first_output = fds.len();
                if let Some(run) = &self.run {
                    for (_, file) in &run.outputs {
                        fds.push(PollFd::new(file.as_fd(), PollFlags::POLLIN));
                    }
                }
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                let ready = |fd: &PollFd<'_>| fd.revents().is_some_and(|r| !r.is_empty());
                let outputs: Vec<bool> = fds[first_output..].iter().map(ready).collect();
                (ready(&fds[0]), has_control && ready(&fds[1]), outputs)
            };
            if signal_ready {
                if let Some(info) = signals.read_signal()? {
                    let signal = match NixSignal::try_from(info.ssi_signo as i32) {
                        Ok(NixSignal::SIGTERM) => Some(Signal::Term),
                        Ok(NixSignal::SIGINT) => Some(Signal::Int),
                        Ok(NixSignal::SIGCHLD) => Some(Signal::Child),
                        _ => None,
                    };
                    self.pending.extend(signal.map(Event::Signal));
                }
            }
            if control_ready {
                self.read_control();
            }
            self.read_outputs(&outputs_ready);
        }
    }

    fn start(&mut self, spec: &RunSpec) -> io::Result<i32> {
        if self.run.is_some() {
            return Err(io::Error::new(ErrorKind::AlreadyExists, "a command is already running"));
        }
        for m in &ROOTFS_MOUNTS {
            std::fs::create_dir_all(m.target)?;
            match self.mount(m) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::ResourceBusy => {}
                Err(e) => return Err(io::Error::new(e.kind(), format!("mounting {}: {e}", m.target))),
            }
        }
        // The agent moves into the image root with the command: one command runs per sandbox.
        chroot(ROOTFS).map_err(context("entering the image root"))?;
        chdir("/").map_err(context("entering the image root"))?;
        chdir(spec.cwd.as_str()).map_err(context(&format!("working directory {}", spec.cwd)))?;
        let path = resolve(&spec.argv[0], &spec.env)?;
        let argv = cstrings(&spec.argv)?;
        let envp = cstrings(&spec.env)?;

        let mut attr = PosixSpawnAttr::init()?;
        // The agent blocks SIGTERM, SIGINT and SIGCHLD; the command must start with nothing blocked.
        attr.set_sigmask(&SigSet::empty())?;
        let mut defaults = shutdown_signals();
        defaults.add(NixSignal::SIGHUP);
        attr.set_sigdefault(&defaults)?;
        attr.set_pgroup(Pid::from_raw(0))?;
        attr.set_flags(
            PosixSpawnFlags::POSIX_SPAWN_SETSIGMASK
                | PosixSpawnFlags::POSIX_SPAWN_SETSIGDEF
                | PosixSpawnFlags::POSIX_SPAWN_SETPGROUP,
        )?;
        let mut actions = PosixSpawnFileActions::init()?;

        let running = if spec.tty {
            let size = spec.size.map(|(rows, cols)| Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            });
            let pty = openpty(size.as_ref(), None).map_err(context("opening a terminal"))?;
            // A session leader that opens a terminal without O_NOCTTY makes it its controlling terminal, so the
            // command's foreground process group gets ^C and job control works.
            let _ = setsid();
            let name = ttyname(&pty.slave).map_err(context("naming the terminal"))?;
            let terminal = open(&name, OFlag::O_RDWR | OFlag::O_CLOEXEC, Mode::empty())
                .map_err(context("opening the terminal"))?;
            drop(pty.slave);
            for fd in 0..3 {
                actions.add_dup2(terminal.as_raw_fd(), fd)?;
            }
            let pid =
                posix_spawn(path.as_path(), &actions, &attr, &argv, &envp).map_err(context("starting the command"))?;
            let _ = tcsetpgrp(&terminal, pid);
            let master = File::from(pty.master);
            self.run = Some(Running {
                tty: true,
                stdin: Some(master.try_clone()?),
                outputs: vec![(Stream::Stdout, master)],
                _terminal: Some(terminal),
            });
            pid
        } else {
            let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
            let (stdout_r, stdout_w) = pipe2(OFlag::O_CLOEXEC)?;
            let (stderr_r, stderr_w) = pipe2(OFlag::O_CLOEXEC)?;
            actions.add_dup2(stdin_r.as_raw_fd(), 0)?;
            actions.add_dup2(stdout_w.as_raw_fd(), 1)?;
            actions.add_dup2(stderr_w.as_raw_fd(), 2)?;
            let pid =
                posix_spawn(path.as_path(), &actions, &attr, &argv, &envp).map_err(context("starting the command"))?;
            self.run = Some(Running {
                tty: false,
                stdin: Some(File::from(stdin_w)),
                outputs: vec![
                    (Stream::Stdout, File::from(stdout_r)),
                    (Stream::Stderr, File::from(stderr_r)),
                ],
                _terminal: None,
            });
            pid
        };
        Ok(running.as_raw())
    }

    fn write_stdin(&mut self, data: &[u8]) -> io::Result<()> {
        let stdin = self
            .run
            .as_mut()
            .and_then(|r| r.stdin.as_mut())
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "the command's stdin is closed"))?;
        stdin.write_all(data)
    }

    fn close_stdin(&mut self) {
        // A terminal stays open: closing it would hang up the command.
        if let Some(run) = self.run.as_mut().filter(|r| !r.tty) {
            run.stdin = None;
        }
    }

    fn drain_output(&mut self) -> Vec<(Stream, Vec<u8>)> {
        let mut drained = Vec::new();
        if let Some(mut run) = self.run.take() {
            for (stream, file) in &mut run.outputs {
                let mut buf = [0u8; 4096];
                loop {
                    let readable = {
                        let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
                        matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0)
                            && fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLIN))
                    };
                    if !readable {
                        break;
                    }
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => drained.push((*stream, buf[..n].to_vec())),
                    }
                }
            }
        }
        drained
    }

    fn send_control(&mut self, line: &str) -> io::Result<()> {
        let port = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "no control channel"))?;
        port.write_all(line.as_bytes())?;
        port.flush()
    }

    fn reap(&mut self) -> io::Result<Vec<Reaped>> {
        let mut reaped = Vec::new();
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => reaped.push(Reaped {
                    pid: pid.as_raw(),
                    code,
                    status: format!("exited with status {code}"),
                }),
                Ok(WaitStatus::Signaled(pid, sig, _)) => reaped.push(Reaped {
                    pid: pid.as_raw(),
                    code: 128 + sig as i32,
                    status: format!("killed by {sig}"),
                }),
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return Ok(reaped),
                Ok(_) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn sync(&mut self) {
        nix::unistd::sync();
    }

    fn power_off(&mut self) -> io::Error {
        match reboot(RebootMode::RB_POWER_OFF) {
            Ok(never) => match never {},
            Err(e) => e.into(),
        }
    }
}

/// Run as PID 1 and never return: a PID 1 that exits or panics panics the kernel.
pub fn run_as_init() -> ! {
    let mut sys = Linux::new();
    let outcome = std::panic::catch_unwind(|| init::run(&mut Linux::new(), &mut io::stdout()));
    let mut out = io::stdout();
    if outcome.is_err() {
        let _ = writeln!(out, "hermit-agent: fatal: panicked, powering off");
        sys.sync();
        let err = sys.power_off();
        let _ = writeln!(out, "hermit-agent: power off failed: {err}");
    }
    let _ = writeln!(out, "hermit-agent: halted");
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_is_not_pid1() {
        assert!(!is_pid1());
    }

    #[test]
    fn mkdir_all_creates_parents() {
        let dir = std::env::temp_dir().join(format!("hermit-agent-{}", std::process::id()));
        let nested = dir.join("a/b");
        Linux::new().mkdir_all(nested.to_str().unwrap()).unwrap();
        assert!(nested.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mount_without_privileges_fails() {
        let m = Mount {
            target: "/nonexistent-hermit-agent-target",
            ..init::EARLY_MOUNTS[5]
        };
        assert!(Linux::new().mount(&m).is_err());
    }

    #[test]
    fn finds_the_control_port_by_name() {
        let dir = std::env::temp_dir().join(format!("hermit-agent-ports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (port, name) in [
            ("vport0p1", "org.example.other\n"),
            ("vport0p2", "org.oci-hermit.control\n"),
        ] {
            std::fs::create_dir_all(dir.join(port)).unwrap();
            std::fs::write(dir.join(port).join("name"), name).unwrap();
        }
        let found = find_port(&dir, Path::new("/dev"), CONTROL_PORT).unwrap();
        assert_eq!(found, Path::new("/dev/vport0p2"));
        let err = find_port(&dir, Path::new("/dev"), "org.example.missing").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(find_port(&dir.join("absent"), Path::new("/dev"), CONTROL_PORT).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn control_lines_become_events_and_hangup_closes_the_channel() {
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sys = Linux::new();
        sys.block_signals().unwrap();
        sys.control = Some(File::from(std::os::fd::OwnedFd::from(ours)));
        theirs.write_all(b"{\"type\":\"shutdown\"}\nsecond").unwrap();
        assert_eq!(
            sys.next_event().unwrap(),
            Event::Control(hermit_proto::Frame::Line(r#"{"type":"shutdown"}"#.into()))
        );
        sys.send_control("reply\n").unwrap();
        let mut got = [0u8; 6];
        theirs.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"reply\n");
        theirs.write_all(b" line\n").unwrap();
        assert_eq!(
            sys.next_event().unwrap(),
            Event::Control(hermit_proto::Frame::Line("second line".into()))
        );
        drop(theirs);
        assert_eq!(sys.next_event().unwrap(), Event::ControlClosed);
        assert_eq!(sys.send_control("x\n").unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[test]
    fn resolves_commands_through_the_image_path() {
        let dir = std::env::temp_dir().join(format!("hermit-agent-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (bin, other) = (dir.join("bin"), dir.join("other"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        for (path, mode) in [
            (bin.join("tool"), 0o755),
            (other.join("tool"), 0o755),
            (bin.join("data"), 0o644),
        ] {
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let env = vec![format!("PATH={}:{}", bin.display(), other.display())];
        assert_eq!(resolve("tool", &env).unwrap(), bin.join("tool"));
        assert_eq!(
            resolve("data", &env).unwrap_err().kind(),
            ErrorKind::NotFound,
            "not executable"
        );
        assert_eq!(resolve("/abs/path", &env).unwrap(), Path::new("/abs/path"));
        assert_eq!(resolve("./rel", &[]).unwrap(), Path::new("./rel"));
        assert!(resolve("no-such-tool-anywhere", &[]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_a_nul_in_the_command() {
        assert_eq!(
            cstrings(&["ok".into(), "bad\0".into()]).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_second_start_is_refused() {
        let mut sys = Linux::new();
        sys.run = Some(Running {
            tty: false,
            stdin: None,
            outputs: Vec::new(),
            _terminal: None,
        });
        let spec = RunSpec {
            argv: vec!["true".into()],
            env: Vec::new(),
            cwd: "/".into(),
            tty: false,
            size: None,
        };
        assert_eq!(sys.start(&spec).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(sys.write_stdin(b"x").unwrap_err().kind(), ErrorKind::NotConnected);
        sys.close_stdin();
        assert!(sys.drain_output().is_empty());
    }

    #[test]
    fn waiting_before_blocking_signals_is_an_error() {
        assert_eq!(Linux::new().next_event().unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[test]
    #[expect(clippy::zombie_processes, reason = "the code under test reaps the child")]
    fn reap_collects_exited_children() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let mut reaped = Vec::new();
        for _ in 0..200 {
            reaped.extend(Linux::new().reap().unwrap());
            if reaped.iter().any(|r| r.pid == pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            reaped
                .iter()
                .any(|r| r.pid == pid && r.status == "exited with status 0")
        );
    }
}
