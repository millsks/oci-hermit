//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Most commands are stubs for now; each one is implemented in a
//! later milestone (see ROADMAP.md).

mod cli;
mod qemu;

use clap::Parser;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const ROADMAP: &str = "https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md";

/// Exit code for a command that exists but is not implemented yet.
const NOT_IMPLEMENTED: u8 = 2;

/// What a command needs from the process it runs in.
struct Env {
    /// The value of `OCI_HERMIT_QEMU_DIR`, if set.
    qemu_dir: Option<OsString>,
    /// This executable's path.
    exe: PathBuf,
}

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let env = Env {
        qemu_dir: std::env::var_os(qemu::DIR_ENV),
        exe: std::env::current_exe().unwrap_or_default(),
    };
    ExitCode::from(run(&cli.command, &env, &mut std::io::stdout(), &mut std::io::stderr()))
}

/// Run one command, writing to the given streams, and return its exit code.
fn run(command: &cli::Command, env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
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
        cli::Command::Machine(cli::MachineCommand::Info) => machine_info(env, out, err),
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

/// `machine info`: where the bundled QEMU is, and that both emulators are the build oci-hermit expects.
fn machine_info(env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let bundle = match qemu::locate(&qemu::candidates(env.qemu_dir.clone(), &env.exe)) {
        Ok(bundle) => bundle,
        Err(e) => {
            let _ = writeln!(err, "oci-hermit: {e}");
            return 1;
        }
    };
    let _ = writeln!(out, "qemu bundle: {}", bundle.display());
    let mut status = 0;
    for arch in qemu::GuestArch::ALL {
        match qemu::verify(&bundle, arch) {
            Ok(emulator) => {
                let _ = writeln!(
                    out,
                    "{:<8} QEMU {} ({}), accel tcg",
                    format!("{:?}:", emulator.arch).to_lowercase(),
                    emulator.version.version,
                    emulator.version.marker.as_deref().unwrap_or_default()
                );
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with(env: &Env, args: &[&str]) -> (u8, String, String) {
        let cli = cli::Cli::try_parse_from(std::iter::once("oci-hermit").chain(args.iter().copied())).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&cli.command, env, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    /// An environment with no QEMU bundle anywhere; `name` keeps each test's scratch directory its own.
    fn no_qemu(name: &str) -> Env {
        let dir = qemu::testing::scratch(name);
        Env {
            qemu_dir: Some(dir.clone().into()),
            exe: dir.join("bin").join("oci-hermit"),
        }
    }

    #[test]
    fn version_prints_crate_and_protocol_versions() {
        let (code, out, err) = run_with(&no_qemu("main-none-1"), &["version"]);
        assert_eq!(code, 0);
        assert!(out.starts_with(concat!("oci-hermit ", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("control protocol 0.1"));
        assert!(err.is_empty());
    }

    #[test]
    fn stub_commands_exit_not_implemented() {
        let (code, out, err) = run_with(&no_qemu("main-none-2"), &["run", "-it", "alpine", "sh"]);
        assert_eq!(code, NOT_IMPLEMENTED);
        assert!(out.is_empty());
        assert!(err.contains("`run` is not implemented yet"));
        assert!(err.contains(ROADMAP));
    }

    #[test]
    fn machine_info_without_a_bundle_fails() {
        let (code, out, err) = run_with(&no_qemu("main-none-3"), &["machine", "info"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("no bundled QEMU found"));
    }

    #[cfg(unix)]
    #[test]
    fn machine_info_reports_both_emulators() {
        let _guard = qemu::testing::exec_lock();
        let dir = qemu::testing::scratch("main-ok");
        qemu::testing::fake_bundle(&dir, "QEMU emulator version 11.1.3 (oci-hermit-qemu-2)", "tcg");
        let env = Env {
            qemu_dir: Some(dir.clone().into()),
            exe: PathBuf::from("/nowhere/bin/oci-hermit"),
        };
        let (code, out, err) = run_with(&env, &["machine", "info"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains(&format!("qemu bundle: {}", dir.display())));
        assert!(out.contains("x86_64:  QEMU 11.1.3 (oci-hermit-qemu-2), accel tcg"));
        assert!(out.contains("aarch64: QEMU 11.1.3 (oci-hermit-qemu-2), accel tcg"));
    }

    #[cfg(unix)]
    #[test]
    fn machine_info_refuses_a_foreign_qemu() {
        let _guard = qemu::testing::exec_lock();
        let dir = qemu::testing::scratch("main-foreign");
        qemu::testing::fake_bundle(&dir, "QEMU emulator version 11.1.3 (Homebrew)", "tcg");
        let env = Env {
            qemu_dir: Some(dir.into()),
            exe: PathBuf::from("/nowhere/bin/oci-hermit"),
        };
        let (code, _, err) = run_with(&env, &["machine", "info"]);
        assert_eq!(code, 1);
        assert!(err.contains("is not the bundled QEMU"));
    }
}
