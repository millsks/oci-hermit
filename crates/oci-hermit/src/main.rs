//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Every command is a stub
//! for now; each one is implemented in a later milestone (see ROADMAP.md).

mod cli;

use clap::Parser;
use std::io::Write;
use std::process::ExitCode;

const ROADMAP: &str = "https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md";

/// Exit code for a command that exists but is not implemented yet.
const NOT_IMPLEMENTED: u8 = 2;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    ExitCode::from(run(&cli.command, &mut std::io::stdout(), &mut std::io::stderr()))
}

/// Run one command, writing to the given streams, and return its exit code.
fn run(command: &cli::Command, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match command {
        cli::Command::Version => {
            let (major, minor) = hermit_proto::PROTOCOL_VERSION;
            let _ = writeln!(
                out,
                "oci-hermit {} (control protocol {major}.{minor})",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
        other => {
            let _ = writeln!(
                err,
                "oci-hermit: `{}` is not implemented yet; see {ROADMAP}",
                other.name()
            );
            NOT_IMPLEMENTED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> (u8, String, String) {
        let cli = cli::Cli::try_parse_from(std::iter::once("oci-hermit").chain(args.iter().copied())).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&cli.command, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn version_prints_crate_and_protocol_versions() {
        let (code, out, err) = run_args(&["version"]);
        assert_eq!(code, 0);
        assert!(out.starts_with(concat!("oci-hermit ", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("control protocol 0.1"));
        assert!(err.is_empty());
    }

    #[test]
    fn stub_commands_exit_not_implemented() {
        let (code, out, err) = run_args(&["run", "-it", "alpine", "sh"]);
        assert_eq!(code, NOT_IMPLEMENTED);
        assert!(out.is_empty());
        assert!(err.contains("`run` is not implemented yet"));
        assert!(err.contains(ROADMAP));
    }
}
