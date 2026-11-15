//! Launching and supervising one sandbox VM.
//!
//! Each sandbox is one QEMU process, started as the current user with software emulation only and a fixed, minimal
//! device set: no default devices, no user config, no display, no network (#26), and the serial console written to
//! a log file. QEMU exits when this process exits, whether it stops cleanly or is killed: through QEMU's
//! `exit-with-parent` on Linux and macOS, and through a kill-on-close Job Object on Windows, where QEMU has no such
//! option. A [`Sandbox`] that is dropped kills its QEMU as well.

use crate::qemu::{self, GuestArch};
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The x86_64 guest CPU model. `qemu64` lacks the AVX the static agent's startup code uses; whether `max` is the
/// right model is settled in #54.
pub const X86_64_CPU: &str = "max";
/// The aarch64 guest CPU model.
pub const AARCH64_CPU: &str = "max";

/// The size of a sandbox VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSpec {
    pub arch: GuestArch,
    pub memory_mib: u32,
    pub cpus: u32,
}

/// Everything one QEMU launch needs.
#[derive(Debug)]
pub struct Launch<'a> {
    pub bundle: &'a Path,
    pub kernel: &'a Path,
    pub initrd: &'a Path,
    pub console_log: &'a Path,
    pub spec: MachineSpec,
}

/// Why a sandbox could not be started or did not come up.
#[derive(Debug, PartialEq, Eq)]
pub enum SupervisorError {
    /// A path QEMU takes inside an option string is not valid UTF-8.
    NonUtf8Path(PathBuf),
    /// Starting, watching or stopping QEMU failed.
    Io(String),
    /// QEMU exited before the console showed what was waited for.
    Exited { status: String },
    /// The console did not show what was waited for in time.
    TimedOut { after: Duration },
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupervisorError::NonUtf8Path(path) => write!(f, "path is not valid UTF-8: {}", path.display()),
            SupervisorError::Io(e) => write!(f, "{e}"),
            SupervisorError::Exited { status } => write!(f, "QEMU exited before the guest was ready ({status})"),
            SupervisorError::TimedOut { after } => {
                write!(f, "the guest was not ready after {}s", after.as_secs())
            }
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<io::Error> for SupervisorError {
    fn from(e: io::Error) -> Self {
        SupervisorError::Io(e.to_string())
    }
}

/// A value inside a QEMU option string, where a comma separates options and `,,` is a literal comma.
fn option_value(path: &Path) -> Result<String, SupervisorError> {
    let text = path
        .to_str()
        .ok_or_else(|| SupervisorError::NonUtf8Path(path.to_path_buf()))?;
    Ok(text.replace(',', ",,"))
}

/// The kernel command line: the machine's serial console, and on a panic, exit at once (QEMU runs `-no-reboot`).
pub fn kernel_cmdline(arch: GuestArch) -> &'static str {
    match arch {
        GuestArch::X86_64 => "console=ttyS0 panic=-1",
        GuestArch::Aarch64 => "console=ttyAMA0 panic=-1",
    }
}

/// The QEMU binary and arguments for one launch.
pub fn qemu_command(launch: &Launch<'_>) -> Result<(PathBuf, Vec<OsString>), SupervisorError> {
    let arch = launch.spec.arch;
    let binary = launch.bundle.join("bin").join(arch.binary_name());
    let (machine, cpu) = match arch {
        GuestArch::X86_64 => ("microvm", X86_64_CPU),
        GuestArch::Aarch64 => ("virt", AARCH64_CPU),
    };
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(OsString::from));
    push(&["-nodefaults", "-no-user-config", "-display", "none", "-no-reboot"]);
    push(&["-accel", "tcg", "-machine", machine, "-cpu", cpu]);
    push(&[
        "-m",
        &launch.spec.memory_mib.to_string(),
        "-smp",
        &launch.spec.cpus.to_string(),
    ]);
    // QEMU accepts -run-with only on POSIX hosts; on Windows the Job Object in `Sandbox::spawn` does this.
    if cfg!(unix) {
        push(&["-run-with", "exit-with-parent=on"]);
    }
    let console = format!("file,id=console,path={}", option_value(launch.console_log)?);
    push(&["-chardev", &console, "-serial", "chardev:console"]);
    push(&["-append", kernel_cmdline(arch)]);
    args.extend([
        OsString::from("-L"),
        qemu::firmware_dir(launch.bundle).into_os_string(),
        OsString::from("-kernel"),
        launch.kernel.as_os_str().to_owned(),
        OsString::from("-initrd"),
        launch.initrd.as_os_str().to_owned(),
    ]);
    Ok((binary, args))
}

