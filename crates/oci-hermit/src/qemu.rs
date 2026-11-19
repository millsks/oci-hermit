//! Finding and checking the bundled QEMU.
//!
//! oci-hermit only runs the QEMU that `qemu/build.sh` produced: a TCG-only build whose version string carries the
//! bundle marker. Any other QEMU, even the same release from elsewhere, is refused, so the device and accelerator set
//! the sandbox relies on is always the one we built.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The QEMU release `qemu/build.sh` builds.
pub const QEMU_VERSION: &str = "11.1.3";
/// The `--with-pkgversion` marker `qemu/build.sh` builds with.
pub const BUNDLE_MARKER: &str = "oci-hermit-qemu-4";
/// Environment variable naming a bundle directory, overriding the search.
pub const DIR_ENV: &str = "OCI_HERMIT_QEMU_DIR";
/// The firmware the x86 `microvm` machine boots with: its SeaBIOS, and the option ROM SeaBIOS needs to boot a
/// `-kernel` bzImage.
pub const MICROVM_FIRMWARE: [&str; 2] = ["bios-microvm.bin", "linuxboot_dma.bin"];

/// The bundle's firmware directory. QEMU is always given it with `-L`: with the bundle's `/` prefix, QEMU's own
/// relocation looks only in a literal `/share/qemu`.
pub fn firmware_dir(bundle: &Path) -> PathBuf {
    bundle.join("share").join("qemu")
}

/// A guest CPU architecture, and so one of the two emulators in the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GuestArch {
    #[value(name = "x86_64")]
    X86_64,
    #[value(name = "aarch64")]
    Aarch64,
}

impl GuestArch {
    pub const ALL: [GuestArch; 2] = [GuestArch::X86_64, GuestArch::Aarch64];

    /// The architecture's usual name: `x86_64` or `aarch64`.
    pub fn name(self) -> &'static str {
        match self {
            GuestArch::X86_64 => "x86_64",
            GuestArch::Aarch64 => "aarch64",
        }
    }

    /// The host's own CPU architecture, if it is one of the two guest architectures.
    pub fn host() -> Option<GuestArch> {
        match std::env::consts::ARCH {
            "x86_64" => Some(GuestArch::X86_64),
            "aarch64" => Some(GuestArch::Aarch64),
            _ => None,
        }
    }

    /// The emulator's file name on this host.
    pub fn binary_name(self) -> String {
        let base = match self {
            GuestArch::X86_64 => "qemu-system-x86_64",
            GuestArch::Aarch64 => "qemu-system-aarch64",
        };
        format!("{base}{}", std::env::consts::EXE_SUFFIX)
    }
}

/// Why no usable QEMU was found.
#[derive(Debug, PartialEq, Eq)]
pub enum QemuError {
    /// No candidate directory holds the emulators.
    NotFound { searched: Vec<PathBuf> },
    /// The emulator could not be run.
    Unrunnable { binary: PathBuf, reason: String },
    /// The emulator runs but is not the bundled build.
    Unknown { binary: PathBuf, found: String },
    /// The emulator has no TCG accelerator.
    NoTcg { binary: PathBuf },
    /// Firmware the emulator needs is not in the bundle.
    MissingFirmware { path: PathBuf },
}

impl fmt::Display for QemuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QemuError::NotFound { searched } => {
                write!(f, "no bundled QEMU found; looked in")?;
                for dir in searched {
                    write!(f, " {}", dir.display())?;
                }
                write!(f, " (set {DIR_ENV} to a bundle built by qemu/build.sh)")
            }
            QemuError::Unrunnable { binary, reason } => write!(f, "cannot run {}: {reason}", binary.display()),
            QemuError::Unknown { binary, found } => write!(
                f,
                "{} is not the bundled QEMU: it reports {found:?}, expected version {QEMU_VERSION} ({BUNDLE_MARKER})",
                binary.display()
            ),
            QemuError::NoTcg { binary } => write!(f, "{} has no TCG accelerator", binary.display()),
            QemuError::MissingFirmware { path } => write!(f, "bundled firmware {} is missing", path.display()),
        }
    }
}

impl std::error::Error for QemuError {}

