//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Every command is a stub
//! for now; each one is implemented in a later milestone (see ROADMAP.md).

mod cli;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Version => {
            let (major, minor) = hermit_proto::PROTOCOL_VERSION;
            println!(
                "oci-hermit {} (control protocol {major}.{minor})",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!(
                "oci-hermit: `{}` is not implemented yet; see https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md",
                other.name()
            );
            ExitCode::from(2)
        }
    }
}
