//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Most commands are stubs for now; each one is implemented in a
//! later milestone (see ROADMAP.md).

mod cli;
mod guest;
mod image;
mod initramfs;
mod qemu;
mod registry;
mod rootfs;
mod store;
mod supervisor;
mod users;

use clap::Parser;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const ROADMAP: &str = "https://github.com/millsks/oci-hermit/blob/main/ROADMAP.md";

/// Exit code for a command that exists but is not implemented yet.
const NOT_IMPLEMENTED: u8 = 2;

/// Set to anything but `0` or empty to make `run` report how long each phase took on stderr.
const TIMINGS_ENV: &str = "OCI_HERMIT_TIMINGS";

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
    /// Whether `run` reports its phase timings (`OCI_HERMIT_TIMINGS`).
    timings: bool,
    /// Where the image store lives; `None` when there is no home or data folder to put it in.
    data_dir: Option<PathBuf>,
    /// Auth files and plain-HTTP registries for pulls.
    registry: registry::Settings,
}

/// How long each phase of `run` took, in order.
struct Timings {
    start: Instant,
    last: Instant,
    phases: Vec<(&'static str, Duration)>,
}

impl Timings {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
            phases: Vec::new(),
        }
    }

    /// Ends the current phase, naming it.
    fn mark(&mut self, phase: &'static str) {
        let now = Instant::now();
        self.phases.push((phase, now - self.last));
        self.last = now;
    }

    fn report(&self) -> String {
        format_timings(&self.phases, self.last - self.start)
    }
}

