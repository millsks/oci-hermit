//! oci-hermit guest agent.
//!
//! Runs as PID 1 inside the sandbox VM. It will mount the layer disks and 9P
//! shares, start containers with crun, and answer the host tool over the
//! control channel. For now it only exists so the workspace layout is in
//! place; the real agent lands in milestone M1.

use std::process::ExitCode;

fn main() -> ExitCode {
    if cfg!(not(target_os = "linux")) {
        eprintln!("hermit-agent runs only inside the Linux guest VM");
        return ExitCode::from(1);
    }
    let (major, minor) = hermit_proto::PROTOCOL_VERSION;
    println!(
        "hermit-agent {} (control protocol {major}.{minor}): not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
