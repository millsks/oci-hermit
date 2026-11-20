//! oci-hermit guest agent.
//!
//! Runs as PID 1 inside the sandbox VM: mounts the early filesystems, logs to the console, reaps children and
//! powers off cleanly. Anywhere else (not PID 1, or not Linux) it refuses to start.

use std::io::Write;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
mod copy;
#[cfg(any(target_os = "linux", test))]
mod init;
#[cfg(target_os = "linux")]
mod linux;

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    if linux::is_pid1() {
        linux::run_as_init();
    }
    ExitCode::from(refuse(cfg!(target_os = "linux"), &mut std::io::stderr()))
}

/// Explain why the agent will not start here, and return its exit code.
fn refuse(on_linux: bool, err: &mut impl Write) -> u8 {
    let reason = if on_linux {
        "must run as PID 1 inside the Linux guest VM"
    } else {
        "runs only inside the Linux guest VM"
    };
    let _ = writeln!(
        err,
        "hermit-agent {}: {reason}; refusing to start",
        env!("CARGO_PKG_VERSION")
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(on_linux: bool) -> String {
        let mut err = Vec::new();
        assert_eq!(refuse(on_linux, &mut err), 1);
        String::from_utf8(err).unwrap()
    }

    #[test]
    fn refuses_outside_linux() {
        assert!(refusal(false).contains("only inside the Linux guest"));
    }

    #[test]
    fn refuses_when_not_pid1() {
        assert!(refusal(true).contains("must run as PID 1"));
    }
}
