//! The real system calls, for the Linux guest.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::path::Path;

use hermit_proto::{CONTROL_PORT, Framer};
use nix::errno::Errno;
use nix::mount::MsFlags;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::signal::{SigSet, Signal as NixSignal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

use crate::init::{self, Event, Mount, Reaped, Signal, System};

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
}

impl Linux {
    pub fn new() -> Self {
        Self::default()
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
            let (signal_ready, control_ready) = {
                let mut fds = vec![PollFd::new(signals.as_fd(), PollFlags::POLLIN)];
                if let Some(port) = &self.control {
                    fds.push(PollFd::new(port.as_fd(), PollFlags::POLLIN));
                }
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                let ready = |fd: &PollFd<'_>| fd.revents().is_some_and(|r| !r.is_empty());
                (ready(&fds[0]), fds.get(1).is_some_and(ready))
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
        }
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
                    status: format!("exited with status {code}"),
                }),
                Ok(WaitStatus::Signaled(pid, sig, _)) => reaped.push(Reaped {
                    pid: pid.as_raw(),
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
