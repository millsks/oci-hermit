//! oci-hermit host tool.
//!
//! The command surface mirrors Docker and Podman. Most commands are stubs for now; each one is implemented in a
//! later milestone (see ROADMAP.md).

mod certs;
mod cli;
mod containers;
mod erofs;
mod guest;
mod image;
mod images;
mod initramfs;
mod ipc;
mod load;
mod login;
mod monitor;
mod qemu;
mod reference;
mod registry;
mod rootfs;
mod store;
mod supervisor;
mod users;

use clap::{CommandFactory, FromArgMatches};
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
    /// Whether `data_dir` came from `OCI_HERMIT_DATA_DIR` rather than the platform's default.
    data_dir_from_env: bool,
    /// Auth files and plain-HTTP registries for pulls.
    registry: registry::Settings,
    /// Reads a yes or no after a question has been printed, for `image prune`.
    confirm: fn() -> bool,
    /// Asks the user for something `login` needs.
    ask: fn(Ask) -> std::io::Result<String>,
    /// Starts a container's monitor: this executable, the data folder, the container's ID, its log.
    start_monitor: fn(&std::path::Path, &std::path::Path, &str, &std::path::Path) -> std::io::Result<monitor::Process>,
}

/// What `login` asks for when it is not on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    Username,
    /// Without echo, on the terminal.
    Password,
    /// All of stdin, for `--password-stdin`.
    Stdin,
}

/// Asks on the terminal: the user name with echo, the password without.
fn ask_on_terminal(what: Ask) -> std::io::Result<String> {
    use std::io::{BufRead, IsTerminal, Read};
    if what == Ask::Stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        return Ok(text.trim_end_matches(['\r', '\n']).to_string());
    }
    if !std::io::stdin().is_terminal() {
        return Err(std::io::Error::other(
            "there is no terminal to ask on; pass --username, and --password-stdin with the password on stdin",
        ));
    }
    let mut err = std::io::stderr();
    if what == Ask::Username {
        write!(err, "Username: ")?;
        err.flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
    write!(err, "Password: ")?;
    err.flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let typed = read_secret(&mut std::io::stdin().lock());
    let _ = crossterm::terminal::disable_raw_mode();
    writeln!(err)?;
    typed
}

/// Reads a line typed in raw mode: Enter ends it, Backspace removes a character, Ctrl-C cancels.
fn read_secret(input: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while input.read(&mut byte)? == 1 {
        match byte[0] {
            b'\r' | b'\n' => break,
            3 => return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")),
            8 | 0x7f => {
                // A whole character, not just its last byte.
                while bytes.pop().is_some_and(|b| b & 0xC0 == 0x80) {}
            }
            b => bytes.push(b),
        }
    }
    String::from_utf8(bytes).map_err(std::io::Error::other)
}

/// A yes from stdin: `y` or `yes`, in any case.
fn confirm_on_stdin() -> bool {
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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

/// The name oci-hermit was started as, for its help text: `hrmt` (#44), else `oci-hermit`.
fn invoked_name(argv0: Option<&std::ffi::OsStr>) -> &'static str {
    let stem = argv0.and_then(|a| std::path::Path::new(a).file_stem());
    if stem.is_some_and(|s| s.eq_ignore_ascii_case("hrmt")) {
        "hrmt"
    } else {
        "oci-hermit"
    }
}

/// This executable, through any symlink (a `hrmt` link put in `~/bin`, say), so `qemu/` and `guest/` are found
/// beside the real file. Windows paths stay as they are: canonical ones there take the `\\?\` form, which QEMU's
/// options do not expect, and the Windows package ships `hrmt.exe` as a copy, not a link.
fn real_exe(exe: std::io::Result<PathBuf>) -> PathBuf {
    let exe = exe.unwrap_or_default();
    if cfg!(unix) {
        exe.canonicalize().unwrap_or(exe)
    } else {
        exe
    }
}

fn main() -> ExitCode {
    let name = invoked_name(std::env::args_os().next().as_deref());
    let matches = cli::Cli::command().name(name).bin_name(name).get_matches();
    let cli = cli::Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let env = Env {
        qemu_dir: std::env::var_os(qemu::DIR_ENV),
        guest_dir: std::env::var_os(guest::DIR_ENV),
        exe: real_exe(std::env::current_exe()),
        run_base: std::env::temp_dir().join("oci-hermit"),
        timings: std::env::var_os(TIMINGS_ENV).is_some_and(|v| !v.is_empty() && v != "0"),
        data_dir: store::data_dir(std::env::var_os(store::DIR_ENV), |name| std::env::var_os(name)),
        data_dir_from_env: std::env::var_os(store::DIR_ENV).is_some_and(|d| !d.is_empty()),
        registry: registry::Settings::from_env(|name| std::env::var_os(name)),
        confirm: confirm_on_stdin,
        ask: ask_on_terminal,
        start_monitor: monitor::spawn,
    };
    ExitCode::from(run(&cli.command, &env, &mut std::io::stdout(), &mut std::io::stderr()))
}

