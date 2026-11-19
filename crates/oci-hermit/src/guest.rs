//! Finding the guest: the kernel, agent and crun each sandbox VM boots.
//!
//! A guest directory holds one subdirectory per architecture:
//!
//! ```text
//! <guest>/x86_64/kernel         the bzImage from guest/kernel/build.sh
//! <guest>/x86_64/hermit-agent   the static agent from `pixi run -e agent agent-build`
//! <guest>/x86_64/crun           the static crun from guest/crun/build.sh
//! <guest>/aarch64/kernel        the arm64 Image
//! <guest>/aarch64/hermit-agent
//! <guest>/aarch64/crun
//! ```

use crate::qemu::{GuestArch, install_dirs};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Environment variable naming a guest directory, overriding the search.
pub const DIR_ENV: &str = "OCI_HERMIT_GUEST_DIR";

/// The kernel, agent and container runtime for one architecture.
#[derive(Debug, PartialEq, Eq)]
pub struct Guest {
    pub arch: GuestArch,
    pub kernel: PathBuf,
    pub agent: PathBuf,
    pub crun: PathBuf,
}

/// The files a guest directory has for each architecture.
pub const FILES: [&str; 3] = ["kernel", "hermit-agent", "crun"];

/// No guest directory holds a kernel, agent and crun for the architecture.
#[derive(Debug, PartialEq, Eq)]
pub struct GuestNotFound {
    pub arch: GuestArch,
    pub searched: Vec<PathBuf>,
}

impl fmt::Display for GuestNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no {} guest kernel, agent and crun found; looked in",
            self.arch.name()
        )?;
        for dir in &self.searched {
            write!(f, " {}", dir.display())?;
        }
        let arch = self.arch.name();
        write!(
            f,
            " (set {DIR_ENV} to a directory with {arch}/kernel, {arch}/hermit-agent and {arch}/crun)"
        )
    }
}

impl std::error::Error for GuestNotFound {}

/// The directories searched for a guest, in order.
pub fn candidates(override_dir: Option<OsString>, exe: &Path) -> Vec<PathBuf> {
    install_dirs(override_dir, exe, "guest")
}

/// The first candidate with the kernel, the agent and crun for `arch`.
pub fn locate(candidates: &[PathBuf], arch: GuestArch) -> Result<Guest, GuestNotFound> {
    candidates
        .iter()
        .map(|dir| dir.join(arch.name()))
        .find(|dir| FILES.iter().all(|f| dir.join(f).is_file()))
        .map(|dir| Guest {
            arch,
            kernel: dir.join("kernel"),
            agent: dir.join("hermit-agent"),
            crun: dir.join("crun"),
        })
        .ok_or_else(|| GuestNotFound {
            arch,
            searched: candidates.to_vec(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    #[test]
    fn candidates_search_the_override_then_the_install_layouts() {
        let dirs = candidates(Some("/custom".into()), Path::new("/opt/hermit/bin/oci-hermit"));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/custom"),
                PathBuf::from("/opt/hermit/libexec/oci-hermit/guest"),
                PathBuf::from("/opt/hermit/bin/guest"),
            ]
        );
    }

    #[test]
    fn locate_needs_the_kernel_and_the_agent_for_that_arch() {
        let dir = scratch("guest-locate");
        let arm = dir.join("aarch64");
        std::fs::create_dir_all(&arm).unwrap();
        std::fs::write(arm.join("kernel"), "").unwrap();
        let err = locate(std::slice::from_ref(&dir), GuestArch::Aarch64).unwrap_err();
        assert!(
            err.to_string()
                .contains("no aarch64 guest kernel, agent and crun found")
        );
        assert!(err.to_string().contains("aarch64/hermit-agent and aarch64/crun"));

        // A guest from before crun (0.2 and older) is not enough.
        std::fs::write(arm.join("hermit-agent"), "").unwrap();
        assert!(locate(std::slice::from_ref(&dir), GuestArch::Aarch64).is_err());
        std::fs::write(arm.join("crun"), "").unwrap();
        let guest = locate(std::slice::from_ref(&dir), GuestArch::Aarch64).unwrap();
        assert_eq!(guest.kernel, arm.join("kernel"));
        assert_eq!(guest.agent, arm.join("hermit-agent"));
        assert_eq!(guest.crun, arm.join("crun"));
        assert!(locate(std::slice::from_ref(&dir), GuestArch::X86_64).is_err());
    }
}
