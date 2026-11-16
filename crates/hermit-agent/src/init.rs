//! PID 1 duties, independent of the operating system so they can be tested on any host.
//!
//! The agent mounts the early filesystems, opens the control channel to the host, logs a ready line, then handles
//! events until it is asked to stop: it answers the host's control messages (`hermit_proto`), reaps children on
//! SIGCHLD, and powers off on the host's `Shutdown`, on SIGTERM or on SIGINT. Any fatal setup error is logged and
//! also ends in a power-off, because PID 1 must never exit or panic. Without a control channel the agent still
//! runs, and signals still stop it.

use hermit_proto::{CONTROL_PORT, Frame, GuestMessage, HostMessage, PROTOCOL_VERSION};
use std::io::{self, ErrorKind, Write};

/// A signal the agent waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: power off.
    Term,
    /// SIGINT: power off.
    Int,
    /// SIGCHLD: reap exited children.
    Child,
}

impl Signal {
    fn name(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Int => "SIGINT",
            Signal::Child => "SIGCHLD",
        }
    }
}

/// A child process that was reaped, with a human-readable description of how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    pub pid: i32,
    pub status: String,
}

/// One early mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mount {
    pub source: &'static str,
    pub target: &'static str,
    pub fstype: &'static str,
    pub nosuid: bool,
    pub nodev: bool,
    pub noexec: bool,
    pub data: Option<&'static str>,
}

/// The early mounts, in order: `/dev/pts` lives on the devtmpfs mounted before it.
pub const EARLY_MOUNTS: [Mount; 6] = [
    Mount {
        source: "proc",
        target: "/proc",
        fstype: "proc",
        nosuid: true,
        nodev: true,
        noexec: true,
        data: None,
    },
    Mount {
        source: "sysfs",
        target: "/sys",
        fstype: "sysfs",
        nosuid: true,
        nodev: true,
        noexec: true,
        data: None,
    },
    Mount {
        source: "devtmpfs",
        target: "/dev",
        fstype: "devtmpfs",
        nosuid: true,
        nodev: false,
        noexec: false,
        data: Some("mode=0755"),
    },
    Mount {
        source: "devpts",
        target: "/dev/pts",
        fstype: "devpts",
        nosuid: true,
        nodev: false,
        noexec: true,
        data: Some("mode=0620,ptmxmode=0666"),
    },
    Mount {
        source: "tmpfs",
        target: "/run",
        fstype: "tmpfs",
        nosuid: true,
        nodev: true,
        noexec: false,
        data: Some("mode=0755"),
    },
    Mount {
        source: "tmpfs",
        target: "/tmp",
        fstype: "tmpfs",
        nosuid: true,
        nodev: true,
        noexec: false,
        data: Some("mode=1777"),
    },
];

/// Something that happened while the agent waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A blocked signal arrived.
    Signal(Signal),
    /// A frame arrived on the control channel.
    Control(Frame),
    /// The host's end of the control channel went away.
    ControlClosed,
}

/// The system calls PID 1 needs.
pub trait System {
    /// Create a directory and its parents.
    fn mkdir_all(&mut self, path: &str) -> io::Result<()>;
    /// Mount a filesystem; an already-mounted target fails with [`ErrorKind::ResourceBusy`].
    fn mount(&mut self, mount: &Mount) -> io::Result<()>;
    /// Block SIGTERM, SIGINT and SIGCHLD so [`System::next_event`] receives them.
    fn block_signals(&mut self) -> io::Result<()>;
    /// Open the virtio-serial port named [`CONTROL_PORT`].
    fn open_control(&mut self) -> io::Result<()>;
    /// Wait for the next signal or control frame.
    fn next_event(&mut self) -> io::Result<Event>;
    /// Send one encoded line to the host.
    fn send_control(&mut self, line: &str) -> io::Result<()>;
    /// Reap every child that has exited, without blocking.
    fn reap(&mut self) -> io::Result<Vec<Reaped>>;
    /// Flush filesystem buffers.
    fn sync(&mut self);
    /// Power the machine off. Returns only on failure.
    fn power_off(&mut self) -> io::Error;
}

