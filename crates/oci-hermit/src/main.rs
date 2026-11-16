//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Most commands are stubs for now; each one is implemented in a
//! later milestone (see ROADMAP.md).

mod cli;
mod guest;
mod initramfs;
mod qemu;
mod supervisor;

use clap::Parser;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const ROADMAP: &str = "https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md";

/// Exit code for a command that exists but is not implemented yet.
const NOT_IMPLEMENTED: u8 = 2;

/// What a command needs from the process it runs in.
struct Env {
    /// The value of `OCI_HERMIT_QEMU_DIR`, if set.
    qemu_dir: Option<OsString>,
    /// The value of `OCI_HERMIT_GUEST_DIR`, if set.
    guest_dir: Option<OsString>,
    /// This executable's path.
    exe: PathBuf,
    /// Where each sandbox gets a directory for its initramfs and logs.
    run_base: PathBuf,
}

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let env = Env {
        qemu_dir: std::env::var_os(qemu::DIR_ENV),
        guest_dir: std::env::var_os(guest::DIR_ENV),
        exe: std::env::current_exe().unwrap_or_default(),
        run_base: std::env::temp_dir().join("oci-hermit"),
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
        cli::Command::Machine(cli::MachineCommand::Boot(args)) => machine_boot(args, env, out, err),
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
                    format!("{}:", emulator.arch.name()),
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

/// `machine boot`: boot a sandbox until its agent is ready, then stop it.
fn machine_boot(args: &cli::BootArgs, env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let Some(arch) = args.arch.or_else(qemu::GuestArch::host) else {
        let _ = writeln!(
            err,
            "oci-hermit: cannot tell the host architecture; pass --arch x86_64 or --arch aarch64"
        );
        return 1;
    };
    match boot(args, arch, env, out) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "oci-hermit: {e}");
            1
        }
    }
}