/// A fresh directory for one sandbox's files: its initramfs, console log and QEMU's own error output.
pub fn run_dir(base: &Path) -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = base.join(format!("sandbox-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// One running QEMU process.
#[derive(Debug)]
pub struct Sandbox {
    child: Option<Child>,
    console_log: PathBuf,
    /// Windows closes this job's handle when oci-hermit exits, however it exits, and that kills QEMU.
    #[cfg(windows)]
    _job: win32job::Job,
}

impl Sandbox {
    /// Start `binary` with `args`, its own output going to `qemu_log`; the guest console goes to `console_log`.
    pub fn spawn(binary: &Path, args: &[OsString], qemu_log: &Path, console_log: &Path) -> io::Result<Sandbox> {
        let log = File::create(qemu_log)?;
        let child = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        #[cfg(windows)]
        let job = match kill_on_close(&child) {
            Ok(job) => job,
            Err(e) => {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        Ok(Sandbox {
            child: Some(child),
            console_log: console_log.to_path_buf(),
            #[cfg(windows)]
            _job: job,
        })
    }

    /// Wait until the console log contains `marker`, and return how long that took.
    pub fn wait_for_console(&mut self, marker: &str, timeout: Duration) -> Result<Duration, SupervisorError> {
        let start = Instant::now();
        loop {
            let console = std::fs::read(&self.console_log).unwrap_or_default();
            if String::from_utf8_lossy(&console).contains(marker) {
                return Ok(start.elapsed());
            }
            let exited = match self.child.as_mut() {
                Some(child) => child.try_wait()?,
                None => None,
            };
            if let Some(status) = exited {
                self.child = None;
                return Err(SupervisorError::Exited {
                    status: status.to_string(),
                });
            }
            if start.elapsed() >= timeout {
                return Err(SupervisorError::TimedOut { after: timeout });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stop QEMU and wait for it to exit. Stopping a sandbox that already stopped does nothing.
    pub fn stop(&mut self) -> io::Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.kill()?;
                child.wait()?;
            }
        }
        Ok(())
    }
}

/// A Job Object holding `child` that Windows kills, with everything in it, once its last handle closes.
#[cfg(windows)]
fn kill_on_close(child: &Child) -> io::Result<win32job::Job> {
    use std::os::windows::io::AsRawHandle;
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info).map_err(io::Error::other)?;
    job.assign_process(child.as_raw_handle() as isize)
        .map_err(io::Error::other)?;
    Ok(job)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    fn args_of(arch: GuestArch, console: &str) -> (PathBuf, Vec<String>) {
        let launch = Launch {
            bundle: Path::new("/b"),
            kernel: Path::new("/g/kernel"),
            initrd: Path::new("/r/initrd.cpio"),
            console_log: Path::new(console),
            spec: MachineSpec {
                arch,
                memory_mib: 256,
                cpus: 2,
            },
        };
        let (binary, args) = qemu_command(&launch).unwrap();
        (binary, args.into_iter().map(|a| a.into_string().unwrap()).collect())
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag}"));
        &args[i + 1]
    }

    #[test]
    fn launches_software_emulation_only() {
        for arch in GuestArch::ALL {
            let (_, args) = args_of(arch, "/r/console.log");
            assert_eq!(value_after(&args, "-accel"), "tcg");
            assert_eq!(args.iter().filter(|a| *a == "-accel").count(), 1);
            let joined = args.join(" ");
            for accelerator in ["kvm", "hvf", "whpx", "xen", "nvmm"] {
                assert!(!joined.contains(accelerator), "{accelerator} in {joined}");
            }
        }
    }

    #[test]
    fn has_no_default_devices_display_or_network() {
        let (_, args) = args_of(GuestArch::X86_64, "/r/console.log");
        for flag in ["-nodefaults", "-no-user-config", "-no-reboot"] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}");
        }
        assert_eq!(value_after(&args, "-display"), "none");
        for flag in ["-netdev", "-nic", "-net", "-device", "-drive", "-usb"] {
            assert!(!args.iter().any(|a| a == flag), "unexpected {flag}");
        }
    }

    #[test]
    fn exits_with_the_host_tool() {
        let (_, args) = args_of(GuestArch::Aarch64, "/r/console.log");
        if cfg!(unix) {
            assert_eq!(value_after(&args, "-run-with"), "exit-with-parent=on");
        } else {
            assert!(
                !args.iter().any(|a| a == "-run-with"),
                "QEMU rejects -run-with on Windows"
            );
        }
    }

    #[test]
    fn writes_the_console_to_the_log() {
        let (_, args) = args_of(GuestArch::Aarch64, "/r/console.log");
        assert_eq!(value_after(&args, "-chardev"), "file,id=console,path=/r/console.log");
        assert_eq!(value_after(&args, "-serial"), "chardev:console");
        assert_eq!(value_after(&args, "-append"), "console=ttyAMA0 panic=-1");
    }

    #[test]
    fn escapes_commas_in_the_console_path() {
        let (_, args) = args_of(GuestArch::X86_64, "/r/a,b/console.log");
        assert_eq!(
            value_after(&args, "-chardev"),
            "file,id=console,path=/r/a,,b/console.log"
        );
    }

    #[test]
    fn picks_the_machine_cpu_and_emulator_per_arch() {
        let (binary, args) = args_of(GuestArch::X86_64, "/r/c");
        assert!(binary.ends_with(GuestArch::X86_64.binary_name()));
        assert_eq!(value_after(&args, "-machine"), "microvm");
        assert_eq!(value_after(&args, "-cpu"), X86_64_CPU);
        assert_eq!(value_after(&args, "-append"), "console=ttyS0 panic=-1");
        assert_eq!(value_after(&args, "-L"), Path::new("/b/share/qemu").to_str().unwrap());
        let (binary, args) = args_of(GuestArch::Aarch64, "/r/c");
        assert!(binary.ends_with(GuestArch::Aarch64.binary_name()));
        assert_eq!(value_after(&args, "-machine"), "virt");
        assert_eq!(value_after(&args, "-cpu"), AARCH64_CPU);
        assert_eq!(value_after(&args, "-m"), "256");
        assert_eq!(value_after(&args, "-smp"), "2");
        assert_eq!(value_after(&args, "-kernel"), "/g/kernel");
        assert_eq!(value_after(&args, "-initrd"), "/r/initrd.cpio");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_utf8_console_path() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/r/\xff"));
        let err = option_value(path).unwrap_err();
        assert!(matches!(err, SupervisorError::NonUtf8Path(_)));
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn run_dirs_are_fresh() {
        let base = scratch("run-dirs");
        let a = run_dir(&base).unwrap();
        let b = run_dir(&base).unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn errors_say_what_happened() {
        let exited = SupervisorError::Exited {
            status: "exit status: 1".into(),
        };
        assert_eq!(
            exited.to_string(),
            "QEMU exited before the guest was ready (exit status: 1)"
        );
        let timed_out = SupervisorError::TimedOut {
            after: Duration::from_secs(30),
        };
        assert_eq!(timed_out.to_string(), "the guest was not ready after 30s");
        let io: SupervisorError = io::Error::other("denied").into();
        assert_eq!(io.to_string(), "denied");
    }

    /// What happens when oci-hermit dies on Windows: its handle to the job closes, and Windows kills QEMU.
    #[cfg(windows)]
    #[test]
    fn closing_the_job_kills_the_process() {
        let mut child = Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let job = kill_on_close(&child).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "still running while the job is open"
        );
        drop(job);
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "closing the job did not kill the process"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use crate::qemu::testing::exec_lock;
        use std::os::unix::fs::PermissionsExt;

        /// A shell script standing in for QEMU: it runs `body` with the console log path as `$1`.
        fn script(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("fake-qemu");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn spawn(name: &str, body: &str) -> (Sandbox, PathBuf) {
            let dir = scratch(name);
            let console = dir.join("console.log");
            let binary = script(&dir, body);
            let sandbox = Sandbox::spawn(
                &binary,
                &[console.clone().into_os_string()],
                &dir.join("qemu.log"),
                &console,
            )
            .unwrap();
            (sandbox, dir)
        }

        #[test]
        fn waits_for_the_console_marker() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn("sup-ready", "sleep 0.2; echo 'hermit-agent: ready' >> \"$1\"; sleep 30");
            let took = sandbox
                .wait_for_console("hermit-agent: ready", Duration::from_secs(10))
                .unwrap();
            assert!(took >= Duration::from_millis(100));
            sandbox.stop().unwrap();
            sandbox.stop().unwrap();
        }

        #[test]
        fn reports_qemu_exiting_first() {
            let _guard = exec_lock();
            let (mut sandbox, dir) = spawn("sup-exit", "echo 'bad option' >&2; exit 1");
            let err = sandbox.wait_for_console("ready", Duration::from_secs(10)).unwrap_err();
            assert!(matches!(err, SupervisorError::Exited { .. }), "{err}");
            let qemu_log = std::fs::read_to_string(dir.join("qemu.log")).unwrap();
            assert!(qemu_log.contains("bad option"));
        }

        #[test]
        fn times_out_and_stops_qemu() {
            let _guard = exec_lock();
            let (mut sandbox, _) = spawn("sup-timeout", "sleep 30");
            let err = sandbox
                .wait_for_console("ready", Duration::from_millis(300))
                .unwrap_err();
            assert!(matches!(err, SupervisorError::TimedOut { .. }));
            let start = Instant::now();
            drop(sandbox);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "drop kills QEMU instead of waiting for it"
            );
        }
    }
}