/// Log one line to the console. A console write error has nowhere else to go, so it is dropped.
fn log(out: &mut impl Write, msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(out, "hermit-agent: {msg}");
    let _ = out.flush();
}

/// Run PID 1: set up, handle events until asked to stop, then power off.
///
/// Returns only if powering off failed, with that error.
pub fn run(sys: &mut impl System, out: &mut impl Write) -> io::Error {
    log(out, format_args!("starting {} as PID 1", env!("CARGO_PKG_VERSION")));
    match setup(sys, out) {
        Ok(()) => {
            match sys.open_control() {
                Ok(()) => log(out, format_args!("control channel open on {CONTROL_PORT}")),
                Err(e) => log(
                    out,
                    format_args!("no control channel ({e}); SIGTERM or SIGINT powers off"),
                ),
            }
            let (major, minor) = PROTOCOL_VERSION;
            log(out, format_args!("ready (control protocol {major}.{minor})"));
            serve(sys, out);
        }
        Err(e) => log(out, format_args!("fatal: {e}")),
    }
    shutdown(sys, out)
}

fn setup(sys: &mut impl System, out: &mut impl Write) -> io::Result<()> {
    sys.block_signals()
        .map_err(|e| io::Error::new(e.kind(), format!("blocking signals: {e}")))?;
    for m in &EARLY_MOUNTS {
        sys.mkdir_all(m.target)
            .map_err(|e| io::Error::new(e.kind(), format!("creating {}: {e}", m.target)))?;
        match sys.mount(m) {
            Ok(()) => log(out, format_args!("mounted {} on {}", m.fstype, m.target)),
            Err(e) if e.kind() == ErrorKind::ResourceBusy => {
                log(out, format_args!("{} already mounted, skipping", m.target));
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("mounting {} on {}: {e}", m.fstype, m.target),
                ));
            }
        }
    }
    Ok(())
}

/// Handle events until one asks the agent to stop.
fn serve(sys: &mut impl System, out: &mut impl Write) {
    loop {
        match sys.next_event() {
            Ok(Event::Signal(Signal::Child)) => reap(sys, out),
            Ok(Event::Signal(sig)) => {
                log(out, format_args!("received {}, shutting down", sig.name()));
                return;
            }
            Ok(Event::Control(frame)) => {
                if answer(sys, out, frame).is_break() {
                    return;
                }
            }
            Ok(Event::ControlClosed) => log(out, format_args!("control channel closed by the host")),
            Err(e) => {
                log(out, format_args!("fatal: waiting for events: {e}"));
                return;
            }
        }
    }
}

/// Answer one control frame. Breaks when the host asked for a shutdown.
fn answer(sys: &mut impl System, out: &mut impl Write, frame: Frame) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow;
    let (reply, flow) = match frame {
        Frame::Line(line) => match hermit_proto::decode::<HostMessage>(&line) {
            Ok(HostMessage::Hello { version }) if hermit_proto::compatible(version) => {
                log(
                    out,
                    format_args!("host connected (control protocol {}.{})", version.0, version.1),
                );
                (
                    GuestMessage::Ready {
                        version: PROTOCOL_VERSION,
                    },
                    ControlFlow::Continue(()),
                )
            }
            Ok(HostMessage::Hello { version }) => {
                let message = format!(
                    "control protocol {}.{} is not supported; this agent speaks {}.{}",
                    version.0, version.1, PROTOCOL_VERSION.0, PROTOCOL_VERSION.1
                );
                log(out, format_args!("refused the host: {message}"));
                (GuestMessage::Error { message }, ControlFlow::Continue(()))
            }
            Ok(HostMessage::Shutdown) => {
                log(out, format_args!("host requested shutdown"));
                (GuestMessage::ShuttingDown, ControlFlow::Break(()))
            }
            Err(e) => (
                GuestMessage::Error {
                    message: format!("not a control message: {e}"),
                },
                ControlFlow::Continue(()),
            ),
        },
        Frame::TooLong => (
            GuestMessage::Error {
                message: format!("line longer than {} bytes", hermit_proto::MAX_LINE),
            },
            ControlFlow::Continue(()),
        ),
        Frame::NotUtf8 => (
            GuestMessage::Error {
                message: "line is not UTF-8".into(),
            },
            ControlFlow::Continue(()),
        ),
    };
    match hermit_proto::encode(&reply) {
        Ok(line) => {
            if let Err(e) = sys.send_control(&line) {
                log(out, format_args!("sending to the host failed: {e}"));
            }
        }
        Err(e) => log(out, format_args!("encoding a reply failed: {e}")),
    }
    flow
}