/// Run one command, writing to the given streams, and return its exit code.
fn run(command: &cli::Command, env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match command {
        cli::Command::Version => {
            let _ = writeln!(out, "{}", version_line());
            0
        }
        cli::Command::Machine(cli::MachineCommand::Info) | cli::Command::Info => machine_info(env, out, err),
        cli::Command::Images(_)
        | cli::Command::Rmi(_)
        | cli::Command::Inspect(_)
        | cli::Command::Tag(_)
        | cli::Command::Save(_)
        | cli::Command::Image(_)
        | cli::Command::System(_) => match image_command(command, env, out, err) {
            Ok(code) => code,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Machine(cli::MachineCommand::Boot(args)) => machine_boot(args, env, out, err),
        cli::Command::Run(args) => run_container(args, env, out, err),
        cli::Command::Ps { all, quiet, no_trunc } => match open_containers(env)
            .map_err(containers::ContainerError::Invalid)
            .and_then(|c| containers::ps(&c, *all, *quiet, *no_trunc, images::now(), out))
        {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Rm {
            containers: wanted,
            force,
        } => match open_containers(env) {
            Ok(c) => {
                let data_dir = env.data_dir.as_deref().expect("the container store opened");
                // As Docker's rm -f: killed at once, with no grace period.
                let stop = |id: &str| monitor::stop(&c, data_dir, id, Some(0), None);
                containers::rm(&c, wanted, force.then_some(&stop as containers::Stopper<'_>), out, err)
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Stop {
            time,
            signal,
            containers: wanted,
        } => match open_containers(env)
            .map_err(|e| e.to_string())
            .and_then(|c| signal.as_deref().map(parse_signal).transpose().map(|s| (c, s)))
        {
            Ok((c, signal)) => {
                let data_dir = env.data_dir.as_deref().expect("the container store opened");
                monitor::stop_command(&c, data_dir, wanted, *time, signal, out, err)
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Kill {
            signal,
            containers: wanted,
        } => match open_containers(env)
            .map_err(|e| e.to_string())
            .and_then(|c| parse_signal(signal).map(|s| (c, s)))
        {
            Ok((c, signal)) => {
                let data_dir = env.data_dir.as_deref().expect("the container store opened");
                monitor::kill_command(&c, data_dir, wanted, signal, out, err)
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Logs { container, follow } => match open_containers(env)
            .map_err(Into::into)
            .and_then(|c| monitor::logs(&c, container, *follow, out, err))
        {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Monitor { data_dir, id } => monitor::serve(data_dir, id, err),
        cli::Command::Exec(args) => exec_command(args, env, out, err),
        cli::Command::Load { input } => match open_store(env).map_err(Into::into).and_then(|store| {
            load::load(std::path::Path::new(input), &store, out).map_err(Box::<dyn std::error::Error>::from)
        }) {
            Ok(_) => 0,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Login(args) => match login_command(args, env, out, err) {
            Ok(code) => code,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Logout(args) => match logout_command(args, env, out, err) {
            Ok(code) => code,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                1
            }
        },
        cli::Command::Manifest(cli::ManifestCommand::Inspect(args)) => {
            let shown = registry::normalize(&args.image)
                .map_err(registry::PullError::Registry)
                .and_then(|reference| {
                    let settings = registry::Settings {
                        cert_dir: args.cert_dir.clone(),
                        ..env.registry.clone()
                    };
                    warn_about_certs(&settings, reference.registry(), err);
                    registry::inspect(&reference, &settings, args.verbose)
                });
            match shown {
                Ok(text) => {
                    let _ = out.write_all(text.as_bytes());
                    0
                }
                Err(e) => {
                    let _ = writeln!(err, "oci-hermit: {e}");
                    1
                }
            }
        }
        cli::Command::Pull {
            platform,
            image,
            cert_dir,
        } => match pull_image(image, platform.as_deref(), cert_dir.as_deref(), env, out, err) {
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

/// `oci-hermit 0.2.0 (control protocol 1.4)`.
fn version_line() -> String {
    let (major, minor) = hermit_proto::PROTOCOL_VERSION;
    format!(
        "oci-hermit {} (control protocol {major}.{minor})",
        env!("CARGO_PKG_VERSION")
    )
}

/// Where a setting came from: its environment variable, or the search beside the executable.
fn origin(env_value: Option<&OsString>, found: &std::path::Path, var: &str) -> String {
    if env_value.is_some_and(|v| !v.is_empty() && std::path::Path::new(v) == found) {
        format!("from {var}")
    } else {
        "found beside oci-hermit".into()
    }
}

/// `machine info`: everything a sandbox is made from and where oci-hermit keeps things, as `podman info` does. Exits
/// 1 when the emulators are missing or wrong, since nothing can run then; a missing guest is shown, not an error.
fn machine_info(env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let mut status = 0;
    let mut lines = vec![version_line()];
    let row = |label: &str, value: String| {
        let label = format!("{label}:");
        format!("  {label:<12}{value}")
    };
    let host_arch = qemu::GuestArch::host();
    lines.push(format!(
        "host: {}/{}, so images default to {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        host_arch.map_or("none (pass --platform)".into(), |a| format!(
            "linux/{}",
            image::oci_arch(a)
        ))
    ));

    lines.push(String::new());
    lines.push("emulator (QEMU, software CPU only):".into());
    match qemu::locate(&qemu::candidates(env.qemu_dir.clone(), &env.exe)) {
        Ok(bundle) => {
            lines.push(row(
                "bundle",
                format!(
                    "{} ({})",
                    bundle.display(),
                    origin(env.qemu_dir.as_ref(), &bundle, qemu::DIR_ENV)
                ),
            ));
            for arch in qemu::GuestArch::ALL {
                match qemu::verify(&bundle, arch) {
                    Ok(emulator) => lines.push(row(
                        arch.name(),
                        format!(
                            "QEMU {} ({}), accel tcg",
                            emulator.version.version,
                            emulator.version.marker.as_deref().unwrap_or_default()
                        ),
                    )),
                    Err(e) => {
                        lines.push(row(arch.name(), "unusable (see below)".into()));
                        let _ = writeln!(err, "oci-hermit: {e}");
                        status = 1;
                    }
                }
            }
        }
        Err(e) => {
            lines.push(row("bundle", "not found (see below)".into()));
            let _ = writeln!(err, "oci-hermit: {e}");
            status = 1;
        }
    }

    lines.push(String::new());
    lines.push("guest (kernel and agent each sandbox boots):".into());
    let candidates = guest::candidates(env.guest_dir.clone(), &env.exe);
    for arch in qemu::GuestArch::ALL {
        let value = match guest::locate(&candidates, arch) {
            Ok(found) => {
                let len = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
                let dir = found.kernel.parent().unwrap_or(&found.kernel);
                let from = dir.parent().map_or_else(String::new, |d| {
                    format!(", {}", origin(env.guest_dir.as_ref(), d, guest::DIR_ENV))
                });
                format!(
                    "{} (kernel {}, agent {}{from})",
                    dir.display(),
                    registry::size(len(&found.kernel)),
                    registry::size(len(&found.agent))
                )
            }
            Err(_) => format!(
                "not found; {} images cannot run (set {} to a folder with {}/kernel and {}/hermit-agent)",
                image::oci_arch(arch),
                guest::DIR_ENV,
                arch.name(),
                arch.name()
            ),
        };
        lines.push(row(arch.name(), value));
    }

    lines.push(String::new());
    lines.push("sandbox defaults for run:".into());
    lines.push(row("cpus", "1 (--cpus)".into()));
    lines.push(row(
        "memory",
        "512 MiB for a stored image (its layers are disks); twice the size of one run from a file, plus 256 MiB (--memory)".into(),
    ));
    lines.push(row(
        "network",
        "none: no egress; -p (127.0.0.1 only) is not implemented yet".into(),
    ));
    lines.push(row(
        "run dir",
        format!(
            "{} (a sandbox-* folder per run: initramfs, console.log, qemu.log)",
            env.run_base.display()
        ),
    ));

    lines.push(String::new());
    lines.push("image store (pull, load):".into());
    match &env.data_dir {
        Some(data) => {
            let from = if env.data_dir_from_env {
                format!("from {}", store::DIR_ENV)
            } else {
                "the platform default".into()
            };
            lines.push(row("path", format!("{} ({from})", data.join("images").display())));
            match store::usage(data) {
                Some(usage) => {
                    lines.push(row(
                        "images",
                        format!(
                            "{} image{} under {} name{} (one per name and platform)",
                            usage.entries,
                            if usage.entries == 1 { "" } else { "s" },
                            usage.names,
                            if usage.names == 1 { "" } else { "s" }
                        ),
                    ));
                    lines.push(row("size", registry::size(usage.bytes)));
                }
                None => lines.push(row("images", "none yet; created by the first pull or load".into())),
            }
        }
        None => lines.push(row(
            "path",
            format!("none: no home or data folder (set {})", store::DIR_ENV),
        )),
    }

    lines.push(String::new());
    lines.push("registries (pulls happen on the host, never in the sandbox):".into());
    lines.push("  auth files: searched in this order; the first with a login for the registry is used".into());
    for (i, location) in env.registry.auth_locations.iter().enumerate() {
        // A location that is a variable on its own keeps its name, so a value you set can be told from a default.
        let bare_variable = location.name.starts_with('$') && !location.name.contains('/');
        let (state, shown) = match &location.path {
            Some(path) => (
                if path.is_file() { "[found]" } else { "[not found]" },
                if bare_variable {
                    format!("{} = {}", location.name, path.display())
                } else {
                    path.display().to_string()
                },
            ),
            None => ("[not set]", location.name.to_string()),
        };
        lines.push(format!("    {}. {state:<12}{shown}", i + 1));
    }
    lines.push(row(
        "login file",
        login::login_file(&env.registry).map_or_else(
            || "none: no home folder (set REGISTRY_AUTH_FILE)".into(),
            |f| format!("{} (where login writes)", f.display()),
        ),
    ));
    lines.push(row(
        "plain http",
        if env.registry.insecure.is_empty() {
            format!("none ({})", registry::INSECURE_ENV)
        } else {
            format!("{} (from {})", env.registry.insecure.join(", "), registry::INSECURE_ENV)
        },
    ));
    lines.push(
        "  certificates (certs.d): searched in this order; the first with a folder for the registry is used".into(),
    );
    for (i, location) in env.registry.cert_locations.iter().enumerate() {
        let (state, shown) = match &location.path {
            Some(path) => (
                if path.is_dir() { "[found]" } else { "[not found]" },
                path.display().to_string(),
            ),
            None => ("[not set]", location.name.to_string()),
        };
        lines.push(format!("    {}. {state:<12}{shown}", i + 1));
        let Some(path) = location.path.as_ref().filter(|p| p.is_dir()) else {
            continue;
        };
        let mut folders: Vec<PathBuf> = std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        folders.sort();
        for folder in folders {
            let name = folder.file_name().unwrap_or_default().to_string_lossy();
            let held = certs::read_dir(&folder).map_or_else(|e| format!("unusable: {e}"), |c| c.summary());
            lines.push(format!("{:19}{name}: {held}", ""));
        }
    }

    for line in lines {
        let _ = writeln!(out, "{}", line.trim_end());
    }
    status
}

/// The auth file `login` and `logout` change: `--authfile`, else Podman's.
fn login_file(authfile: Option<&str>, env: &Env) -> Result<PathBuf, String> {
    match authfile {
        Some(file) => Ok(PathBuf::from(file)),
        None => login::login_file(&env.registry).ok_or_else(|| {
            "cannot tell where to keep logins: no home folder; set REGISTRY_AUTH_FILE or pass --authfile".into()
        }),
    }
}

/// `login`: asks for what the command line does not give, checks it with the registry, then stores it.
fn login_command(
    args: &cli::LoginArgs,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, Box<dyn std::error::Error>> {
    let registry = login::registry_name(args.registry.as_deref().unwrap_or("docker.io"))?;
    let file = login_file(args.authfile.as_deref(), env)?;
    if args.get_login {
        return Ok(match login::user(&file, &registry) {
            Some(user) => {
                let _ = writeln!(out, "{user}");
                0
            }
            None => {
                let _ = writeln!(err, "oci-hermit: not logged in to {registry}");
                1
            }
        });
    }
    if args.password.is_some() && args.password_stdin {
        return Err("--password and --password-stdin cannot be used together".into());
    }
    if args.password_stdin && args.username.is_none() {
        return Err("--password-stdin needs --username, since stdin holds the password".into());
    }
    let user = match &args.username {
        Some(user) => user.clone(),
        None => (env.ask)(Ask::Username)?,
    };
    let password = match &args.password {
        Some(password) => {
            let _ = writeln!(
                err,
                "WARNING! --password on the command line can be seen by other users; use --password-stdin."
            );
            password.clone()
        }
        None if args.password_stdin => (env.ask)(Ask::Stdin)?,
        None => (env.ask)(Ask::Password)?,
    };
    if user.is_empty() || password.is_empty() {
        return Err("a user name and a password are both needed".into());
    }
    let settings = registry::Settings {
        cert_dir: args.cert_dir.clone(),
        ..env.registry.clone()
    };
    warn_about_certs(&settings, &registry, err);
    login::verify(&registry, &user, &password, &settings)?;
    login::store(&file, &registry, &user, &password)?;
    let _ = writeln!(out, "Login Succeeded!");
    Ok(0)
}

/// `logout`: removes the registry's login (or every login) from the auth file `login` writes.
fn logout_command(
    args: &cli::LogoutArgs,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, Box<dyn std::error::Error>> {
    let file = login_file(args.authfile.as_deref(), env)?;
    if args.all {
        if args.registry.is_some() {
            return Err("give a registry or --all, not both".into());
        }
        login::remove_all(&file)?;
        let _ = writeln!(out, "Removed login credentials for all registries");
        return Ok(0);
    }
    let registry = login::registry_name(args.registry.as_deref().unwrap_or("docker.io"))?;
    if login::remove(&file, &registry)? {
        let _ = writeln!(out, "Removed login credentials for {registry}");
        return Ok(0);
    }
    match login::elsewhere(&env.registry, &file, &registry) {
        Some(other) => {
            let _ = writeln!(
                err,
                "oci-hermit: not logged in to {registry} with oci-hermit or Podman; the login in {} was made by \
                 another tool (`docker logout {registry}` removes it)",
                other.display()
            );
        }
        None => {
            let _ = writeln!(err, "oci-hermit: not logged in to {registry}");
        }
    }
    Ok(1)
}

/// The image commands (`images`, `rmi`, `inspect`, `tag`, `save`, `image ...`, `system df`).
fn image_command(
    command: &cli::Command,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, Box<dyn std::error::Error>> {
    use cli::{Command as C, ImageCommand as I};
    let exclusive = || -> Result<store::Store, Box<dyn std::error::Error>> {
        let dir = env.data_dir.as_ref().ok_or_else(no_data_dir)?;
        store::Store::open_exclusive(dir, std::time::Duration::from_secs(2))
    };
    match command {
        C::Images(args) | C::Image(I::Ls(args)) => {
            let options = images::ListOptions {
                reference: args.reference.as_deref(),
                quiet: args.quiet,
                digests: args.digests,
                no_trunc: args.no_trunc,
            };
            images::list(&open_store(env)?, &options, images::now(), out)?;
        }
        C::Rmi(args) | C::Image(I::Rm(args)) => {
            if !images::remove(&exclusive()?, &args.images, args.force, out, err)? {
                return Ok(1);
            }
        }
        C::Inspect(args) | C::Image(I::Inspect(args)) => {
            let (objects, missing) = images::inspect(&open_store(env)?, &args.images)?;
            let _ = writeln!(out, "{}", serde_json::to_string_pretty(&objects)?);
            for name in &missing {
                let _ = writeln!(err, "oci-hermit: no such image: {name}");
            }
            if !missing.is_empty() {
                return Ok(1);
            }
        }
        C::Tag(args) | C::Image(I::Tag(args)) => images::tag(&open_store(env)?, &args.source, &args.target)?,
        C::Save(args) | C::Image(I::Save(args)) => images::save(
            &open_store(env)?,
            &args.images,
            args.platform.as_deref(),
            std::path::Path::new(&args.output),
        )?,
        C::Image(I::Prune { all, force }) => {
            if !force {
                let what = if *all {
                    "all images"
                } else {
                    "all images loaded without a name"
                };
                let _ = write!(
                    out,
                    "WARNING! This will remove {what}.\nAre you sure you want to continue? [y/N] "
                );
                if !(env.confirm)() {
                    let _ = writeln!(out);
                    return Ok(0);
                }
            }
            images::prune(&exclusive()?, *all, out)?;
        }
        C::Image(I::History { no_trunc, quiet, image }) => {
            images::history(&open_store(env)?, image, *no_trunc, *quiet, images::now(), out)?
        }
        C::Image(I::Exists { image }) => {
            if !images::exists(&open_store(env)?, image)? {
                return Ok(1);
            }
        }
        C::System(cli::SystemCommand::Df) => {
            let store = match &env.data_dir {
                Some(dir) if dir.join("images").is_dir() => Some(store::Store::open(dir)?),
                _ => None,
            };
            images::disk_usage(store.as_ref(), out)?;
        }
        _ => unreachable!("only image commands come here"),
    }
    Ok(0)
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
    std::fs::write(
        &initrd,
        initramfs::build(&std::fs::read(&guest.agent)?, &std::fs::read(&guest.crun)?),
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
            memory_mib: args.memory,
            cpus: args.cpus,
        },
        disks: &[],
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

fn no_data_dir() -> String {
    format!(
        "cannot tell where to keep images: no home folder; set {}",
        store::DIR_ENV
    )
}

fn open_containers(env: &Env) -> Result<containers::Containers, String> {
    let dir = env.data_dir.as_ref().ok_or_else(no_data_dir)?;
    containers::Containers::open(dir).map_err(|e| format!("the container store in {}: {e}", dir.display()))
}

fn open_store(env: &Env) -> Result<store::Store, String> {
    let dir = env.data_dir.as_ref().ok_or_else(no_data_dir)?;
    store::Store::open(dir).map_err(|e| format!("the image store in {}: {e}", dir.display()))
}

/// `pull`: fetch an image for one platform into the store, verifying every digest.
fn pull_image(
    image: &str,
    platform: Option<&str>,
    cert_dir: Option<&std::path::Path>,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let arch = guest_arch(platform)?;
    let reference = registry::normalize(image)?;
    let store = open_store(env)?;
    let settings = registry::Settings {
        cert_dir: cert_dir.map(std::path::Path::to_path_buf),
        ..env.registry.clone()
    };
    warn_about_certs(&settings, reference.registry(), err);
    let pulled = registry::pull(&reference, image::oci_arch(arch), &store, &settings, out)?;
    let _ = writeln!(
        out,
        "Status: pulled {} for linux/{}",
        pulled.name,
        image::oci_arch(arch)
    );
    Ok(())
}

/// Warns about a registry's certificates (a private key other users can read). Problems that stop the pull are
/// reported by the pull itself.
fn warn_about_certs(settings: &registry::Settings, registry: &str, err: &mut impl Write) {
    if let Ok(Some(certs)) = settings.certs_for(registry) {
        for warning in certs.warnings() {
            let _ = writeln!(err, "{warning}");
        }
    }
}

/// The image `run` names, for `requested` or, without `--platform`, for the host's architecture or else the other
/// one (as Docker runs an image built for another platform under emulation): a local layout or archive, else a name
/// or ID in the store, else pulled. Returns the image, the guest architecture it needs, and the store it came from:
/// holding that keeps `rmi` from deleting its blobs before they are read.
fn open_image(
    name: &str,
    requested: Option<qemu::GuestArch>,
    env: &Env,
    err: &mut impl Write,
) -> Result<(image::Image, qemu::GuestArch, Option<store::Store>), Box<dyn std::error::Error>> {
    let choices = match requested {
        Some(arch) => vec![arch],
        None => {
            let host = qemu::GuestArch::host().ok_or("cannot tell the host architecture; pass --platform")?;
            vec![host, other_arch(host)]
        }
    };
    let fall_back = |arch: qemu::GuestArch, err: &mut dyn Write| {
        if arch != choices[0] {
            let _ = writeln!(
                err,
                "WARNING: the image has no linux/{} variant; running its linux/{} variant (choose with --platform)",
                image::oci_arch(choices[0]),
                image::oci_arch(arch)
            );
        }
    };
    let path = PathBuf::from(name);
    if path.exists() {
        for (i, &arch) in choices.iter().enumerate() {
            match image::Image::open(&path, arch) {
                Ok(image) => {
                    fall_back(arch, err);
                    return Ok((image, arch, None));
                }
                Err(image::ImageError::NoPlatform { .. }) if i + 1 < choices.len() => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let store = open_store(env)?;
    // A stored name, the name as Docker spells it out, or an image ID or a prefix of one.
    let stored: Vec<images::Stored> = store
        .entries()?
        .iter()
        .filter_map(|e| images::read(&store, e).ok())
        .collect();
    if let Ok((found, _)) = images::find(&stored, name) {
        for &arch in &choices {
            let wanted = image::oci_arch(arch);
            if let Some(i) = found.iter().find(|i| i.entry.os == "linux" && i.entry.arch == wanted) {
                fall_back(arch, err);
                let image = image::Image::open_named(store.path(), Some(&i.entry.name), arch)?;
                return Ok((image, arch, Some(store)));
            }
        }
    }
    let reference = registry::normalize(name)?;
    let _ = writeln!(err, "Unable to find image '{name}' locally");
    warn_about_certs(&env.registry, reference.registry(), err);
    for (i, &arch) in choices.iter().enumerate() {
        match registry::pull(&reference, image::oci_arch(arch), &store, &env.registry, err) {
            Ok(pulled) => {
                fall_back(arch, err);
                let image = image::Image::open_named(store.path(), Some(&pulled.name), arch)?;
                return Ok((image, arch, Some(store)));
            }
            Err(registry::PullError::NoPlatform(_)) if i + 1 < choices.len() => {}
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("the last choice returns")
}

fn other_arch(arch: qemu::GuestArch) -> qemu::GuestArch {
    match arch {
        qemu::GuestArch::X86_64 => qemu::GuestArch::Aarch64,
        qemu::GuestArch::Aarch64 => qemu::GuestArch::X86_64,
    }
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

/// A signal as `kill -s`, `stop -s` or an image's `StopSignal` names it.
fn parse_signal(name: &str) -> Result<i32, String> {
    hermit_proto::signal_number(name)
        .ok_or_else(|| format!("{name:?} is not a signal: use a name such as TERM or SIGHUP, or a number from 1 to 64"))
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
    if let Some(name) = &args.name {
        containers::check_name(name)?;
    }
    let memory = match &args.memory {
        Some(text) => {
            Some(parse_memory(text).ok_or_else(|| format!("--memory {text}: use e.g. 512m or 2g, at least 64m"))?)
        }
        None => None,
    };
    let requested = match args.platform.as_deref() {
        Some(platform) => Some(guest_arch(Some(platform))?),
        None => None,
    };
    // The bundle first: without it nothing runs, so there is no point pulling an image.
    let bundle = qemu::locate(&qemu::candidates(env.qemu_dir.clone(), &env.exe))?;
    let (image, arch, store) = open_image(&args.image, requested, env, err)?;
    qemu::verify(&bundle, arch)?;
    let guest = guest::locate(&guest::candidates(env.guest_dir.clone(), &env.exe), arch)?;
    // A stored image runs from its layer disks (#17), built once per layer and kept; the guest stacks them with
    // overlayfs. An image opened from a path, or one with more layers than a sandbox takes disks, goes into the
    // initramfs instead, which needs about twice its size in memory.
    let disks = match &store {
        Some(store) if image.layers().len() <= supervisor::MAX_DISKS => Some(
            image
                .layers()
                .iter()
                .map(|(digest, media_type)| store.layer_disk(digest, media_type))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => None,
    };
    let user_spec = args.user.as_deref().or(image.config.user.as_deref()).unwrap_or("");
    // The image's accounts are kept for `exec -u` in the running container.
    let (rootfs, identity, accounts) = match &disks {
        Some(paths) => {
            let layers = erofs::Layers(
                paths
                    .iter()
                    .rev()
                    .map(|p| erofs::reader::Disk::open_path(p))
                    .collect::<std::io::Result<_>>()?,
            );
            (None, users::resolve(user_spec, &layers)?, users::Accounts::of(&layers))
        }
        None => {
            let rootfs = image.rootfs()?;
            let identity = users::resolve(user_spec, &rootfs)?;
            let accounts = users::Accounts::of(&rootfs);
            (Some(rootfs), identity, accounts)
        }
    };
    let stop_signal = match image.config.stop_signal.as_deref() {
        Some(name) => parse_signal(name).map_err(|e| format!("the image's StopSignal: {e}"))?,
        None => monitor::SIGTERM,
    };
    let argv = image.config.argv(&args.command);
    if argv.is_empty() {
        return Err("the image has no entrypoint or command, and none was given".into());
    }
    // The container is recorded once its image is known, and says how it ended whatever happens next.
    let data_dir = env.data_dir.as_deref().ok_or_else(no_data_dir)?;
    let containers = containers::Containers::open(data_dir)
        .map_err(|e| format!("the container store in {}: {e}", data_dir.display()))?;
    let container = containers.create(
        containers::Spec {
            name: args.name.clone(),
            image: args.image.clone(),
            image_id: image.id().to_string(),
            platform: format!("linux/{}", image::oci_arch(arch)),
            command: argv.clone(),
            tty: args.tty,
            interactive: args.interactive,
            user: args.user.clone(),
            auto_remove: args.rm,
        },
        images::now(),
    )?;
    let started = (|| -> Result<(monitor::Process, ipc::Conn, PathBuf), Box<dyn std::error::Error>> {
        let memory = memory.unwrap_or_else(|| memory_for(rootfs.as_ref().map_or(0, rootfs::Rootfs::content_bytes)));
        let dir = supervisor::run_dir(&env.run_base)?;
        let initrd = dir.join("initrd.cpio");
        let (agent, crun) = (std::fs::read(&guest.agent)?, std::fs::read(&guest.crun)?);
        std::fs::write(
            &initrd,
            match &rootfs {
                Some(rootfs) => initramfs::build_with_rootfs(&agent, &crun, rootfs),
                None => initramfs::build(&agent, &crun),
            },
        )?;
        let console_log = dir.join("console.log");
        let qemu_log = dir.join("qemu.log");
        let (qemu, qemu_args) = supervisor::qemu_command(&supervisor::Launch {
            bundle: &bundle,
            kernel: &guest.kernel,
            initrd: &initrd,
            console_log: &console_log,
            spec: supervisor::MachineSpec {
                arch,
                memory_mib: memory,
                cpus: args.cpus.unwrap_or(1),
            },
            disks: disks.as_deref().unwrap_or_default(),
        })?;
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
            layers: disks
                .iter()
                .flatten()
                .enumerate()
                .map(|(i, _)| supervisor::disk_serial(i))
                .collect(),
            // As Docker names it: the container's short ID.
            hostname: Some(containers::short(&container.id).to_string()),
        };
        let launch = monitor::Launch {
            qemu,
            args: qemu_args,
            qemu_log,
            console_log,
            command,
            interactive: args.interactive,
            detach: args.detach,
            boot_timeout_secs: 120,
            accounts,
            stop_signal,
        };
        store::write_atomically(
            &containers.dir(&container.id).join(monitor::LAUNCH),
            &serde_json::to_vec_pretty(&launch)?,
        )?;
        timings.mark("prepare");
        let monitor_log = dir.join(monitor::MONITOR_LOG);
        let mut process = (env.start_monitor)(&env.exe, data_dir, &container.id, &monitor_log)?;
        let conn = monitor::connect_new(data_dir, &container.id, &mut process, &monitor_log)?;
        Ok((process, conn, monitor_log))
    })();
    drop(rootfs);
    let (_process, conn, monitor_log) = match started {
        Ok(started) => started,
        Err(e) => {
            // No monitor ran it, so this records how it ended.
            if let Ok(running) = containers.start(&container.id, images::now()) {
                let _ = running.exited(Some(125), Some(e.to_string()), images::now());
            }
            if args.rm {
                let _ = containers.remove(&container.id);
            }
            return Err(e);
        }
    };
    attach(args, env, &container, conn, &monitor_log, store, timings, out, err)
}

/// The writing half of a connection to a monitor, shared by the threads that pass stdin and signals on.
type ToMonitor = std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>;

fn tell_monitor(to_monitor: &ToMonitor, request: &monitor::Request) -> std::io::Result<()> {
    let mut writer = to_monitor.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    monitor::send(writer.as_mut(), request)
}

/// Passes Ctrl-C and SIGTERM sent to this process on to the container's command, as `docker run` does, instead of
/// ending this process (which would stop the container at once). On Windows, Ctrl-C and Ctrl-Break.
fn forward_signals(to_monitor: ToMonitor) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_io().build() else {
            return;
        };
        let pass_on = |to_monitor: ToMonitor, signal: i32| {
            move || tell_monitor(&to_monitor, &monitor::Request::Signal { signal }).is_ok()
        };
        runtime.block_on(async move {
            #[cfg(unix)]
            let (interrupt, terminate) = {
                use tokio::signal::unix::{SignalKind, signal};
                (signal(SignalKind::interrupt()), signal(SignalKind::terminate()))
            };
            #[cfg(windows)]
            let (interrupt, terminate) = (tokio::signal::windows::ctrl_c(), tokio::signal::windows::ctrl_break());
            let (Ok(mut interrupt), Ok(mut terminate)) = (interrupt, terminate) else {
                return;
            };
            let on_terminate = pass_on(to_monitor.clone(), monitor::SIGTERM);
            tokio::spawn(async move { while terminate.recv().await.is_some() && on_terminate() {} });
            let on_interrupt = pass_on(to_monitor, SIGINT);
            while interrupt.recv().await.is_some() && on_interrupt() {}
        });
    });
}

/// SIGINT, what Ctrl-C sends.
const SIGINT: i32 = 2;

/// Passes this process's stdin to a monitor, on a thread of its own, until it ends.
fn forward_stdin(to_monitor: ToMonitor) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; hermit_proto::CHUNK];
        loop {
            let request = match std::io::Read::read(&mut stdin, &mut buf) {
                Ok(0) | Err(_) => monitor::Request::StdinClosed,
                Ok(n) => monitor::Request::Stdin {
                    data: hermit_proto::b64::encode(&buf[..n]),
                },
            };
            let last = request == monitor::Request::StdinClosed;
            if tell_monitor(&to_monitor, &request).is_err() || last {
                return;
            }
        }
    });
}

/// `exec`: run a command in a running container, through its monitor, and exit with its exit code; 125 when it
/// cannot be started.
fn exec_command(args: &cli::ExecArgs, env: &Env, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match exec_in(args, env, out, err) {
        Ok(code) => u8::try_from(code.rem_euclid(256)).unwrap_or(1),
        Err(e) => {
            let _ = writeln!(err, "oci-hermit: {e}");
            125
        }
    }
}

fn exec_in(
    args: &cli::ExecArgs,
    env: &Env,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<i32, Box<dyn std::error::Error>> {
    let containers = open_containers(env)?;
    let data_dir = env.data_dir.as_deref().expect("the container store opened");
    let listed = containers.find(&args.container)?;
    let name = &listed.container.name;
    if !listed.running {
        return Err(format!("container {name} is not running").into());
    }
    let ipc::Conn { reader, mut writer } = ipc::connect(data_dir, &listed.container.id)
        .map_err(|e| format!("cannot reach the monitor of container {name}: {e}"))?;
    // `-e NAME` without a value passes this process's own, as Docker does; one that is not set is left out.
    let mut vars: Vec<String> = args
        .env
        .iter()
        .filter_map(|var| match var.contains('=') {
            true => Some(var.clone()),
            false => std::env::var(var).ok().map(|value| format!("{var}={value}")),
        })
        .collect();
    if args.tty && !vars.iter().any(|v| v.starts_with("TERM=")) {
        vars.push(format!(
            "TERM={}",
            std::env::var("TERM").unwrap_or_else(|_| "xterm".into())
        ));
    }
    monitor::send(
        writer.as_mut(),
        &monitor::Request::Exec {
            argv: args.command.clone(),
            env: vars,
            workdir: args.workdir.clone(),
            user: args.user.clone(),
            tty: args.tty,
            size: args
                .tty
                .then(crossterm::terminal::size)
                .and_then(Result::ok)
                .map(|(cols, rows)| (rows, cols)),
            interactive: args.interactive,
        },
    )?;
    let writer: ToMonitor = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let mut raw = None;
    let (mut code, mut failure) = (None, None);
    monitor::read_messages(reader, |notice: monitor::Notice| {
        match notice {
            monitor::Notice::Started => {
                if args.tty && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    raw = RawTerminal::enable();
                }
                if args.interactive {
                    forward_stdin(writer.clone());
                }
            }
            monitor::Notice::Output { stream, data } => {
                let Some(bytes) = hermit_proto::b64::decode(&data) else {
                    failure = Some("the monitor sent output that is not base64".to_string());
                    return false;
                };
                let sink: &mut dyn Write = match stream {
                    hermit_proto::Stream::Stdout => out,
                    hermit_proto::Stream::Stderr => err,
                };
                // A reader that went away (`| head`) ends the exec here; its command runs on.
                if sink.write_all(&bytes).and_then(|()| sink.flush()).is_err() {
                    return false;
                }
            }
            monitor::Notice::Exited { code: ended } => {
                drop(raw.take());
                code = Some(ended);
            }
            monitor::Notice::Failed { message } => failure = Some(message),
            monitor::Notice::Done => return false,
            monitor::Notice::Booted | monitor::Notice::Stopping | monitor::Notice::Signalled => {}
        }
        true
    });
    drop(raw);
    if let Some(message) = failure {
        return Err(message.into());
    }
    Ok(code.ok_or_else(|| format!("container {name} stopped before the command ended"))?)
}

/// Follows the container `run` started, through its monitor: drops `store` once QEMU has the layer disks open, and
/// with `-d` prints the container's ID once its command started; otherwise relays the command's stdin and output
/// and returns its exit code.
#[allow(clippy::too_many_arguments)]
fn attach(
    args: &cli::RunArgs,
    env: &Env,
    container: &containers::Container,
    conn: ipc::Conn,
    monitor_log: &std::path::Path,
    store: Option<store::Store>,
    mut timings: Timings,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<i32, Box<dyn std::error::Error>> {
    let ipc::Conn { reader, mut writer } = conn;
    monitor::send(writer.as_mut(), &monitor::Request::Attach)?;
    let (notices, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        monitor::read_messages(reader, |notice: monitor::Notice| notices.send(notice).is_ok());
    });
    let mut store = store;
    // Kept open for as long as the run is: the monitor stops a foreground container whose run went away.
    let writer: ToMonitor = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let mut raw = None;
    let (mut code, mut failure) = (None, None);
    for notice in received {
        match notice {
            monitor::Notice::Booted => {
                // QEMU has the layer disks open now, so the store may let `rmi` go ahead.
                drop(store.take());
                timings.mark("boot");
            }
            monitor::Notice::Started if args.detach => {
                writeln!(out, "{}", container.id)?;
                return Ok(0);
            }
            monitor::Notice::Started => {
                if args.tty && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    raw = RawTerminal::enable();
                }
                forward_signals(writer.clone());
                if args.interactive {
                    forward_stdin(writer.clone());
                }
            }
            monitor::Notice::Output { stream, data } => {
                let bytes = hermit_proto::b64::decode(&data).ok_or("the monitor sent output that is not base64")?;
                let sink: &mut dyn Write = match stream {
                    hermit_proto::Stream::Stdout => out,
                    hermit_proto::Stream::Stderr => err,
                };
                sink.write_all(&bytes)?;
                sink.flush()?;
            }
            monitor::Notice::Exited { code: ended } => {
                drop(raw.take());
                code = Some(ended);
                timings.mark("command");
            }
            monitor::Notice::Failed { message } => failure = Some(message),
            monitor::Notice::Stopping | monitor::Notice::Signalled => {}
            monitor::Notice::Done => break,
        }
    }
    drop(raw);
    if let Some(message) = failure {
        return Err(message.into());
    }
    let code = code.ok_or_else(|| {
        format!(
            "the container's monitor stopped without saying how it ended; see {}",
            monitor_log.display()
        )
    })?;
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
        let cli = <cli::Cli as clap::Parser>::try_parse_from(std::iter::once("oci-hermit").chain(args.iter().copied()))
            .unwrap();
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
            data_dir_from_env: true,
            registry: registry::Settings::default(),
            confirm: || false,
            ask: |_| Err(std::io::Error::other("no terminal in tests")),
            start_monitor: monitor::testing::in_thread,
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
            data_dir_from_env: true,
            registry: registry::Settings::default(),
            confirm: || false,
            ask: |_| Err(std::io::Error::other("no terminal in tests")),
            start_monitor: monitor::testing::in_thread,
        }
    }

    #[test]
    fn the_help_uses_the_name_it_was_started_as() {
        use std::ffi::OsStr;
        assert_eq!(invoked_name(Some(OsStr::new("/opt/bin/hrmt"))), "hrmt");
        assert_eq!(invoked_name(Some(OsStr::new("HRMT.EXE"))), "hrmt");
        assert_eq!(invoked_name(Some(OsStr::new("/usr/bin/oci-hermit"))), "oci-hermit");
        assert_eq!(invoked_name(Some(OsStr::new(""))), "oci-hermit");
        assert_eq!(invoked_name(None), "oci-hermit");
        let help = cli::Cli::command()
            .name("hrmt")
            .bin_name("hrmt")
            .render_help()
            .to_string();
        assert!(help.contains("Usage: hrmt <COMMAND>"), "{help}");
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_is_found_through_a_symlink() {
        let dir = qemu::testing::scratch("main-real-exe");
        std::fs::write(dir.join("oci-hermit"), "").unwrap();
        let elsewhere = dir.join("bin");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(dir.join("oci-hermit"), elsewhere.join("hrmt")).unwrap();
        let real = real_exe(Ok(elsewhere.join("hrmt")));
        assert_eq!(real, dir.join("oci-hermit").canonicalize().unwrap());
        assert_eq!(real_exe(Err(std::io::Error::other("none"))), PathBuf::new());
    }

    #[test]
    fn version_prints_crate_and_protocol_versions() {
        let (code, out, err) = run_with(&no_qemu("main-none-1"), &["version"]);
        assert_eq!(code, 0);
        assert!(out.starts_with(concat!("oci-hermit ", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("control protocol 1.6"));
        assert!(err.is_empty());
    }

    #[test]
    fn stub_commands_exit_not_implemented() {
        let (code, out, err) = run_with(&no_qemu("main-none-2"), &["cp", "web:/a", "b"]);
        assert_eq!(code, NOT_IMPLEMENTED);
        assert!(out.is_empty());
        assert!(err.contains("`cp` is not implemented yet"));
        assert!(err.contains(ROADMAP));
    }

    #[test]
    fn machine_info_without_a_bundle_fails() {
        let (code, out, err) = run_with(&no_qemu("main-none-3"), &["machine", "info"]);
        assert_eq!(code, 1);
        // The rest is still shown: it says where things would be.
        assert!(out.contains("  bundle:     not found (see below)"), "{out}");
        assert!(
            out.contains("  x86_64:     not found; amd64 images cannot run"),
            "{out}"
        );
        assert!(
            out.contains("  images:     none yet; created by the first pull or load"),
            "{out}"
        );
        assert!(err.contains("no bundled QEMU found"));
        // `info` is the same report, under Podman's name.
        assert_eq!(run_with(&no_qemu("main-none-3"), &["info"]), (code, out, err));
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
        assert!(out.starts_with(&version_line()), "{out}");
        assert!(out.contains(&format!("  bundle:     {} (from OCI_HERMIT_QEMU_DIR)", dir.display())));
        assert!(out.contains("  x86_64:     QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg"));
        assert!(out.contains("  aarch64:    QEMU 11.1.3 (oci-hermit-qemu-4), accel tcg"));
    }

    #[cfg(unix)]
    #[test]
    fn machine_info_shows_the_guest_store_and_registries() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-info-all");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-info-all-guest");
        let auth = bundle.join("auth.json");
        std::fs::write(&auth, "{}").unwrap();
        let mut env = with_bundle(&bundle, Some(&guest));
        env.registry.auth_locations = vec![
            registry::AuthLocation {
                name: "$REGISTRY_AUTH_FILE",
                path: None,
            },
            registry::AuthLocation {
                name: "$ELSEWHERE/auth.json",
                path: Some(bundle.join("missing.json")),
            },
            registry::AuthLocation {
                name: "$HERE",
                path: Some(auth.clone()),
            },
        ];
        env.registry.insecure = vec!["localhost:5000".into()];
        let certs_d = bundle.join("certs.d");
        for (folder, files) in [
            ("reg.example:5000", &["ca.crt", "client.cert", "client.key"][..]),
            ("broken.example", &["lonely.cert"][..]),
        ] {
            std::fs::create_dir_all(certs_d.join(folder)).unwrap();
            for file in files {
                std::fs::write(certs_d.join(folder).join(file), "x").unwrap();
            }
        }
        env.registry.cert_locations = vec![
            certs::CertLocation {
                name: "$NOWHERE/certs.d",
                path: None,
            },
            certs::CertLocation {
                name: "here",
                path: Some(certs_d.clone()),
            },
            certs::CertLocation {
                name: "missing",
                path: Some(bundle.join("no-certs.d")),
            },
        ];
        env.data_dir_from_env = false;

        // A store with one image for two platforms.
        let store = store::Store::open(env.data_dir.as_ref().unwrap()).unwrap();
        let blob = b"{}";
        let digest = image::digest_of(blob);
        store.put_blob(&digest, blob).unwrap();
        for arch in ["amd64", "arm64"] {
            store.tag("example.com/app:1", "linux", arch, "m", &digest, 2).unwrap();
        }

        let (code, out, err) = run_with(&env, &["machine", "info"]);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        let x86 = guest.join("x86_64");
        assert!(
            out.contains(&format!(
                "  x86_64:     {} (kernel 6 B, agent 5 B, from OCI_HERMIT_GUEST_DIR)",
                x86.display()
            )),
            "{out}"
        );
        let images = env.data_dir.as_ref().unwrap().join("images");
        assert!(out.contains(&format!("  path:       {} (the platform default)", images.display())));
        assert!(
            out.contains("  images:     2 images under 1 name (one per name and platform)"),
            "{out}"
        );
        assert!(out.contains("  size:       2 B"), "{out}");
        assert!(
            out.contains(&format!(
                "    1. [not set]   $REGISTRY_AUTH_FILE\n    2. [not found] {}\n    3. [found]     $HERE = {}\n",
                bundle.join("missing.json").display(),
                auth.display()
            )),
            "{out}"
        );
        assert!(out.contains("  plain http: localhost:5000 (from OCI_HERMIT_INSECURE_REGISTRIES)"));
        let lonely = certs_d.join("broken.example/lonely.cert");
        assert!(
            out.contains(&format!(
                "    1. [not set]   $NOWHERE/certs.d\n    2. [found]     {}\n{:19}broken.example: unusable: {} has no \
                 matching {}; a client certificate needs its key\n{:19}reg.example:5000: 1 CA, a client certificate\n\
                 \x20   3. [not found] {}\n",
                certs_d.display(),
                "",
                lonely.display(),
                lonely.with_extension("key").display(),
                "",
                bundle.join("no-certs.d").display()
            )),
            "{out}"
        );
        assert!(out.contains(&format!("  run dir:    {}", env.run_base.display())));
    }

    #[cfg(unix)]
    #[test]
    fn pull_and_login_take_a_cert_dir_and_warn_about_open_keys() {
        use std::os::unix::fs::PermissionsExt;
        let env = no_qemu("main-cert-dir");
        let given = qemu::testing::scratch("main-cert-dir-certs");
        std::fs::write(given.join("client.cert"), "junk").unwrap();
        std::fs::write(given.join("client.key"), "junk").unwrap();
        std::fs::set_permissions(given.join("client.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let dir = given.display().to_string();
        let (code, _, err) = run_with(&env, &["pull", "--cert-dir", &dir, "reg.example/app:1"]);
        assert_eq!(code, 1);
        assert!(err.contains("can be read by other users"), "{err}");
        assert!(err.contains("not a PEM certificate and its private key"), "{err}");
        let (code, _, err) = run_with(
            &env,
            &[
                "login",
                "--cert-dir",
                &dir,
                "--authfile",
                &given.join("auth.json").display().to_string(),
                "-u",
                "me",
                "-p",
                "pw",
                "reg.example",
            ],
        );
        assert_eq!(code, 1);
        assert!(err.contains("not a PEM certificate and its private key"), "{err}");
        let (code, _, err) = run_with(&env, &["pull", "--cert-dir", "/no/such/dir", "reg.example/app:1"]);
        assert_eq!(code, 1);
        assert!(err.contains("/no/such/dir"), "{err}");
    }

    #[test]
    fn image_commands_work_on_the_store() {
        let env = no_qemu("main-images");
        let store = store::Store::open(env.data_dir.as_ref().unwrap()).unwrap();
        images::testing::add(&store, "example.com/app:1", "amd64", images::testing::CONFIG);
        drop(store);

        let (code, out, err) = run_with(&env, &["images"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains("example.com/app   1"), "{out}");
        let (code, out, _) = run_with(&env, &["image", "ls", "-q"]);
        assert_eq!((code, out.lines().count()), (0, 1));

        assert_eq!(run_with(&env, &["tag", "example.com/app:1", "example.com/app:2"]).0, 0);
        assert_eq!(run_with(&env, &["image", "exists", "example.com/app:2"]).0, 0);
        assert_eq!(run_with(&env, &["image", "exists", "example.com/app:3"]).0, 1);

        let (code, out, err) = run_with(&env, &["inspect", "example.com/app:2", "nope"]);
        assert_eq!(code, 1);
        assert!(out.starts_with("[\n  {\n    \"Id\": \"sha256:"), "{out}");
        assert!(err.contains("no such image: nope"));

        let (code, out, _) = run_with(&env, &["image", "history", "example.com/app:1"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("IMAGE"));

        let saved = env.data_dir.as_ref().unwrap().join("app.tar");
        let saved = saved.to_str().unwrap();
        assert_eq!(run_with(&env, &["save", "-o", saved, "example.com/app:1"]).0, 0);
        let (code, _, err) = run_with(&env, &["image", "save", "-o", saved, "nope"]);
        assert_eq!(code, 1);
        assert!(err.contains("no such image: nope"));

        let (code, out, _) = run_with(&env, &["system", "df"]);
        assert_eq!(code, 0);
        assert!(out.contains("Images       1"), "{out}");

        // Prune asks first; the test's answer is no.
        let (code, out, _) = run_with(&env, &["image", "prune", "-a"]);
        assert_eq!(code, 0);
        assert!(out.contains("This will remove all images."), "{out}");
        assert_eq!(run_with(&env, &["image", "exists", "example.com/app:1"]).0, 0);

        let (code, out, _) = run_with(&env, &["rmi", "example.com/app:2", "nope"]);
        assert_eq!(code, 1);
        assert!(out.contains("Untagged: example.com/app:2"));
        let (code, out, _) = run_with(&env, &["image", "prune", "-a", "-f"]);
        assert_eq!(code, 0);
        assert!(out.contains("untagged: example.com/app:1"), "{out}");
        assert!(out.contains("Total reclaimed space"));

        // While another oci-hermit has the store open, nothing is deleted.
        let held = store::Store::open(env.data_dir.as_ref().unwrap()).unwrap();
        let (code, _, err) = run_with(&env, &["image", "rm", "x"]);
        assert_eq!(code, 1);
        assert!(err.contains("in use by another oci-hermit"), "{err}");
        drop(held);
    }

    /// An environment whose logins go to `file` and whose registry at `address` is plain HTTP.
    fn login_env(name: &str, address: &str, ask: fn(Ask) -> std::io::Result<String>) -> (Env, PathBuf) {
        let base = no_qemu(name);
        let file = base.run_base.with_file_name("auth.json");
        let env = Env {
            registry: registry::Settings {
                auth_files: vec![file.clone()],
                auth_locations: vec![registry::AuthLocation {
                    name: "$REGISTRY_AUTH_FILE",
                    path: Some(file.clone()),
                }],
                insecure: vec![address.to_string()],
                ..registry::Settings::default()
            },
            ask,
            ..base
        };
        (env, file)
    }

    fn answers(what: Ask) -> std::io::Result<String> {
        Ok(match what {
            Ask::Username => "alice".into(),
            Ask::Password | Ask::Stdin => "s3cret".into(),
        })
    }

    #[test]
    fn login_checks_with_the_registry_then_stores_it() {
        let secret = hermit_proto::b64::encode(b"alice:s3cret");
        let address = login::testing::registry(&secret, true);
        let (env, file) = login_env("main-login", &address, answers);

        // Asked for both, checked through the token server, stored.
        let (code, out, err) = run_with(&env, &["login", &address]);
        assert_eq!((code, out.as_str(), err.as_str()), (0, "Login Succeeded!\n", ""));
        assert_eq!(login::user(&file, &address).as_deref(), Some("alice"));
        let (code, out, _) = run_with(&env, &["login", "--get-login", &address]);
        assert_eq!((code, out.as_str()), (0, "alice\n"));

        // A wrong password is turned down and nothing changes.
        let before = std::fs::read(&file).unwrap();
        let (code, _, err) = run_with(&env, &["login", "-u", "alice", "-p", "nope", &address]);
        assert_eq!(code, 1);
        assert!(err.contains("WARNING! --password on the command line"), "{err}");
        assert!(err.contains("rejected the user name or password"), "{err}");
        assert_eq!(std::fs::read(&file).unwrap(), before);

        let (code, out, _) = run_with(&env, &["login", "-u", "alice", "--password-stdin", &address]);
        assert_eq!((code, out.as_str()), (0, "Login Succeeded!\n"));
        assert!(machine_info_text(&env).contains(&format!("login file: {} (where login writes)", file.display())));

        let (code, out, _) = run_with(&env, &["logout", &address]);
        assert_eq!(
            (code, out.trim()),
            (0, format!("Removed login credentials for {address}").as_str())
        );
        let (code, _, err) = run_with(&env, &["logout", &address]);
        assert_eq!(code, 1);
        assert!(err.contains(&format!("not logged in to {address}")), "{err}");
        let (code, _, err) = run_with(&env, &["login", "--get-login", &address]);
        assert_eq!(code, 1);
        assert!(err.contains("not logged in"));
    }

    fn machine_info_text(env: &Env) -> String {
        run_with(env, &["machine", "info"]).1
    }

    #[test]
    fn login_refuses_what_it_cannot_use() {
        let (env, file) = login_env("main-login-bad", "127.0.0.1:9", answers);
        let cases: &[(&[&str], &str)] = &[
            (
                &["login", "-p", "x", "--password-stdin", "r.example"],
                "cannot be used together",
            ),
            (&["login", "--password-stdin", "r.example"], "needs --username"),
            (&["login", "ghcr.io/org/app"], "not a registry"),
            (&["logout", "--all", "ghcr.io"], "not both"),
        ];
        for (args, message) in cases {
            let (code, _, err) = run_with(&env, args);
            assert_eq!(code, 1, "{args:?}");
            assert!(err.contains(message), "{args:?}: {err}");
        }
        let empty = |_: Ask| Ok(String::new());
        let (env_empty, _) = login_env("main-login-empty", "127.0.0.1:9", empty);
        let (code, _, err) = run_with(&env_empty, &["login", "r.example"]);
        assert_eq!(code, 1);
        assert!(err.contains("both needed"), "{err}");
        // With no terminal, asking fails with a hint.
        let (code, _, err) = run_with(
            &no_qemu("main-login-noterm"),
            &["login", "--authfile", "a.json", "r.example"],
        );
        assert_eq!(code, 1);
        assert!(err.contains("no terminal in tests"), "{err}");

        // logout --all, and a login another tool made.
        login::store(&file, "ghcr.io", "a", "b").unwrap();
        let (code, out, _) = run_with(&env, &["logout", "--all"]);
        assert_eq!((code, out.trim()), (0, "Removed login credentials for all registries"));
        let docker = file.with_file_name("config.json");
        std::fs::write(&docker, r#"{"auths":{"https://index.docker.io/v1/":{"auth":"eDp5"}}}"#).unwrap();
        let env = Env {
            registry: registry::Settings {
                auth_files: vec![file.clone(), docker.clone()],
                ..env.registry.clone()
            },
            ..env
        };
        let (code, _, err) = run_with(&env, &["logout"]);
        assert_eq!(code, 1);
        assert!(err.contains("docker logout docker.io"), "{err}");
        let nowhere = Env {
            registry: registry::Settings::default(),
            ..no_qemu("main-login-nowhere")
        };
        let (code, _, err) = run_with(&nowhere, &["logout"]);
        assert_eq!(code, 1);
        assert!(err.contains("no home folder"), "{err}");
    }

    #[test]
    fn passwords_are_read_without_echo_handling() {
        let read = |bytes: &[u8]| read_secret(&mut &bytes[..]);
        assert_eq!(read(b"s3cret\r").unwrap(), "s3cret");
        assert_eq!(read(b"ab\x7fc\n").unwrap(), "ac");
        assert_eq!(read("p\u{e9}\x08x\r".as_bytes()).unwrap(), "px");
        assert_eq!(read(b"no newline").unwrap(), "no newline");
        assert_eq!(read(b"ab\x03").unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn image_commands_need_somewhere_to_keep_images() {
        let env = Env {
            data_dir: None,
            ..no_qemu("main-images-none")
        };
        let (code, out, _) = run_with(&env, &["system", "df"]);
        assert_eq!(code, 0);
        assert!(out.contains("Images       0"), "{out}");
        for args in [&["images"][..], &["rmi", "x"], &["image", "prune", "-f"]] {
            let (code, _, err) = run_with(&env, args);
            assert_eq!(code, 1, "{args:?}");
            assert!(err.contains("no home folder"), "{args:?}: {err}");
        }
    }

    #[test]
    fn prune_goes_ahead_when_confirmed() {
        let env = Env {
            confirm: || true,
            ..no_qemu("main-images-yes")
        };
        let (code, out, _) = run_with(&env, &["image", "prune"]);
        assert_eq!(code, 0);
        assert!(out.ends_with("Total reclaimed space: 0 B\n"), "{out}");
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

    /// A guest directory holding a kernel, agent and crun for both architectures.
    #[cfg(unix)]
    fn fake_guest(name: &str) -> PathBuf {
        let dir = qemu::testing::scratch(name);
        for arch in qemu::GuestArch::ALL {
            let arch_dir = dir.join(arch.name());
            std::fs::create_dir_all(&arch_dir).unwrap();
            std::fs::write(arch_dir.join("kernel"), "kernel").unwrap();
            std::fs::write(arch_dir.join("hermit-agent"), "agent").unwrap();
            std::fs::write(arch_dir.join("crun"), "crun").unwrap();
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
                out.contains("(control protocol 1.6)"),
                "the fake agent speaks 1.6: {out}"
            );
            assert!(out.contains("guest powered off; sandbox stopped"));
        }
        // Each boot got its own directory with the initramfs it built from the agent.
        let runs: Vec<_> = std::fs::read_dir(bundle.join("run")).unwrap().collect();
        assert_eq!(runs.len(), 2);
        let run = runs[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::read(run.join("initrd.cpio")).unwrap(),
            initramfs::build(b"agent", b"crun")
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
        assert!(err.contains("no aarch64 guest kernel, agent and crun found"));
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
        // The container's hostname is its short ID, as Docker gives it.
        let container = containers::Containers::open(&bundle.join("data"))
            .unwrap()
            .list()
            .unwrap();
        assert_eq!(container.len(), 1);
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
                layers: Vec::new(),
                hostname: Some(containers::short(&container[0].container.id).to_string()),
            }
        );
        // An image opened from a path has no layer disks: the initramfs holds the agent and its files under rootfs/.
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

    #[cfg(unix)]
    #[test]
    fn a_detached_container_runs_until_it_is_stopped() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-detach");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-detach-guest");
        let layout = image::testing::layout(
            &qemu::testing::scratch("main-detach-image"),
            &[("amd64", image::testing::layer())],
            r#"{"architecture":"ARCH","config":{"Cmd":["true"]}}"#,
        );
        let env = with_bundle(&bundle, Some(&guest));
        let image = layout.to_str().unwrap();
        let detached = |name: &str| {
            run_with(
                &env,
                &["run", "-d", "--name", name, "--platform", "linux/amd64", image, "wait"],
            )
        };

        // `run -d` prints the container's ID once its command started, and the container goes on without it.
        let (code, out, err) = detached("web");
        assert_eq!((code, err.as_str()), (0, ""));
        let id = out.trim_end();
        assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()), "{out}");
        let (_, out, _) = run_with(&env, &["ps"]);
        let row = out.lines().nth(1).unwrap_or_else(|| panic!("{out}"));
        assert!(
            row.contains(" Up ") && row.ends_with("web") && row.starts_with(&id[..12]),
            "{out}"
        );
        // `Started` can arrive before the monitor has logged the command's first output, so give it a moment.
        let mut logged = run_with(&env, &["logs", "web"]);
        for _ in 0..100 {
            if !logged.1.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            logged = run_with(&env, &["logs", "web"]);
        }
        let (code, out, err) = logged;
        assert_eq!((code, out.as_str(), err.as_str()), (0, "hi\n", ""));

        // exec runs another command in it, by name or ID, and exits with that command's code; its output is not
        // the container's.
        let (code, out, err) = run_with(&env, &["exec", "-e", "A=1", "-w", "/tmp", "web", "ls"]);
        assert_eq!((code, out.as_str(), err.as_str()), (5, "ex\n", ""));
        assert_eq!(run_with(&env, &["exec", &id[..12], "ls"]).0, 5);
        assert_eq!(run_with(&env, &["logs", "web"]).1, "hi\n");
        let (code, _, err) = run_with(&env, &["exec", "-u", "ghost", "web", "id"]);
        assert_eq!(code, 125);
        assert!(err.contains("\"ghost\" is not in the image's /etc/passwd"), "{err}");
        let (code, _, err) = run_with(&env, &["exec", "nothere", "ls"]);
        assert_eq!(code, 125);
        assert!(err.contains("no such container: nothere"), "{err}");

        // kill passes a signal on; the fake command ignores all but SIGTERM.
        let (code, out, err) = run_with(&env, &["kill", "-s", "SIGUSR1", "web"]);
        assert_eq!((code, out.as_str()), (0, "web\n"), "{err}");
        let (code, _, err) = run_with(&env, &["kill", "-s", "NOPE", "web"]);
        assert_eq!(code, 1);
        assert!(err.contains("\"NOPE\" is not a signal"), "{err}");
        assert_eq!(run_with(&env, &["stop", "-s", "BOGUS", "web"]).0, 1);
        // stop -s with a signal it ignores waits out -t, then kills it.
        let started = std::time::Instant::now();
        let (code, out, err) = run_with(&env, &["stop", "-t", "1", "-s", "INT", "web"]);
        assert_eq!((code, out.as_str()), (0, "web\n"), "{err}");
        assert!(started.elapsed() >= std::time::Duration::from_secs(1));
        let (_, out, _) = run_with(&env, &["ps", "-a"]);
        assert!(out.contains("Exited (137)"), "{out}");
        let (code, _, err) = run_with(&env, &["kill", "web"]);
        assert_eq!(code, 1);
        assert!(err.contains("container web is not running"), "{err}");

        // rm will not remove a running container; stop sends it SIGTERM, which this one exits on.
        assert_eq!(detached("calm").0, 0);
        let (code, _, err) = run_with(&env, &["rm", "calm"]);
        assert_eq!(code, 1);
        assert!(err.contains("container calm is running; stop it first"), "{err}");
        let (code, out, err) = run_with(&env, &["stop", "calm"]);
        assert_eq!((code, out.as_str()), (0, "calm\n"), "{err}");
        let (_, out, _) = run_with(&env, &["ps", "-a"]);
        assert!(out.contains("Exited (143)"), "{out}");
        assert_eq!(run_with(&env, &["rm", "calm"]).0, 0);
        // Following a stopped container's logs shows them and ends.
        let (code, out, _) = run_with(&env, &["logs", "-f", id]);
        assert_eq!((code, out.as_str()), (0, "hi\n"));
        // Stopping it again is fine, as with Docker; exec needs it running.
        assert_eq!(run_with(&env, &["stop", "web"]).0, 0);
        let (code, _, err) = run_with(&env, &["exec", "web", "ls"]);
        assert_eq!(code, 125);
        assert!(err.contains("container web is not running"), "{err}");

        // rm -f stops a running container and removes it.
        let (code, _, err) = detached("job");
        assert_eq!(code, 0, "{err}");
        let (code, out, err) = run_with(&env, &["rm", "-f", "job", "web"]);
        assert_eq!((code, out.as_str()), (0, "job\nweb\n"), "{err}");
        assert_eq!(run_with(&env, &["ps", "-aq"]).1, "");

        // A guest from before exec runs containers, but no exec in them.
        std::fs::write(bundle.join("agent.version"), "1,4").unwrap();
        assert_eq!(detached("legacy").0, 0);
        let (code, _, err) = run_with(&env, &["exec", "legacy", "ls"]);
        assert_eq!(code, 125);
        assert!(err.contains("which has no exec; it needs 1.5 or later"), "{err}");
        assert_eq!(run_with(&env, &["rm", "-f", "legacy"]).0, 0);

        // A detached container that cannot run says why, and is kept as having failed.
        std::fs::write(bundle.join("agent.version"), "1,3").unwrap();
        let (code, out, err) = detached("old");
        assert_eq!((code, out.as_str()), (125, ""));
        assert!(err.contains("it needs 1.4 or later"), "{err}");
        let c = containers::Containers::open(&bundle.join("data")).unwrap();
        assert_eq!(c.find("old").unwrap().state.exit_code, Some(125));
        let (code, _, err) = run_with(&env, &["logs", "ghost"]);
        assert_eq!(code, 1);
        assert!(err.contains("no such container: ghost"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_records_each_container_for_ps_and_rm() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-run-record");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-run-record-guest");
        let layout = image::testing::layout(
            &qemu::testing::scratch("main-run-record-image"),
            &[("amd64", image::testing::layer())],
            r#"{"architecture":"ARCH","config":{"Cmd":["true"]}}"#,
        );
        let env = with_bundle(&bundle, Some(&guest));
        let image = layout.to_str().unwrap();
        let run = |extra: &[&str]| {
            let argv: Vec<&str> = ["run", "--platform", "linux/amd64"]
                .into_iter()
                .chain(extra.iter().copied())
                .chain([image])
                .collect();
            run_with(&env, &argv)
        };

        let (code, _, err) = run(&["--name", "job"]);
        assert_eq!(code, 3, "{err}");
        let (code, out, _) = run_with(&env, &["ps"]);
        assert_eq!(
            (code, out.lines().count()),
            (0, 1),
            "only the header: nothing is running\n{out}"
        );
        let (code, out, _) = run_with(&env, &["ps", "-a"]);
        assert_eq!(code, 0);
        let row = out.lines().nth(1).unwrap_or_else(|| panic!("{out}"));
        assert!(
            row.contains("\"true\"") && row.contains("Exited (3)") && row.ends_with("job"),
            "{out}"
        );

        // The name stays taken until the container is removed.
        let (code, _, err) = run(&["--name", "job"]);
        assert_eq!(code, 125);
        assert!(err.contains("the container name \"job\" is already in use"), "{err}");
        let (code, out, err) = run_with(&env, &["rm", "job"]);
        assert_eq!((code, out.as_str()), (0, "job\n"), "{err}");

        // --rm leaves nothing behind; neither does a failed run with it.
        let (code, _, err) = run(&["--rm"]);
        assert_eq!(code, 3, "{err}");
        let (_, out, _) = run_with(&env, &["ps", "-aq"]);
        assert_eq!(out, "");

        // A run oci-hermit could not finish is kept with exit code 125 and the reason.
        std::fs::write(bundle.join("agent.version"), "0,9").unwrap();
        let (code, _, _) = run(&["--name", "old-agent"]);
        assert_eq!(code, 125);
        let c = containers::Containers::open(&bundle.join("data")).unwrap();
        let failed = c.find("old-agent").unwrap();
        assert_eq!(failed.state.exit_code, Some(125));
        assert!(failed.state.error.is_some());
        let (code, _, err) = run_with(&env, &["rm", "-f", "old-agent", "missing"]);
        assert_eq!(code, 1);
        assert!(err.contains("no such container: missing"), "{err}");
        assert!(c.list().unwrap().is_empty());
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
            (&["-p", "8080:80"][..], "-p/--publish", "#27"),
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
        // --rm and --name are taken; a name is checked before anything else happens.
        let (code, _, err) = run_with(
            &env,
            &["run", "--rm", "--name", "web", "--platform", "linux/arm64", "."],
        );
        assert_eq!(code, 125);
        assert!(err.contains("no bundled QEMU found"), "{err}");
        let (code, _, err) = run_with(&env, &["run", "--name=-web", "."]);
        assert_eq!(code, 125);
        assert!(err.contains("invalid container name \"-web\""), "{err}");
        let (code, _, err) = run_with(&env, &["run", "-m", "lots", "."]);
        assert_eq!(code, 125);
        assert!(err.contains("--memory lots"), "{err}");
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
        let (code, err, seen) = run_as("main-user-image", "app", "1,4", &[]);
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
        let (code, err, seen) = run_as("main-user-flag", "app", "1,4", &["-u", "root"]);
        assert_eq!(code, 3, "{err}");
        assert_eq!(user(seen), (None, vec!["HOME=/root".to_string()]));
        let (_, _, seen) = run_as("main-user-numeric", "", "1,4", &["--user", "4242:44"]);
        assert_eq!(user(seen).0.map(|u| (u.uid, u.gid)), Some((4242, 44)));
    }

    #[test]
    #[cfg(unix)]
    fn run_refuses_users_it_cannot_honour() {
        // A name the image does not have fails before anything boots.
        let (code, err, seen) = run_as("main-user-ghost", "ghost", "1,4", &[]);
        assert_eq!(code, 125);
        assert_eq!(err, "oci-hermit: user \"ghost\" is not in the image's /etc/passwd\n");
        assert!(seen.is_none());
        // An agent that predates containers would run the command outside one (as root, before 1.2), so it is
        // never asked to, whoever the user is.
        for (name, user) in [("main-user-old-agent", "app"), ("main-user-old-root", "")] {
            for version in ["1,1", "1,3"] {
                let (code, err, seen) = run_as(name, user, version, &[]);
                assert_eq!(code, 125);
                assert!(err.contains("cannot run containers; it needs 1.4 or later"), "{err}");
                assert!(seen.is_none());
            }
        }
    }

    #[test]
    fn manifest_inspect_prints_what_the_registry_serves() {
        let address = registry::testing::registry("main-manifest-registry", None, false, 0);
        let env = Env {
            registry: registry::Settings {
                insecure: vec![address.clone()],
                ..registry::Settings::default()
            },
            ..no_qemu("main-manifest-env")
        };
        let reference = format!("{address}/test/app:1");
        let (code, out, err) = run_with(&env, &["manifest", "inspect", &reference]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(
            out.contains("\"mediaType\": \"application/vnd.oci.image.index.v1+json\""),
            "{out}"
        );
        let (code, out, _) = run_with(&env, &["manifest", "inspect", "-v", &reference]);
        assert_eq!(code, 0);
        assert_eq!(out.matches("\"OCIManifest\"").count(), 2, "{out}");
        let (code, _, err) = run_with(&env, &["manifest", "inspect", "Not A Reference"]);
        assert_eq!(code, 1);
        assert!(err.contains("is not an image reference"), "{err}");
        // Nothing was stored.
        let (_, out, _) = run_with(&env, &["images", "-q"]);
        assert_eq!(out, "");
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
        // A stored image runs from its layer disk (#17): the agent is told the disk, and the initramfs holds the
        // agent alone.
        assert!(seen.contains(r#""layers":["layer0"]"#), "{seen}");
        let disks = env
            .data_dir
            .as_ref()
            .unwrap()
            .join(format!("images/erofs/{}", erofs::FORMAT));
        assert_eq!(std::fs::read_dir(&disks).unwrap().count(), 1);
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
        assert!(!names.iter().any(|n| n.starts_with("rootfs/")), "{names:?}");

        // The second run finds it in the store, and its disk.
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/amd64", &reference]);
        assert_eq!(code, 3);
        assert_eq!(err, "oops\n");
        assert_eq!(std::fs::read_dir(&disks).unwrap().count(), 1);

        // An older agent cannot run containers, and says so.
        std::fs::write(bundle.join("agent.version"), "1,2").unwrap();
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/amd64", &reference]);
        assert_eq!(code, 125);
        assert!(err.contains("cannot run containers; it needs 1.4"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn load_stores_an_archive_that_run_then_uses_by_name_or_id() {
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-load");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-load-guest");
        let env = with_bundle(&bundle, Some(&guest));
        let config = r#"{"architecture":"ARCH","config":{"Cmd":["from-the-archive"]}}"#;
        let dir = image::testing::layout(
            &qemu::testing::scratch("main-load-image"),
            &[("amd64", image::testing::layer())],
            config,
        );
        let tarball = image::testing::archive(&dir);
        let (code, out, err) = run_with(&env, &["load", "-i", tarball.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        let id = out
            .strip_prefix("Loaded image ID: ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("{out}"))
            .to_string();
        let (code, _, err) = run_with(&env, &["run", "--platform", "linux/amd64", &id]);
        assert_eq!(code, 3, "{err}");
        assert!(
            std::fs::read_to_string(bundle.join("run.seen"))
                .unwrap()
                .contains("from-the-archive")
        );

        let (code, _, err) = run_with(&env, &["load", "-i", "/nonexistent/image.tar"]);
        assert_eq!(code, 1);
        assert!(err.starts_with("oci-hermit: "), "{err}");
    }

    /// The architecture this host would not pick by itself.
    fn foreign() -> (qemu::GuestArch, &'static str) {
        let arch = other_arch(qemu::GuestArch::host().unwrap());
        (arch, image::oci_arch(arch))
    }

    #[test]
    #[cfg(unix)]
    fn run_falls_back_to_the_only_architecture_an_image_has() {
        let (_, foreign) = foreign();
        let native = image::oci_arch(qemu::GuestArch::host().unwrap());
        let warning = format!(
            "WARNING: the image has no linux/{native} variant; running its linux/{foreign} variant (choose with --platform)\n"
        );
        let _guard = qemu::testing::exec_lock();
        let bundle = qemu::testing::scratch("main-fallback");
        qemu::testing::fake_bundle(&bundle, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let guest = fake_guest("main-fallback-guest");
        let address = registry::testing::registry_of("main-fallback-registry", None, false, 0, &[foreign], true);
        let env = Env {
            registry: registry::Settings {
                insecure: vec![address.clone()],
                ..registry::Settings::default()
            },
            ..with_bundle(&bundle, Some(&guest))
        };

        // A local layout.
        let layout = image::testing::layout(
            &qemu::testing::scratch("main-fallback-layout"),
            &[(foreign, image::testing::layer())],
            r#"{"architecture":"ARCH","config":{"Cmd":["sh"]}}"#,
        );
        let (code, _, err) = run_with(&env, &["run", layout.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        assert!(err.starts_with(&warning), "{err}");
        assert!(bundle.join("run.seen").exists());

        // Pulled: the host's platform is not there, the other one is.
        let reference = format!("{address}/test/app:1");
        let (code, _, err) = run_with(&env, &["run", &reference]);
        assert_eq!(code, 3, "{err}");
        assert!(
            err.starts_with(&format!("Unable to find image '{reference}' locally\n")),
            "{err}"
        );
        assert!(err.contains(&warning), "{err}");
        // Stored now, so no pull; asking for the host's platform outright is still an error.
        let (code, _, err) = run_with(&env, &["run", &reference]);
        assert_eq!(code, 3, "{err}");
        assert_eq!(err, format!("{warning}oops\n"));
        let (code, _, err) = run_with(&env, &["run", "--platform", &format!("linux/{native}"), &reference]);
        assert_eq!(code, 125);
        assert!(err.contains(&format!("has no linux/{native} variant")), "{err}");
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