/// The directories searched for a bundle, in order: the override, then the install layouts beside the executable.
pub fn candidates(override_dir: Option<OsString>, exe: &Path) -> Vec<PathBuf> {
    install_dirs(override_dir, exe, "qemu")
}

/// Where a part of oci-hermit named `leaf` (`qemu`, `guest`) is looked for: the override, then a prefix install
/// (conda, pixi global: `<prefix>/bin/oci-hermit` and `<prefix>/libexec/oci-hermit/<leaf>`), then an unpacked
/// release archive (`oci-hermit` beside a `<leaf>/` directory).
pub fn install_dirs(override_dir: Option<OsString>, exe: &Path, leaf: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(bin) = exe.parent() {
        if let Some(prefix) = bin.parent() {
            dirs.push(prefix.join("libexec").join("oci-hermit").join(leaf));
        }
        dirs.push(bin.join(leaf));
    }
    dirs
}

/// The first candidate that contains both emulators.
pub fn locate(candidates: &[PathBuf]) -> Result<PathBuf, QemuError> {
    candidates
        .iter()
        .find(|dir| {
            GuestArch::ALL
                .iter()
                .all(|arch| dir.join("bin").join(arch.binary_name()).is_file())
        })
        .cloned()
        .ok_or_else(|| QemuError::NotFound {
            searched: candidates.to_vec(),
        })
}

/// What `--version` reported: the release and the package marker in parentheses, if any.
#[derive(Debug, PartialEq, Eq)]
pub struct VersionInfo {
    pub version: String,
    pub marker: Option<String>,
}

/// Parse the first line of `qemu-system-* --version`, e.g. `QEMU emulator version 11.1.3 (oci-hermit-qemu-4)`.
pub fn parse_version(output: &str) -> Option<VersionInfo> {
    let rest = output.lines().next()?.trim().strip_prefix("QEMU emulator version ")?;
    let (version, marker) = match rest.split_once(' ') {
        Some((version, tail)) => {
            let marker = tail.trim().strip_prefix('(').and_then(|t| t.strip_suffix(')'));
            (version, marker.map(str::to_owned))
        }
        None => (rest, None),
    };
    Some(VersionInfo {
        version: version.to_owned(),
        marker,
    })
}

/// A bundled emulator that passed every check.
#[derive(Debug, PartialEq, Eq)]
pub struct Emulator {
    pub arch: GuestArch,
    pub binary: PathBuf,
    pub version: VersionInfo,
}