fn reap(sys: &mut impl System, out: &mut impl Write) {
    match sys.reap() {
        Ok(children) => {
            for c in children {
                log(out, format_args!("reaped pid {}: {}", c.pid, c.status));
            }
        }
        Err(e) => log(out, format_args!("reaping children failed: {e}")),
    }
}

fn shutdown(sys: &mut impl System, out: &mut impl Write) -> io::Error {
    log(out, format_args!("syncing filesystems"));
    sys.sync();
    log(out, format_args!("powering off"));
    let err = sys.power_off();
    log(out, format_args!("power off failed: {err}"));
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        mount_errors: HashMap<&'static str, ErrorKind>,
        mkdir_error: Option<&'static str>,
        block_error: bool,
        no_control: bool,
        send_error: bool,
        events: VecDeque<io::Result<Event>>,
        reaps: VecDeque<io::Result<Vec<Reaped>>>,
        sent: Vec<String>,
    }

    impl System for Fake {
        fn mkdir_all(&mut self, path: &str) -> io::Result<()> {
            self.calls.push(format!("mkdir {path}"));
            if self.mkdir_error == Some(path) {
                return Err(io::Error::from(ErrorKind::PermissionDenied));
            }
            Ok(())
        }
        fn mount(&mut self, m: &Mount) -> io::Result<()> {
            self.calls.push(format!("mount {}", m.target));
            match self.mount_errors.get(m.target) {
                Some(kind) => Err(io::Error::from(*kind)),
                None => Ok(()),
            }
        }
        fn block_signals(&mut self) -> io::Result<()> {
            self.calls.push("block".into());
            if self.block_error {
                return Err(io::Error::from(ErrorKind::InvalidInput));
            }
            Ok(())
        }
        fn open_control(&mut self) -> io::Result<()> {
            self.calls.push("open_control".into());
            if self.no_control {
                return Err(io::Error::new(
                    ErrorKind::NotFound,
                    "no port named org.oci-hermit.control",
                ));
            }
            Ok(())
        }
        fn next_event(&mut self) -> io::Result<Event> {
            self.calls.push("wait".into());
            self.events.pop_front().unwrap_or(Ok(Event::Signal(Signal::Term)))
        }
        fn send_control(&mut self, line: &str) -> io::Result<()> {
            if self.send_error {
                return Err(io::Error::from(ErrorKind::BrokenPipe));
            }
            self.sent.push(line.to_string());
            Ok(())
        }
        fn reap(&mut self) -> io::Result<Vec<Reaped>> {
            self.calls.push("reap".into());
            self.reaps.pop_front().unwrap_or(Ok(Vec::new()))
        }
        fn sync(&mut self) {
            self.calls.push("sync".into());
        }
        fn power_off(&mut self) -> io::Error {
            self.calls.push("power_off".into());
            io::Error::from(ErrorKind::PermissionDenied)
        }
    }

    fn run_fake(sys: &mut Fake) -> (io::Error, String) {
        let mut out = Vec::new();
        let err = run(sys, &mut out);
        (err, String::from_utf8(out).unwrap())
    }

    fn ends_with_power_off(calls: &[String]) -> bool {
        calls.ends_with(&["sync".to_string(), "power_off".to_string()])
    }

    fn line(msg: &HostMessage) -> io::Result<Event> {
        let text = hermit_proto::encode(msg).unwrap();
        Ok(Event::Control(Frame::Line(text.trim_end().to_string())))
    }

    fn replies(sys: &Fake) -> Vec<GuestMessage> {
        sys.sent.iter().map(|l| hermit_proto::decode(l).unwrap()).collect()
    }

    fn with_events(events: Vec<io::Result<Event>>) -> Fake {
        Fake {
            events: VecDeque::from(events),
            ..Fake::default()
        }
    }

    #[test]
    fn mounts_everything_opens_control_then_powers_off_on_sigterm() {
        let mut sys = Fake::default();
        let (err, log) = run_fake(&mut sys);
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        for m in &EARLY_MOUNTS {
            assert!(sys.calls.contains(&format!("mkdir {}", m.target)));
            assert!(log.contains(&format!("mounted {} on {}", m.fstype, m.target)));
        }
        let mounts: Vec<_> = sys.calls.iter().filter(|c| c.starts_with("mount ")).collect();
        assert_eq!(mounts.len(), EARLY_MOUNTS.len());
        assert_eq!(sys.calls[0], "block");
        assert!(log.contains("control channel open on org.oci-hermit.control"));
        assert!(log.contains("ready (control protocol 1.0)"));
        assert!(
            log.contains(hermit_proto::CONSOLE_READY),
            "people read this line: {log}"
        );
        assert!(log.contains("received SIGTERM, shutting down"));
        assert!(log.lines().all(|l| l.starts_with("hermit-agent: ")));
        assert!(ends_with_power_off(&sys.calls));
        assert!(log.contains("power off failed"));
    }

    #[test]
    fn answers_a_compatible_hello_with_ready() {
        let mut sys = with_events(vec![line(&HostMessage::Hello {
            version: (PROTOCOL_VERSION.0, PROTOCOL_VERSION.1 + 3),
        })]);
        let (_, log) = run_fake(&mut sys);
        assert_eq!(
            replies(&sys),
            [GuestMessage::Ready {
                version: PROTOCOL_VERSION
            }]
        );
        assert!(log.contains("host connected (control protocol 1.3)"));
    }

    #[test]
    fn refuses_another_major_version_and_keeps_serving() {
        let mut sys = with_events(vec![
            line(&HostMessage::Hello { version: (9, 0) }),
            line(&HostMessage::Hello {
                version: PROTOCOL_VERSION,
            }),
        ]);
        let (_, log) = run_fake(&mut sys);
        let got = replies(&sys);
        assert!(
            matches!(&got[0], GuestMessage::Error { message } if message.contains("9.0 is not supported")),
            "{got:?}"
        );
        assert!(matches!(got[1], GuestMessage::Ready { .. }));
        assert!(log.contains("refused the host"));
    }

    #[test]
    fn shuts_down_when_the_host_asks() {
        let mut sys = with_events(vec![line(&HostMessage::Shutdown), Ok(Event::Signal(Signal::Int))]);
        let (_, log) = run_fake(&mut sys);
        assert_eq!(replies(&sys), [GuestMessage::ShuttingDown]);
        assert!(log.contains("host requested shutdown"));
        assert!(!log.contains("received SIGINT"), "stopped at the shutdown request");
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn rejects_anything_that_is_not_a_message_and_keeps_serving() {
        let mut sys = with_events(vec![
            Ok(Event::Control(Frame::Line(r#"{"type":"launch_missiles"}"#.into()))),
            Ok(Event::Control(Frame::Line("not json".into()))),
            Ok(Event::Control(Frame::TooLong)),
            Ok(Event::Control(Frame::NotUtf8)),
            line(&HostMessage::Shutdown),
        ]);
        run_fake(&mut sys);
        let got = replies(&sys);
        assert_eq!(got.len(), 5);
        let errors: Vec<_> = got[..4]
            .iter()
            .map(|m| match m {
                GuestMessage::Error { message } => message.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(errors[0].starts_with("not a control message"));
        assert!(errors[1].starts_with("not a control message"));
        assert_eq!(errors[2], format!("line longer than {} bytes", hermit_proto::MAX_LINE));
        assert_eq!(errors[3], "line is not UTF-8");
        assert_eq!(got[4], GuestMessage::ShuttingDown);
    }

    #[test]
    fn a_closed_control_channel_is_logged_and_signals_still_work() {
        let mut sys = with_events(vec![Ok(Event::ControlClosed), Ok(Event::Signal(Signal::Term))]);
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("control channel closed by the host"));
        assert!(log.contains("received SIGTERM"));
    }

    #[test]
    fn runs_without_a_control_channel() {
        let mut sys = Fake {
            no_control: true,
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("no control channel (no port named org.oci-hermit.control)"));
        assert!(log.contains("ready (control protocol 1.0)"));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn a_failed_send_is_logged_not_fatal() {
        let mut sys = Fake {
            send_error: true,
            events: VecDeque::from([
                line(&HostMessage::Hello {
                    version: PROTOCOL_VERSION,
                }),
                Ok(Event::Signal(Signal::Term)),
            ]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("sending to the host failed"));
        assert!(log.contains("received SIGTERM"));
    }

    #[test]
    fn devpts_is_mounted_after_dev() {
        let pos = |t: &str| EARLY_MOUNTS.iter().position(|m| m.target == t).unwrap();
        assert!(pos("/dev") < pos("/dev/pts"));
    }

    #[test]
    fn sigint_also_powers_off() {
        let mut sys = with_events(vec![Ok(Event::Signal(Signal::Int))]);
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("received SIGINT"));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn reaps_children_on_sigchld_and_keeps_waiting() {
        let mut sys = Fake {
            events: VecDeque::from([
                Ok(Event::Signal(Signal::Child)),
                Ok(Event::Signal(Signal::Child)),
                Ok(Event::Signal(Signal::Term)),
            ]),
            reaps: VecDeque::from([
                Ok(vec![Reaped {
                    pid: 42,
                    status: "exited with status 0".into(),
                }]),
                Err(io::Error::from(ErrorKind::Interrupted)),
            ]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("reaped pid 42: exited with status 0"));
        assert!(log.contains("reaping children failed"));
        assert_eq!(sys.calls.iter().filter(|c| *c == "wait").count(), 3);
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn tolerates_already_mounted() {
        let mut sys = Fake {
            mount_errors: HashMap::from([("/proc", ErrorKind::ResourceBusy)]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("/proc already mounted, skipping"));
        assert!(log.contains("mounted tmpfs on /tmp"));
        assert!(log.contains("ready"));
    }

    #[test]
    fn mount_failure_is_fatal_and_powers_off() {
        let mut sys = Fake {
            mount_errors: HashMap::from([("/sys", ErrorKind::PermissionDenied)]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("fatal: mounting sysfs on /sys"));
        assert!(!log.contains("ready"));
        assert!(!sys.calls.contains(&"mount /dev".to_string()));
        assert!(!sys.calls.contains(&"open_control".to_string()));
        assert!(!sys.calls.contains(&"wait".to_string()));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn mkdir_failure_is_fatal() {
        let mut sys = Fake {
            mkdir_error: Some("/run"),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("fatal: creating /run"));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn signal_setup_failure_is_fatal() {
        let mut sys = Fake {
            block_error: true,
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("fatal: blocking signals"));
        assert!(!sys.calls.iter().any(|c| c.starts_with("mount")));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn event_wait_failure_powers_off() {
        let mut sys = with_events(vec![Err(io::Error::from(ErrorKind::InvalidInput))]);
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("fatal: waiting for events"));
        assert!(ends_with_power_off(&sys.calls));
    }
}