fn boot(
    args: &cli::BootArgs,
    arch: qemu::GuestArch,
    env: &Env,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let bundle = qemu::locate(&qemu::candidates(env.qemu_dir.clone(), &env.exe))?;
    qemu::verify(&bundle, arch)?;
    let guest = guest::locate(&guest::candidates(env.guest_dir.clone(), &env.exe), arch)?;
    let dir = supervisor::run_dir(&env.run_base)?;
    let initrd = dir.join("initrd.cpio");
    std::fs::write(&initrd, initramfs::build(&std::fs::read(&guest.agent)?))?;
    let console = dir.join("console.log");
    let qemu_log = dir.join("qemu.log");
    let (binary, qemu_args) = supervisor::qemu_command(&supervisor::Launch {
        bundle: &bundle,
        kernel: &guest.kernel,
        initrd: &initrd,
        console_log: &console,
        spec: supervisor::MachineSpec {
            arch,
            memory_mib: args.memory,
            cpus: args.cpus,
        },
    })?;
    let _ = writeln!(
        out,
        "booting an {} sandbox; console log: {}",
        arch.name(),
        console.display()
    );
    let mut sandbox = supervisor::Sandbox::spawn(&binary, &qemu_args, &qemu_log)?;
    let see_logs =
        |e: supervisor::SupervisorError| format!("{e}; see {} and {}", console.display(), qemu_log.display());
    let timeout = Duration::from_secs(args.timeout);
    let (took, version) = sandbox.handshake(timeout).map_err(see_logs)?;
    let _ = writeln!(
        out,
        "agent ready after {:.1}s (control protocol {}.{})",
        took.as_secs_f64(),
        version.0,
        version.1
    );
    sandbox.shutdown(timeout).map_err(see_logs)?;
    let _ = writeln!(out, "guest powered off; sandbox stopped");
    Ok(())
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
            guest_dir: Some(dir.clone().into()),
            exe: dir.join("bin").join("oci-hermit"),
            run_base: dir.join("run"),
        }
    }

    /// An environment whose QEMU bundle is the fake in `bundle`, with no guest unless `guest` is given.
    #[cfg(unix)]
    fn with_bundle(bundle: &std::path::Path, guest: Option<&std::path::Path>) -> Env {
        Env {
            qemu_dir: Some(bundle.into()),
            guest_dir: guest.map(Into::into),
            exe: PathBuf::from("/nowhere/bin/oci-hermit"),
            run_base: bundle.join("run"),
        }
    }

    #[test]
    fn version_prints_crate_and_protocol_versions() {
        let (code, out, err) = run_with(&no_qemu("main-none-1"), &["version"]);
        assert_eq!(code, 0);
        assert!(out.starts_with(concat!("oci-hermit ", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("control protocol 1.0"));
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
        let env = with_bundle(&dir, None);
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
        let env = with_bundle(&dir, None);
        let (code, _, err) = run_with(&env, &["machine", "info"]);
        assert_eq!(code, 1);
        assert!(err.contains("is not the bundled QEMU"));
    }

    /// A guest directory holding a kernel and agent for both architectures.
    #[cfg(unix)]
    fn fake_guest(name: &str) -> PathBuf {
        let dir = qemu::testing::scratch(name);
        for arch in qemu::GuestArch::ALL {
            let arch_dir = dir.join(arch.name());
            std::fs::create_dir_all(&arch_dir).unwrap();
            std::fs::write(arch_dir.join("kernel"), "kernel").unwrap();
            std::fs::write(arch_dir.join("hermit-agent"), "agent").unwrap();
        }
        dir
    }

    #[cfg(unix)]
    #[test]
    fn machine_boot_waits_for_the_agent_then_stops() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-boot");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-2)", "tcg");
        let guest = fake_guest("main-boot-guest");
        let env = with_bundle(&bundle, Some(&guest));
        for arch in ["aarch64", "x86_64"] {
            let (code, out, err) = run_with(&env, &["machine", "boot", "--arch", arch, "--timeout", "10"]);
            assert_eq!((code, err.as_str()), (0, ""), "{arch}: {out}");
            assert!(out.contains(&format!("booting an {arch} sandbox")));
            assert!(out.contains("agent ready after"));
            assert!(out.contains("(control protocol 1.0)"));
            assert!(out.contains("guest powered off; sandbox stopped"));
        }
        // Each boot got its own directory with the initramfs it built from the agent.
        let runs: Vec<_> = std::fs::read_dir(bundle.join("run")).unwrap().collect();
        assert_eq!(runs.len(), 2);
        let run = runs[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::read(run.join("initrd.cpio")).unwrap(),
            initramfs::build(b"agent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn machine_boot_without_a_guest_names_the_variable() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-boot-noguest");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-2)", "tcg");
        let env = with_bundle(&bundle, None);
        let (code, _, err) = run_with(&env, &["machine", "boot", "--arch", "aarch64"]);
        assert_eq!(code, 1);
        assert!(err.contains("no aarch64 guest kernel and agent found"));
        assert!(err.contains(guest::DIR_ENV));
    }

    #[cfg(unix)]
    #[test]
    fn machine_boot_reports_a_guest_that_never_gets_ready() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-boot-slow");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-2)", "tcg");
        // Overwrite the emulator with one that exits instead of answering.
        let x86 = bundle.join("bin").join(qemu::GuestArch::X86_64.binary_name());
        let script = std::fs::read_to_string(&x86).unwrap();
        let start = script.find("*)").unwrap();
        std::fs::write(&x86, format!("{}*) exit 3 ;;\nesac\n", &script[..start])).unwrap();
        let guest = fake_guest("main-boot-slow-guest");
        let env = with_bundle(&bundle, Some(&guest));
        let (code, _, err) = run_with(&env, &["machine", "boot", "--arch", "x86_64", "--timeout", "10"]);
        assert_eq!(code, 1);
        assert!(err.contains("QEMU exited before the agent answered"), "{err}");
        assert!(err.contains("console.log"));
    }

    #[test]
    fn machine_boot_without_a_bundle_fails() {
        let (code, _, err) = run_with(&no_qemu("main-none-4"), &["machine", "boot", "--arch", "x86_64"]);
        assert_eq!(code, 1);
        assert!(err.contains("no bundled QEMU found"));
    }
}