fn output_of(binary: &Path, args: &[&str]) -> Result<String, QemuError> {
    let unrunnable = |reason: String| QemuError::Unrunnable {
        binary: binary.to_path_buf(),
        reason,
    };
    let output = Command::new(binary)
        .args(args)
        .output()
        .map_err(|e| unrunnable(e.to_string()))?;
    if !output.status.success() {
        return Err(unrunnable(format!(
            "`{}` exited with {}",
            args.join(" "),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Check that the bundle's emulator for `arch` is the build oci-hermit expects and can run TCG.
pub fn verify(bundle: &Path, arch: GuestArch) -> Result<Emulator, QemuError> {
    let binary = bundle.join("bin").join(arch.binary_name());
    if arch == GuestArch::X86_64 {
        for name in MICROVM_FIRMWARE {
            let firmware = firmware_dir(bundle).join(name);
            if !firmware.is_file() {
                return Err(QemuError::MissingFirmware { path: firmware });
            }
        }
    }
    let reported = output_of(&binary, &["--version"])?;
    let version = parse_version(&reported)
        .filter(|v| v.version == QEMU_VERSION && v.marker.as_deref() == Some(BUNDLE_MARKER))
        .ok_or_else(|| QemuError::Unknown {
            binary: binary.clone(),
            found: reported.lines().next().unwrap_or("").trim().to_owned(),
        })?;
    let accels = output_of(&binary, &["-accel", "help"])?;
    if !accels.lines().any(|line| line.trim() == "tcg") {
        return Err(QemuError::NoTcg { binary });
    }
    Ok(Emulator { arch, binary, version })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// An empty directory with a `bin/` in it, unique to this test process. On Unix it is under `/tmp`, which keeps
    /// the containers' socket paths short (macOS's own temporary folders are too deep for them).
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("oci-hermit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        dir
    }

    /// Held by every test that writes and then runs a fake emulator. On Linux, executing a script fails with
    /// ETXTBSY while another thread's freshly forked child still holds it open for writing, so those tests run one
    /// at a time.
    #[cfg(unix)]
    pub(crate) fn exec_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Shell scripts standing in for both emulators. They answer `--version` and `-accel help`; launched as a
    /// sandbox, they play the agent on their stdin and stdout: `Ready` to the hello, with protocol 1.4 unless
    /// `<bundle>/agent.version` says otherwise (as `1,1`); for a `Run`, `Started`, `hi` on stdout, `oops` on stderr and
    /// exit code 3 (the `Run` line is saved to `<bundle>/run.seen`); then `ShuttingDown` and exit. A command with a
    /// `wait` argument prints `hi`, echoes its stdin and runs until the host shuts the guest down.
    #[cfg(unix)]
    pub(crate) fn fake_bundle(dir: &Path, version_line: &str, accels: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --version) echo '{version_line}' ;;\n\
             -accel) printf 'Accelerators supported in QEMU binary:\\n{accels}\\n' ;;\n\
             *) read hello; v=$(cat \"$(dirname \"$0\")/../agent.version\" 2>/dev/null || echo 1,4); \
             echo \"{{\\\"type\\\":\\\"ready\\\",\\\"version\\\":[$v]}}\"; read next; \
             case \"$next\" in *'\"run\"'*) echo \"$next\" > \"$(dirname \"$0\")/../run.seen\"; \
             echo '{{\"type\":\"started\",\"pid\":7}}'; \
             echo '{{\"type\":\"output\",\"stream\":\"stdout\",\"data\":\"aGkK\"}}'; \
             case \"$next\" in *'\"wait\"'*) while read l; do case \"$l\" in \
             *'\"stdin\"'*) echo \"$l\" | sed 's/\"type\":\"stdin\"/\"type\":\"output\",\"stream\":\"stdout\"/' ;; \
             *shutdown*) break ;; esac; done ;; \
             *) echo '{{\"type\":\"output\",\"stream\":\"stderr\",\"data\":\"b29wcwo=\"}}'; \
             echo '{{\"type\":\"exited\",\"code\":3}}'; read closed; read bye ;; esac ;; esac; \
             echo '{{\"type\":\"shutting_down\"}}'; exit 0 ;;\n\
             esac\n"
        );
        for arch in GuestArch::ALL {
            let path = dir.join("bin").join(arch.binary_name());
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let firmware = firmware_dir(dir);
        std::fs::create_dir_all(&firmware).unwrap();
        for name in MICROVM_FIRMWARE {
            std::fs::write(firmware.join(name), "").unwrap();
        }
        dir.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::scratch;
    use super::*;

    #[test]
    fn parses_bundled_version() {
        let info = parse_version("QEMU emulator version 11.1.3 (oci-hermit-qemu-4)\nCopyright (c) 2003-2026").unwrap();
        assert_eq!(info.version, "11.1.3");
        assert_eq!(info.marker.as_deref(), Some("oci-hermit-qemu-4"));
    }

    #[test]
    fn parses_distro_version_and_rejects_garbage() {
        let info = parse_version("QEMU emulator version 11.1.3 (Debian 1:11.1.3+ds-1)").unwrap();
        assert_eq!(info.marker.as_deref(), Some("Debian 1:11.1.3+ds-1"));
        assert_eq!(parse_version("QEMU emulator version 9.2.0").unwrap().marker, None);
        assert_eq!(parse_version("not qemu"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn candidates_put_the_override_first() {
        let exe = Path::new("/opt/hermit/bin/oci-hermit");
        let dirs = candidates(Some("/custom".into()), exe);
        assert_eq!(
            dirs,
            [
                PathBuf::from("/custom"),
                PathBuf::from("/opt/hermit/libexec/oci-hermit/qemu"),
                PathBuf::from("/opt/hermit/bin/qemu"),
            ]
        );
        assert_eq!(candidates(Some("".into()), exe).len(), 2);
    }

    #[test]
    fn locate_needs_both_emulators() {
        let dir = scratch("locate");
        let missing = scratch("locate-missing");
        std::fs::write(dir.join("bin").join(GuestArch::X86_64.binary_name()), "").unwrap();
        assert!(matches!(
            locate(std::slice::from_ref(&dir)),
            Err(QemuError::NotFound { .. })
        ));
        std::fs::write(dir.join("bin").join(GuestArch::Aarch64.binary_name()), "").unwrap();
        assert_eq!(locate(&[missing.clone(), dir.clone()]).unwrap(), dir);
        let err = locate(std::slice::from_ref(&missing)).unwrap_err();
        assert!(err.to_string().contains(DIR_ENV));
    }

    #[test]
    fn errors_name_the_binary() {
        let binary = PathBuf::from("/x/qemu");
        let unknown = QemuError::Unknown {
            binary: binary.clone(),
            found: "QEMU emulator version 9.2.0".into(),
        };
        assert!(
            unknown
                .to_string()
                .contains("expected version 11.1.3 (oci-hermit-qemu-4)")
        );
        assert!(
            QemuError::NoTcg { binary: binary.clone() }
                .to_string()
                .contains("/x/qemu")
        );
        let unrunnable = QemuError::Unrunnable {
            binary,
            reason: "denied".into(),
        };
        assert_eq!(unrunnable.to_string(), "cannot run /x/qemu: denied");
    }

    #[cfg(unix)]
    mod fake {
        use super::*;
        use crate::qemu::testing::{exec_lock, fake_bundle};
        use std::os::unix::fs::PermissionsExt;

        fn fake_qemu(name: &str, version_line: &str, accels: &str) -> PathBuf {
            fake_bundle(&scratch(name), version_line, accels)
        }

        #[test]
        fn accepts_the_bundled_build() {
            let _guard = exec_lock();
            let dir = fake_qemu("ok", "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
            let emulator = verify(&dir, GuestArch::X86_64).unwrap();
            assert_eq!(emulator.version.version, QEMU_VERSION);
            assert_eq!(emulator.arch, GuestArch::X86_64);
        }

        #[test]
        fn refuses_another_build_of_the_same_release() {
            let _guard = exec_lock();
            let dir = fake_qemu("distro", "QEMU emulator version 11.1.3 (Debian 1:11.1.3+ds-1)", "tcg");
            assert!(matches!(
                verify(&dir, GuestArch::X86_64),
                Err(QemuError::Unknown { .. })
            ));
        }

        #[test]
        fn refuses_a_build_without_tcg() {
            let _guard = exec_lock();
            let dir = fake_qemu("notcg", "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "kvm");
            assert!(matches!(verify(&dir, GuestArch::X86_64), Err(QemuError::NoTcg { .. })));
        }

        #[test]
        fn refuses_a_bundle_without_microvm_firmware() {
            let _guard = exec_lock();
            let dir = fake_qemu("nofw", "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
            for name in MICROVM_FIRMWARE {
                std::fs::write(firmware_dir(&dir).join(MICROVM_FIRMWARE[0]), "").unwrap();
                std::fs::write(firmware_dir(&dir).join(MICROVM_FIRMWARE[1]), "").unwrap();
                std::fs::remove_file(firmware_dir(&dir).join(name)).unwrap();
                let err = verify(&dir, GuestArch::X86_64).unwrap_err();
                assert!(err.to_string().contains(&format!("{name} is missing")), "{err}");
                assert!(verify(&dir, GuestArch::Aarch64).is_ok());
            }
        }

        #[test]
        fn reports_a_missing_or_failing_binary() {
            let _guard = exec_lock();
            let dir = scratch("absent");
            let firmware = firmware_dir(&dir);
            std::fs::create_dir_all(&firmware).unwrap();
            for name in MICROVM_FIRMWARE {
                std::fs::write(firmware.join(name), "").unwrap();
            }
            assert!(matches!(
                verify(&dir, GuestArch::X86_64),
                Err(QemuError::Unrunnable { .. })
            ));
            let path = dir.join("bin").join(GuestArch::X86_64.binary_name());
            std::fs::write(&path, "#!/bin/sh\nexit 3\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let err = verify(&dir, GuestArch::X86_64).unwrap_err();
            assert!(err.to_string().contains("exited with"));
        }
    }
}
