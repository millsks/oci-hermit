//! The real system calls, for the Linux guest.

use std::io::{self, Write};

use nix::errno::Errno;
use nix::mount::MsFlags;
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::signal::{SigSet, Signal as NixSignal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

use crate::init::{self, Mount, Reaped, Signal, System};

/// Whether this process is PID 1.
pub fn is_pid1() -> bool {
    nix::unistd::getpid().as_raw() == 1
}

/// Linux implementation of [`System`].
pub struct Linux;

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
        shutdown_signals().thread_block()?;
        Ok(())
    }

    fn wait_signal(&mut self) -> io::Result<Signal> {
        loop {
            match shutdown_signals().wait()? {
                NixSignal::SIGTERM => return Ok(Signal::Term),
                NixSignal::SIGINT => return Ok(Signal::Int),
                NixSignal::SIGCHLD => return Ok(Signal::Child),
                _ => continue,
            }
        }
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
    let mut sys = Linux;
    let outcome = std::panic::catch_unwind(|| init::run(&mut Linux, &mut io::stdout()));
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
        Linux.mkdir_all(nested.to_str().unwrap()).unwrap();
        assert!(nested.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mount_without_privileges_fails() {
        let m = Mount {
            target: "/nonexistent-hermit-agent-target",
            ..init::EARLY_MOUNTS[5]
        };
        assert!(Linux.mount(&m).is_err());
    }

    #[test]
    #[expect(clippy::zombie_processes, reason = "the code under test reaps the child")]
    fn reap_collects_exited_children() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let mut reaped = Vec::new();
        for _ in 0..200 {
            reaped.extend(Linux.reap().unwrap());
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
