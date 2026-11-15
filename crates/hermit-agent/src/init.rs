//! PID 1 duties, independent of the operating system so they can be tested on any host.
//!
//! The agent mounts the early filesystems, logs a ready line, then waits for signals: it reaps children on
//! SIGCHLD and powers off on SIGTERM or SIGINT. Any fatal setup error is logged and also ends in a power-off,
//! because PID 1 must never exit or panic. Signals are the interim shutdown request until the control
//! protocol (issue #10) carries one.

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

/// The system calls PID 1 needs.
pub trait System {
    /// Create a directory and its parents.
    fn mkdir_all(&mut self, path: &str) -> io::Result<()>;
    /// Mount a filesystem; an already-mounted target fails with [`ErrorKind::ResourceBusy`].
    fn mount(&mut self, mount: &Mount) -> io::Result<()>;
    /// Block SIGTERM, SIGINT and SIGCHLD so [`System::wait_signal`] receives them.
    fn block_signals(&mut self) -> io::Result<()>;
    /// Wait for the next blocked signal.
    fn wait_signal(&mut self) -> io::Result<Signal>;
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

/// Run PID 1: set up, wait for a shutdown signal, then power off.
///
/// Returns only if powering off failed, with that error.
pub fn run(sys: &mut impl System, out: &mut impl Write) -> io::Error {
    log(out, format_args!("starting {} as PID 1", env!("CARGO_PKG_VERSION")));
    match setup(sys, out) {
        Ok(()) => {
            let (major, minor) = hermit_proto::PROTOCOL_VERSION;
            log(
                out,
                format_args!("ready (control protocol {major}.{minor}); SIGTERM or SIGINT powers off"),
            );
            wait_for_shutdown(sys, out);
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

fn wait_for_shutdown(sys: &mut impl System, out: &mut impl Write) {
    loop {
        match sys.wait_signal() {
            Ok(Signal::Child) => reap(sys, out),
            Ok(sig) => {
                log(out, format_args!("received {}, shutting down", sig.name()));
                return;
            }
            Err(e) => {
                log(out, format_args!("fatal: waiting for signals: {e}"));
                return;
            }
        }
    }
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
        signals: VecDeque<io::Result<Signal>>,
        reaps: VecDeque<io::Result<Vec<Reaped>>>,
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
        fn wait_signal(&mut self) -> io::Result<Signal> {
            self.calls.push("wait".into());
            self.signals.pop_front().unwrap_or(Ok(Signal::Term))
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

    #[test]
    fn mounts_everything_then_powers_off_on_sigterm() {
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
        assert!(log.contains("ready (control protocol 0.1)"));
        assert!(
            log.contains(hermit_proto::CONSOLE_READY),
            "the host waits for this line: {log}"
        );
        assert!(log.contains("received SIGTERM, shutting down"));
        assert!(log.lines().all(|l| l.starts_with("hermit-agent: ")));
        assert!(ends_with_power_off(&sys.calls));
        assert!(log.contains("power off failed"));
    }

    #[test]
    fn devpts_is_mounted_after_dev() {
        let pos = |t: &str| EARLY_MOUNTS.iter().position(|m| m.target == t).unwrap();
        assert!(pos("/dev") < pos("/dev/pts"));
    }

    #[test]
    fn sigint_also_powers_off() {
        let mut sys = Fake {
            signals: VecDeque::from([Ok(Signal::Int)]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("received SIGINT"));
        assert!(ends_with_power_off(&sys.calls));
    }

    #[test]
    fn reaps_children_on_sigchld_and_keeps_waiting() {
        let mut sys = Fake {
            signals: VecDeque::from([Ok(Signal::Child), Ok(Signal::Child), Ok(Signal::Term)]),
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
    fn signal_wait_failure_powers_off() {
        let mut sys = Fake {
            signals: VecDeque::from([Err(io::Error::from(ErrorKind::InvalidInput))]),
            ..Fake::default()
        };
        let (_, log) = run_fake(&mut sys);
        assert!(log.contains("fatal: waiting for signals"));
        assert!(ends_with_power_off(&sys.calls));
    }
}