/// One line such as `timings: prepare 0.40s, boot 2.10s, total 2.50s`.
fn format_timings(phases: &[(&str, Duration)], total: Duration) -> String {
    let mut line = String::from("timings:");
    for (phase, took) in phases.iter().chain(std::iter::once(&("total", total))) {
        line.push_str(&format!(" {phase} {:.2}s,", took.as_secs_f64()));
    }
    line.pop();
    line
}

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let env = Env {
        qemu_dir: std::env::var_os(qemu::DIR_ENV),
        guest_dir: std::env::var_os(guest::DIR_ENV),
        exe: std::env::current_exe().unwrap_or_default(),
        run_base: std::env::temp_dir().join("oci-hermit"),
        timings: std::env::var_os(TIMINGS_ENV).is_some_and(|v| !v.is_empty() && v != "0"),
        data_dir: store::data_dir(std::env::var_os(store::DIR_ENV), |name| std::env::var_os(name)),
        registry: registry::Settings::from_env(|name| std::env::var_os(name)),
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
        cli::Command::Run(args) => run_container(args, env, out, err),
        cli::Command::Pull { platform, image } => match pull_image(image, platform.as_deref(), env, out) {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
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

/// The guest architecture for `--platform`, or the host's.
fn guest_arch(platform: Option<&str>) -> Result<qemu::GuestArch, String> {
    match platform {
        Some(platform) => platform_arch(platform).ok_or_else(|| format!("unsupported --platform {platform}")),
        None => qemu::GuestArch::host().ok_or_else(|| "cannot tell the host architecture; pass --platform".into()),
    }
}

fn open_store(env: &Env) -> Result<store::Store, String> {
    let dir = env.data_dir.as_ref().ok_or_else(|| {
        format!(
            "cannot tell where to keep images: no home folder; set {}",
            store::DIR_ENV
        )
    })?;
    store::Store::open(dir).map_err(|e| format!("the image store in {}: {e}", dir.display()))
}

/// `pull`: fetch an image for one platform into the store, verifying every digest.
fn pull_image(
    image: &str,
    platform: Option<&str>,
    env: &Env,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let arch = guest_arch(platform)?;
    let reference = registry::normalize(image)?;
    let store = open_store(env)?;
    let pulled = registry::pull(&reference, image::oci_arch(arch), &store, &env.registry, out)?;
    let _ = writeln!(
        out,
        "Status: pulled {} for linux/{}",
        pulled.name,
        image::oci_arch(arch)
    );
    Ok(())
}

/// The image `run` names: a local layout or archive, else a reference in the store, pulled first if missing.
fn open_image(
    name: &str,
    arch: qemu::GuestArch,
    env: &Env,
    err: &mut impl Write,
) -> Result<image::Image, Box<dyn std::error::Error>> {
    let path = PathBuf::from(name);
    if path.exists() {
        return Ok(image::Image::open(&path, arch)?);
    }
    let reference = registry::normalize(name)?;
    let store = open_store(env)?;
    let full = reference.whole();
    if !store.has(&full, "linux", image::oci_arch(arch)) {
        let _ = writeln!(err, "Unable to find image '{name}' locally");
        registry::pull(&reference, image::oci_arch(arch), &store, &env.registry, err)?;
    }
    Ok(image::Image::open_named(store.path(), Some(&full), arch)?)
}

/// `--memory` as MiB: a number with an optional `k`, `m` or `g` suffix (bytes when there is none, as Docker).
fn parse_memory(text: &str) -> Option<u32> {
    let text = text.trim().to_ascii_lowercase();
    let (digits, scale) = match text.strip_suffix(['b']).unwrap_or(&text) {
        t if t.ends_with('g') => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        t if t.ends_with('m') => (&t[..t.len() - 1], 1024 * 1024),
        t if t.ends_with('k') => (&t[..t.len() - 1], 1024),
        t => (t, 1),
    };
    let bytes = digits.parse::<u64>().ok()?.checked_mul(scale)?;
    u32::try_from(bytes / (1024 * 1024)).ok().filter(|&mib| mib >= 64)
}

/// Memory for a sandbox holding `rootfs_bytes` of image files: the initramfs keeps a compressed-free copy of them
/// and the guest unpacks another, so twice that, plus room for the kernel, the agent and the command.
fn memory_for(rootfs_bytes: u64) -> u32 {
    let mib = rootfs_bytes.div_ceil(1024 * 1024);
    u32::try_from(256 + 2 * mib).unwrap_or(u32::MAX).max(512)
}

/// The guest architecture `--platform` names, e.g. `linux/arm64`.
fn platform_arch(platform: &str) -> Option<qemu::GuestArch> {
    let arch = platform.strip_prefix("linux/").unwrap_or(platform);
    match arch.split('/').next()? {
        "amd64" | "x86_64" => Some(qemu::GuestArch::X86_64),
        "arm64" | "aarch64" => Some(qemu::GuestArch::Aarch64),
        _ => None,
    }
}

/// The host terminal in raw mode for as long as this lives.
struct RawTerminal;

impl RawTerminal {
    fn enable() -> Option<RawTerminal> {
        crossterm::terminal::enable_raw_mode().ok().map(|()| RawTerminal)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// `run`: run a command from a local image in a fresh sandbox, and exit with its exit code.
fn run_container(args: &cli::RunArgs, env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match run_in_sandbox(args, env, out, err) {
        Ok(code) => u8::try_from(code.rem_euclid(256)).unwrap_or(1),
        Err(e) => {
            let _ = writeln!(err, "oci-hermit: {e}");
            125
        }
    }
}

/// The first `run` flag given that is accepted but not implemented yet, as an error naming the issue that adds it.
fn not_supported_yet(args: &cli::RunArgs) -> Option<String> {
    let given = [
        (args.detach, "-d/--detach", 22),
        (args.name.is_some(), "--name", 20),
        (!args.publish.is_empty(), "-p/--publish", 27),
        (!args.volume.is_empty(), "-v/--volume", 33),
        (args.pod.is_some(), "--pod", 35),
    ];
    given
        .iter()
        .find(|(set, ..)| *set)
        .map(|(_, flag, issue)| format!("{flag} is not supported yet (#{issue}); run it without {flag}"))
}

fn run_in_sandbox(
    args: &cli::RunArgs,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut timings = Timings::new();
    if let Some(refusal) = not_supported_yet(args) {
        return Err(refusal.into());
    }
    let arch = guest_arch(args.platform.as_deref())?;
    let bundle = qemu::locate(&qemu::candidates(env.qemu_dir.clone(), &env.exe))?;
    qemu::verify(&bundle, arch)?;
    let guest = guest::locate(&guest::candidates(env.guest_dir.clone(), &env.exe), arch)?;
    let image = open_image(&args.image, arch, env, err)?;
    let rootfs = image.rootfs()?;
    let user_spec = args.user.as_deref().or(image.config.user.as_deref()).unwrap_or("");
    let identity = users::resolve(user_spec, &rootfs)?;
    let argv = image.config.argv(&args.command);
    if argv.is_empty() {
        return Err("the image has no entrypoint or command, and none was given".into());
    }
    let memory = match &args.memory {
        Some(text) => {
            parse_memory(text).ok_or_else(|| format!("--memory {text}: use e.g. 512m or 2g, at least 64m"))?
        }
        None => memory_for(rootfs.content_bytes()),
    };
    let dir = supervisor::run_dir(&env.run_base)?;
    let initrd = dir.join("initrd.cpio");
    std::fs::write(
        &initrd,
        initramfs::build_with_rootfs(&std::fs::read(&guest.agent)?, &rootfs),
    )?;
    let console = dir.join("console.log");
    let qemu_log = dir.join("qemu.log");
    let (binary, qemu_args) = supervisor::qemu_command(&supervisor::Launch {
        bundle: &bundle,
        kernel: &guest.kernel,
        initrd: &initrd,
        console_log: &console,
        spec: supervisor::MachineSpec {
            arch,
            memory_mib: memory,
            cpus: args.cpus.unwrap_or(1),
        },
    })?;
    let see_logs =
        |e: supervisor::SupervisorError| format!("{e}; see {} and {}", console.display(), qemu_log.display());
    timings.mark("prepare");
    let mut sandbox = supervisor::Sandbox::spawn(&binary, &qemu_args, &qemu_log)?;
    let (_, version) = sandbox.handshake(Duration::from_secs(120)).map_err(see_logs)?;
    timings.mark("boot");
    // An agent older than 1.2 ignores the user and would run the command as root.
    if !identity.is_root() && version < (1, 2) {
        return Err(format!(
            "the guest agent speaks control protocol {}.{} and cannot run a command as a user; it needs 1.2 or later",
            version.0, version.1
        )
        .into());
    }

    let mut command_env = image.config.env.clone().unwrap_or_default();
    if !command_env.iter().any(|e| e.starts_with("HOME=")) {
        command_env.push(format!("HOME={}", identity.home));
    }
    if args.tty && !command_env.iter().any(|e| e.starts_with("TERM=")) {
        command_env.push(format!(
            "TERM={}",
            std::env::var("TERM").unwrap_or_else(|_| "xterm".into())
        ));
    }
    let command = supervisor::Command {
        argv,
        env: command_env,
        cwd: image.config.working_dir(),
        tty: args.tty,
        size: args
            .tty
            .then(crossterm::terminal::size)
            .and_then(Result::ok)
            .map(|(cols, rows)| (rows, cols)),
        user: (!identity.is_root()).then(|| hermit_proto::User {
            uid: identity.uid,
            gid: identity.gid,
            groups: identity.groups.clone(),
        }),
    };
    let stdin = args.interactive.then(|| {
        let (chunks, input) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut stdin, &mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = chunks.send(None);
                        return;
                    }
                    Ok(n) => {
                        if chunks.send(Some(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        input
    });
    let raw = (args.tty && std::io::IsTerminal::is_terminal(&std::io::stdin()))
        .then(RawTerminal::enable)
        .flatten();
    let code = sandbox.run(&command, stdin.as_ref(), out, err, Duration::from_secs(60));
    drop(raw);
    let code = code.map_err(see_logs)?;
    timings.mark("command");
    sandbox.shutdown(Duration::from_secs(30)).map_err(see_logs)?;
    timings.mark("stop");
    if env.timings {
        let _ = writeln!(err, "{}", timings.report());
    }
    Ok(code)
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
            timings: false,
            data_dir: Some(dir.join("data")),
            registry: registry::Settings::default(),
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
            timings: false,
            data_dir: Some(bundle.join("data")),
            registry: registry::Settings::default(),
        }
    }

    #[test]
    fn version_prints_crate_and_protocol_versions() {
        let (code, out, err) = run_with(&no_qemu("main-none-1"), &["version"]);
        assert_eq!(code, 0);
        assert!(out.starts_with(concat!("oci-hermit ", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("control protocol 1.2"));
        assert!(err.is_empty());
    }

    #[test]
    fn stub_commands_exit_not_implemented() {
        let (code, out, err) = run_with(&no_qemu("main-none-2"), &["ps", "-a"]);
        assert_eq!(code, NOT_IMPLEMENTED);
        assert!(out.is_empty());
        assert!(err.contains("`ps` is not implemented yet"));
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
        qemu::testing::fake_bundle(&dir, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let env = with_bundle(&dir, None);
        let (code, out, err) = run_with(&env, &["machine", "info"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains(&format!("qemu bundle: {}", dir.display())));
        assert!(out.contains("x86_64:  QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg"));
        assert!(out.contains("aarch64: QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg"));
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
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-boot-guest");
        let env = with_bundle(&bundle, Some(&guest));
        for arch in ["aarch64", "x86_64"] {
            let (code, out, err) = run_with(&env, &["machine", "boot", "--arch", arch, "--timeout", "10"]);
            assert_eq!((code, err.as_str()), (0, ""), "{arch}: {out}");
            assert!(out.contains(&format!("booting an {arch} sandbox")));
            assert!(out.contains("agent ready after"));
            assert!(
                out.contains("(control protocol 1.2)"),
                "the fake agent speaks 1.0: {out}"
            );
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
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
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
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
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

    #[cfg(unix)]
    #[test]
    fn run_relays_output_and_exits_with_the_commands_code() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-run");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-run-guest");
        let config = r#"{"architecture":"ARCH","config":{"Cmd":["bash"],"Env":["PATH=/bin"],"WorkingDir":"/srv"}}"#;
        let layout = image::testing::layout(
            &qemu::testing::scratch("main-run-image"),
            &[("amd64", image::testing::layer())],
            config,
        );
        let env = with_bundle(&bundle, Some(&guest));
        let image = layout.to_str().unwrap();
        let (code, out, err) = run_with(
            &env,
            &["run", "--platform", "linux/amd64", image, "sh", "-c", "echo hi"],
        );
        assert_eq!(code, 3, "{err}");
        assert_eq!(out, "hi\n");
        assert_eq!(err, "oops\n");
        let seen: hermit_proto::HostMessage =
            hermit_proto::decode(&std::fs::read_to_string(bundle.join("run.seen")).unwrap()).unwrap();
        assert_eq!(
            seen,
            hermit_proto::HostMessage::Run {
                argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
                env: vec!["PATH=/bin".into(), "HOME=/".into()],
                cwd: "/srv".into(),
                tty: false,
                rows: None,
                cols: None,
                user: None,
            }
        );
        // The initramfs holds the agent and the image's files under rootfs/.
        let run = std::fs::read_dir(bundle.join("run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let names: Vec<_> = initramfs::testing::parse(&std::fs::read(run.join("initrd.cpio")).unwrap())
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"init".to_string()));
        assert!(names.contains(&"rootfs/bin/sh".to_string()), "{names:?}");
    }

    #[test]
    fn run_explains_what_it_cannot_do_yet() {
        let env = no_qemu("main-run-none");
        let (code, _, err) = run_with(&env, &["run", "debian:bookworm-slim"]);
        assert_eq!(code, 125);
        assert!(err.contains("no bundled QEMU found"), "{err}");
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/s390x", "."]);
        assert_eq!(code, 125);
        assert!(err.contains("unsupported --platform linux/s390x"));
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/arm64", "."]);
        assert_eq!(code, 125);
        assert!(err.contains("no bundled QEMU found"));
    }

    #[test]
    fn run_refuses_flags_it_does_not_implement_yet() {
        let env = no_qemu("main-run-flags");
        for (args, flag, issue) in [
            (&["-d"][..], "-d/--detach", "#22"),
            (&["--name", "web"], "--name", "#20"),
            (&["-p", "8080:80"], "-p/--publish", "#27"),
            (&["-v", "./src:/src"], "-v/--volume", "#33"),
            (&["--pod", "dev"], "--pod", "#35"),
        ] {
            let argv: Vec<&str> = std::iter::once("run")
                .chain(args.iter().copied())
                .chain(["."])
                .collect();
            let (code, out, err) = run_with(&env, &argv);
            assert_eq!(code, 125, "{args:?}");
            assert!(out.is_empty());
            assert_eq!(
                err,
                format!("oci-hermit: {flag} is not supported yet ({issue}); run it without {flag}\n")
            );
        }
        // --rm is what every run does already; it gets as far as looking for QEMU.
        let (code, _, err) = run_with(&env, &["run", "--rm", "--platform", "linux/arm64", "."]);
        assert_eq!(code, 125);
        assert!(err.contains("no bundled QEMU found"), "{err}");
    }

    /// Runs `args` against an image whose config sets `User` to `image_user`, with a fake agent speaking `agent`;
    /// returns the exit code, stderr and the `Run` message the agent saw, if any.
    #[cfg(unix)]
    fn run_as(
        name: &str,
        image_user: &str,
        agent: &str,
        args: &[&str],
    ) -> (u8, String, Option<hermit_proto::HostMessage>) {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch(name);
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        std::fs::write(bundle.join("agent.version"), agent).unwrap();
        let guest = fake_guest(&format!("{name}-guest"));
        let layer = rootfs::testing::Layer::new()
            .dir("etc")
            .file(
                "etc/passwd",
                b"root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000::/home/app:/bin/sh\n",
            )
            .file("etc/group", b"root:x:0:\napp:x:1000:\nvideo:x:44:app\n")
            .bytes();
        let config = format!(r#"{{"architecture":"ARCH","config":{{"Cmd":["id"],"User":"{image_user}"}}}}"#);
        let layout = image::testing::layout(
            &qemu::testing::scratch(&format!("{name}-image")),
            &[("amd64", layer)],
            &config,
        );
        let argv: Vec<&str> = ["run", "--platform", "linux/amd64"]
            .into_iter()
            .chain(args.iter().copied())
            .chain([layout.to_str().unwrap()])
            .collect();
        let (code, _, err) = run_with(&with_bundle(&bundle, Some(&guest)), &argv);
        let seen = std::fs::read_to_string(bundle.join("run.seen"))
            .ok()
            .map(|line| hermit_proto::decode(&line).unwrap());
        (code, err, seen)
    }

    #[test]
    #[cfg(unix)]
    fn run_runs_as_the_images_user_or_the_one_given() {
        let user = |seen: Option<hermit_proto::HostMessage>| match seen {
            Some(hermit_proto::HostMessage::Run { user, env, .. }) => (user, env),
            other => panic!("{other:?}"),
        };
        let (code, err, seen) = run_as("main-user-image", "app", "1,2", &[]);
        assert_eq!(code, 3, "{err}");
        let (who, env) = user(seen);
        assert_eq!(
            who,
            Some(hermit_proto::User {
                uid: 1000,
                gid: 1000,
                groups: vec![44]
            })
        );
        assert_eq!(env, ["HOME=/home/app"]);

        // -u wins over the image; root is sent as no user at all.
        let (code, err, seen) = run_as("main-user-flag", "app", "1,2", &["-u", "root"]);
        assert_eq!(code, 3, "{err}");
        assert_eq!(user(seen), (None, vec!["HOME=/root".to_string()]));
        let (_, _, seen) = run_as("main-user-numeric", "", "1,2", &["--user", "4242:44"]);
        assert_eq!(user(seen).0.map(|u| (u.uid, u.gid)), Some((4242, 44)));
    }

    #[test]
    #[cfg(unix)]
    fn run_refuses_users_it_cannot_honour() {
        // A name the image does not have fails before anything boots.
        let (code, err, seen) = run_as("main-user-ghost", "ghost", "1,2", &[]);
        assert_eq!(code, 125);
        assert_eq!(err, "oci-hermit: user \"ghost\" is not in the image's /etc/passwd\n");
        assert!(seen.is_none());
        // An agent that predates users would run the command as root, so it is never asked to.
        let (code, err, seen) = run_as("main-user-old-agent", "app", "1,1", &[]);
        assert_eq!(code, 125);
        assert!(
            err.contains("speaks control protocol 1.1 and cannot run a command as a user"),
            "{err}"
        );
        assert!(seen.is_none());
        // Root needs nothing new from the agent.
        let (code, err, _) = run_as("main-user-old-root", "", "1,1", &[]);
        assert_eq!(code, 3, "{err}");
    }

    #[test]
    fn pull_stores_an_image_that_run_then_uses() {
        let address = registry::testing::registry("main-pull-registry", None, false, 0);
        let reference = format!("{address}/test/app:1");
        let env = Env {
            registry: registry::Settings {
                insecure: vec![address.clone()],
                ..registry::Settings::default()
            },
            ..no_qemu("main-pull-env")
        };
        let (code, out, err) = run_with(&env, &["pull", "--platform", "linux/arm64", &reference]);
        assert_eq!(code, 0, "{err}");
        assert!(
            out.ends_with(&format!("Status: pulled {reference} for linux/arm64\n")),
            "{out}"
        );

        let (code, _, err) = run_with(&env, &["pull", "--platform", "linux/s390x", &reference]);
        assert_eq!(code, 1);
        assert_eq!(err, "oci-hermit: unsupported --platform linux/s390x\n");
        let (code, _, err) = run_with(&env, &["pull", "Not A Reference"]);
        assert_eq!(code, 1);
        assert!(err.contains("is not an image reference"), "{err}");
        let nowhere = Env {
            data_dir: None,
            ..no_qemu("main-pull-nowhere")
        };
        let (code, _, err) = run_with(&nowhere, &["pull", &reference]);
        assert_eq!(code, 1);
        assert!(err.contains("cannot tell where to keep images"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn run_pulls_a_missing_image_then_runs_it() {
        let address = registry::testing::registry("main-run-pull-registry", None, false, 0);
        let reference = format!("{address}/test/app:1");
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-run-pull");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-run-pull-guest");
        let env = Env {
            registry: registry::Settings {
                insecure: vec![address.clone()],
                ..registry::Settings::default()
            },
            ..with_bundle(&bundle, Some(&guest))
        };
        let (code, out, err) = run_with(&env, &["run", "--platform", "linux/amd64", &reference]);
        assert_eq!(code, 3, "{err}");
        assert_eq!(out, "hi\n");
        assert!(
            err.starts_with(&format!("Unable to find image '{reference}' locally\n")),
            "{err}"
        );
        let seen = std::fs::read_to_string(bundle.join("run.seen")).unwrap();
        assert!(seen.contains("from-the-registry"), "{seen}");

        // The second run finds it in the store.
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/amd64", &reference]);
        assert_eq!(code, 3);
        assert_eq!(err, "oops\n");
    }

    #[test]
    fn timings_report_each_phase_and_the_total() {
        let phases = [
            ("prepare", Duration::from_millis(400)),
            ("boot", Duration::from_millis(2105)),
        ];
        assert_eq!(
            format_timings(&phases, Duration::from_millis(2505)),
            "timings: prepare 0.40s, boot 2.10s, total 2.50s"
        );
        let mut timings = Timings::new();
        timings.mark("prepare");
        timings.mark("boot");
        assert!(
            timings.report().starts_with("timings: prepare "),
            "{}",
            timings.report()
        );
        assert!(timings.report().contains(", boot "));
    }

    #[test]
    #[cfg(unix)]
    fn run_reports_timings_when_asked() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-run-timings");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-run-timings-guest");
        let layout = image::testing::layout(
            &qemu::testing::scratch("main-run-timings-image"),
            &[("amd64", image::testing::layer())],
            r#"{"architecture":"ARCH","config":{"Cmd":["true"]}}"#,
        );
        let env = Env {
            timings: true,
            ..with_bundle(&bundle, Some(&guest))
        };
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/amd64", layout.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        let line = err.lines().last().unwrap();
        for phase in ["timings: prepare ", ", boot ", ", command ", ", stop ", ", total "] {
            assert!(line.contains(phase), "{line}");
        }
    }

    #[test]
    fn memory_and_platform_parse_like_docker() {
        assert_eq!(parse_memory("512m"), Some(512));
        assert_eq!(parse_memory("2g"), Some(2048));
        assert_eq!(parse_memory("2GB"), Some(2048));
        assert_eq!(parse_memory("1048576k"), Some(1024));
        assert_eq!(parse_memory("536870912"), Some(512));
        assert_eq!(parse_memory("32m"), None, "too small to boot");
        assert_eq!(parse_memory("lots"), None);
        assert_eq!(memory_for(0), 512);
        assert_eq!(memory_for(300 * 1024 * 1024), 856);
        assert_eq!(platform_arch("linux/arm64/v8"), Some(qemu::GuestArch::Aarch64));
        assert_eq!(platform_arch("amd64"), Some(qemu::GuestArch::X86_64));
        assert_eq!(platform_arch("linux/riscv64"), None);
    }
}
