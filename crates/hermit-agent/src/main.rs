//! oci-hermit guest agent.
//!
//! Runs as PID 1 inside the sandbox VM. It will mount the layer disks and 9P
//! shares, start containers with crun, and answer the host tool over the
//! control channel. The real agent lands in milestone M1.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(run(
        cfg!(target_os = "linux"),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

/// Run the agent, refusing to start outside a Linux guest, and return its exit code.
fn run(on_linux: bool, out: &mut impl Write, err: &mut impl Write) -> u8 {
    if !on_linux {
        let _ = writeln!(err, "hermit-agent runs only inside the Linux guest VM");
        return 1;
    }
    let (major, minor) = hermit_proto::PROTOCOL_VERSION;
    let _ = writeln!(
        out,
        "hermit-agent {} (control protocol {major}.{minor}): not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_run_outside_linux() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(false, &mut out, &mut err), 1);
        assert!(out.is_empty());
        assert!(String::from_utf8(err).unwrap().contains("only inside the Linux guest"));
    }

    #[test]
    fn reports_its_version_on_linux() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(true, &mut out, &mut err), 0);
        assert!(String::from_utf8(out).unwrap().contains("control protocol 0.1"));
        assert!(err.is_empty());
    }
}
