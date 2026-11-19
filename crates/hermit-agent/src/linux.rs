//! The real system calls, for the Linux guest.

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, ErrorKind, IoSliceMut, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::{Duration, Instant};

use hermit_proto::{CONTROL_PORT, Framer, Stream};
use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::mount::MsFlags;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::spawn::{PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags, posix_spawn};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::signal::{SigSet, Signal as NixSignal, kill};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, chdir, chroot, pipe2};

use crate::init::{self, Event, Mount, ROOTFS, Reaped, RunSpec, Signal, System};

/// crun, in the initramfs and in the root the agent moves to.
const CRUN: &str = "/bin/crun";

/// Where the agent's root moves, off the initramfs (see [`System::leave_initramfs`]).
const NEW_ROOT: &str = "/newroot";

/// The container's OCI runtime bundle (`config.json`), the files it bind-mounts and crun's console socket.
const BUNDLE: &str = "/run/oci-hermit";

/// The container's name in crun: one runs per sandbox.
const CONTAINER: &str = "hermit";

/// How long crun gets to hand over a container's terminal.
const CONSOLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The command a `Run` started: where its input goes and where its output comes from.
struct Running {
    tty: bool,
    stdin: Option<File>,
    outputs: Vec<(Stream, File)>,
}

fn context(what: &str) -> impl Fn(Errno) -> io::Error + '_ {
    move |e| io::Error::new(io::Error::from(e).kind(), format!("{what}: {e}"))
}

/// The virtio disks and their serial numbers, as `(name, serial)` from `/sys/block/vd*/serial`.
fn list_disks() -> io::Result<Vec<(String, String)>> {
    let mut disks = Vec::new();
    for entry in std::fs::read_dir("/sys/block")? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with("vd") {
            continue;
        }
        if let Ok(serial) = std::fs::read_to_string(format!("/sys/block/{name}/serial")) {
            disks.push((name, serial.trim().to_string()));
        }
    }
    Ok(disks)
}

/// `PATH` when the image's environment has none, as Docker uses.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Whether the image under `root` has `argv0` as an executable file: as a path (relative to `cwd`), or found in the
/// `PATH` from `env`. Paths resolve as inside the image, so its absolute symlinks (busybox's) point into it, and
/// nothing outside it can be reached. crun checks this too, but only after the container has started, and would
/// report it as the command's own exit status.
fn find_executable(root: &str, argv0: &str, env: &[String], cwd: &str) -> io::Result<()> {
    let root_fd = open(
        root,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(context(&format!("opening {root}")))?;
    let candidates: Vec<String> = if argv0.contains('/') {
        vec![if argv0.starts_with('/') {
            argv0.to_string()
        } else {
            format!("{cwd}/{argv0}")
        }]
    } else {
        let path = env
            .iter()
            .rev()
            .find_map(|e| e.strip_prefix("PATH="))
            .unwrap_or(DEFAULT_PATH);
        path.split(':')
            .map(|dir| if dir.is_empty() { cwd } else { dir })
            .map(|dir| format!("{dir}/{argv0}"))
            .collect()
    };
    for candidate in candidates {
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT);
        let Ok(fd) = openat2(&root_fd, candidate.as_str(), how) else {
            continue;
        };
        if let Ok(stat) = nix::sys::stat::fstat(&fd)
            && stat.st_mode & libc_mode::S_IFMT == libc_mode::S_IFREG
            && stat.st_mode & 0o111 != 0
        {
            return Ok(());
        }
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        "executable file not found in $PATH",
    ))
}

/// File type bits of `st_mode`.
mod libc_mode {
    pub const S_IFMT: u32 = 0o170_000;
    pub const S_IFREG: u32 = 0o100_000;
}

fn cstrings(items: &[String]) -> io::Result<Vec<CString>> {
    items
        .iter()
        .map(|s| {
            CString::new(s.as_str()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "a NUL byte in the command"))
        })
        .collect()
}

/// The container's terminal, as crun sends it on the console socket; `None` if crun ends first (its stderr, the
/// pipe in `crun_stderr`, hangs up).
fn receive_terminal(listener: &UnixListener, crun_stderr: &File) -> io::Result<Option<File>> {
    let deadline = Instant::now() + CONSOLE_TIMEOUT;
    loop {
        let (connected, ended) = {
            let mut fds = [
                PollFd::new(listener.as_fd(), PollFlags::POLLIN),
                PollFd::new(crun_stderr.as_fd(), PollFlags::empty()),
            ];
            match poll(&mut fds, PollTimeout::from(500u16)) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(e) => return Err(e.into()),
            }
            let has = |fd: &PollFd<'_>, flag| fd.revents().is_some_and(|r| r.intersects(flag));
            (
                has(&fds[0], PollFlags::POLLIN),
                has(&fds[1], PollFlags::POLLHUP | PollFlags::POLLERR),
            )
        };
        if connected {
            let (stream, _) = listener.accept()?;
            let mut buf = [0u8; 256];
            let mut iov = [IoSliceMut::new(&mut buf)];
            let mut space = nix::cmsg_space!(RawFd);
            let msg = recvmsg::<()>(
                stream.as_raw_fd(),
                &mut iov,
                Some(&mut space),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )
            .map_err(context("receiving the terminal"))?;
            for cmsg in msg.cmsgs().map_err(context("receiving the terminal"))? {
                if let ControlMessageOwned::ScmRights(fds) = cmsg {
                    let mut owned: Vec<OwnedFd> = fds.into_iter().map(owned_fd).collect();
                    if !owned.is_empty() {
                        return Ok(Some(File::from(owned.swap_remove(0))));
                    }
                }
            }
            return Err(io::Error::new(ErrorKind::InvalidData, "crun sent no terminal"));
        }
        if ended {
            return Ok(None);
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "crun did not hand over the terminal",
            ));
        }
    }
}

/// Takes ownership of a file descriptor the kernel just passed this process over a socket.
#[expect(unsafe_code, reason = "nix hands over descriptors from SCM_RIGHTS as raw numbers")]
fn owned_fd(fd: RawFd) -> OwnedFd {
    // SAFETY: the kernel installed `fd` in this process for the message just received (SCM_RIGHTS, with
    // MSG_CMSG_CLOEXEC), and nothing else refers to it.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Where the kernel lists virtio-serial ports, each with a `name` file.
const VIRTIO_PORTS: &str = "/sys/class/virtio-ports";

/// Whether this process is PID 1.
pub fn is_pid1() -> bool {
    nix::unistd::getpid().as_raw() == 1
}

/// Linux implementation of [`System`]: the blocked signals arrive on a signalfd, and the control port is polled
/// alongside it.
#[derive(Default)]
pub struct Linux {
    signals: Option<SignalFd>,
    control: Option<File>,
    framer: Framer,
    pending: VecDeque<Event>,
    run: Option<Running>,
}

impl Linux {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the command's outputs that poll found ready; one that ends (EOF, or EIO from a terminal) is dropped.
    fn read_outputs(&mut self, ready: &[bool]) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let mut ended = Vec::new();
        for (i, (stream, file)) in run.outputs.iter_mut().enumerate() {
            if !ready.get(i).copied().unwrap_or(false) {
                continue;
            }
            match file.read(&mut buf) {
                Ok(0) => ended.push(i),
                Ok(n) => self.pending.push_back(Event::Output(*stream, buf[..n].to_vec())),
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Err(_) => ended.push(i),
            }
        }
        for i in ended.into_iter().rev() {
            run.outputs.remove(i);
        }
    }

    /// Read what the control port has, turning it into events.
    fn read_control(&mut self) {
        let Some(port) = self.control.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        match port.read(&mut buf) {
            Ok(0) => {
                self.control = None;
                self.pending.push_back(Event::ControlClosed);
            }
            Ok(n) => {
                let mut frames = Vec::new();
                self.framer.push(&buf[..n], &mut frames);
                self.pending.extend(frames.into_iter().map(Event::Control));
            }
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
            Err(_) => {
                self.control = None;
                self.pending.push_back(Event::ControlClosed);
            }
        }
    }
}

/// The device node of the virtio-serial port called `name`, found through sysfs.
fn find_port(ports: &Path, devices: &Path, name: &str) -> io::Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(ports)? {
        let entry = entry?;
        let port_name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if port_name.trim_end() == name {
            return Ok(devices.join(entry.file_name()));
        }
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        format!("no virtio-serial port named {name}"),
    ))
}

fn shutdown_signals() -> SigSet {
    let mut set = SigSet::empty();
    for sig in [NixSignal::SIGTERM, NixSignal::SIGINT, NixSignal::SIGCHLD] {
        set.add(sig);
    }
    set
}

impl System for Linux {
    fn mkdir_all(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn mount(&mut self, m: &Mount) -> io::Result<()> {
        let mut flags = MsFlags::empty();
        flags.set(MsFlags::MS_NOSUID, m.nosuid);
        flags.set(MsFlags::MS_NODEV, m.nodev);
        flags.set(MsFlags::MS_NOEXEC, m.noexec);
        nix::mount::mount(Some(m.source), m.target, Some(m.fstype), flags, m.data)?;
        Ok(())
    }

    fn block_signals(&mut self) -> io::Result<()> {
        let set = shutdown_signals();
        set.thread_block()?;
        self.signals = Some(SignalFd::with_flags(&set, SfdFlags::SFD_CLOEXEC)?);
        Ok(())
    }

    fn open_control(&mut self) -> io::Result<()> {
        let device = find_port(Path::new(VIRTIO_PORTS), Path::new("/dev"), CONTROL_PORT)?;
        self.control = Some(File::options().read(true).write(true).open(device)?);
        Ok(())
    }

    fn next_event(&mut self) -> io::Result<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            let signals = self
                .signals
                .as_ref()
                .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "signals are not blocked"))?;
            let (signal_ready, control_ready, outputs_ready) = {
                let mut fds = vec![PollFd::new(signals.as_fd(), PollFlags::POLLIN)];
                let has_control = self.control.is_some();
                if let Some(port) = &self.control {
                    fds.push(PollFd::new(port.as_fd(), PollFlags::POLLIN));
                }
                let first_output = fds.len();
                if let Some(run) = &self.run {
                    for (_, file) in &run.outputs {
                        fds.push(PollFd::new(file.as_fd(), PollFlags::POLLIN));
                    }
                }
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                let ready = |fd: &PollFd<'_>| fd.revents().is_some_and(|r| !r.is_empty());
                let outputs: Vec<bool> = fds[first_output..].iter().map(ready).collect();
                (ready(&fds[0]), has_control && ready(&fds[1]), outputs)
            };
            if signal_ready && let Some(info) = signals.read_signal()? {
                let signal = match NixSignal::try_from(info.ssi_signo as i32) {
                    Ok(NixSignal::SIGTERM) => Some(Signal::Term),
                    Ok(NixSignal::SIGINT) => Some(Signal::Int),
                    Ok(NixSignal::SIGCHLD) => Some(Signal::Child),
                    _ => None,
                };
                self.pending.extend(signal.map(Event::Signal));
            }
            if control_ready {
                self.read_control();
            }
            self.read_outputs(&outputs_ready);
        }
    }

    fn mount_root(&mut self, layers: &[String]) -> io::Result<()> {
        // The disks were probed before PID 1 started; a late one gets a few seconds.
        let mut disks = list_disks()?;
        for _ in 0..50 {
            if layers.iter().all(|l| disks.iter().any(|(_, serial)| serial == l)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            disks = list_disks()?;
        }
        let devices = init::layer_devices(layers, &disks)?;
        let fs_error = |what: String| move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
        for (i, device) in devices.iter().enumerate() {
            let target = init::layer_dir(i);
            std::fs::create_dir_all(&target).map_err(fs_error(format!("creating {target}")))?;
            nix::mount::mount(
                Some(device.as_str()),
                target.as_str(),
                Some("erofs"),
                MsFlags::MS_RDONLY,
                None::<&str>,
            )
            .map_err(context(&format!("mounting layer {i} ({device}) on {target}")))?;
        }
        let overlay = init::OVERLAY_DIR;
        std::fs::create_dir_all(overlay).map_err(fs_error(format!("creating {overlay}")))?;
        nix::mount::mount(
            Some("tmpfs"),
            overlay,
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=0755"),
        )
        .map_err(context("mounting the writable layer"))?;
        for dir in ["upper", "work"] {
            let path = format!("{overlay}/{dir}");
            std::fs::create_dir_all(&path).map_err(fs_error(format!("creating {path}")))?;
        }
        std::fs::create_dir_all(ROOTFS).map_err(fs_error(format!("creating {ROOTFS}")))?;
        let options = init::overlay_options(devices.len());
        nix::mount::mount(
            Some("overlay"),
            ROOTFS,
            Some("overlay"),
            MsFlags::empty(),
            Some(options.as_str()),
        )
        .map_err(context("stacking the layers with overlayfs"))?;
        Ok(())
    }

    fn leave_initramfs(&mut self) -> io::Result<()> {
        let fs_error = |what: String| move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
        if !Path::new(CRUN).is_file() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("no {CRUN} in the initramfs"),
            ));
        }
        std::fs::create_dir_all(NEW_ROOT).map_err(fs_error(format!("creating {NEW_ROOT}")))?;
        nix::mount::mount(
            Some("tmpfs"),
            NEW_ROOT,
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=0755"),
        )
        .map_err(context("mounting a tmpfs for the new root"))?;
        for dir in [
            "bin",
            "dev",
            "proc",
            "sys",
            "run",
            "tmp",
            ROOTFS.trim_start_matches('/'),
        ] {
            let path = format!("{NEW_ROOT}/{dir}");
            std::fs::create_dir_all(&path).map_err(fs_error(format!("creating {path}")))?;
        }
        let crun = format!("{NEW_ROOT}{CRUN}");
        std::fs::copy(CRUN, &crun).map_err(fs_error(format!("copying {CRUN}")))?;
        std::fs::set_permissions(&crun, std::fs::Permissions::from_mode(0o755))?;
        let _ = std::fs::remove_file(CRUN);
        // An image run from a file came in the initramfs; it stays where it is and is bound into the new root.
        if Path::new(ROOTFS).is_dir() {
            nix::mount::mount(
                Some(ROOTFS),
                format!("{NEW_ROOT}{}", ROOTFS).as_str(),
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .map_err(context("binding the image's files into the new root"))?;
        }
        // What switch_root does: the tmpfs becomes `/`, and the initramfs is out of reach below it.
        chdir(NEW_ROOT).map_err(context("entering the new root"))?;
        nix::mount::mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
            .map_err(context("moving the new root onto /"))?;
        chroot(".").map_err(context("entering the new root"))?;
        chdir("/").map_err(context("entering the new root"))?;
        Ok(())
    }

    fn start(&mut self, spec: &RunSpec) -> io::Result<i32> {
        if self.run.is_some() {
            return Err(io::Error::new(ErrorKind::AlreadyExists, "a command is already running"));
        }
        find_executable(ROOTFS, &spec.argv[0], &spec.env, &spec.cwd)?;
        let fs_error = |what: String| move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
        std::fs::create_dir_all(BUNDLE).map_err(fs_error(format!("creating {BUNDLE}")))?;
        std::fs::write(format!("{BUNDLE}/hosts"), init::hosts_file(&spec.hostname))?;
        std::fs::write(format!("{BUNDLE}/hostname"), format!("{}\n", spec.hostname))?;
        let config = serde_json::to_vec_pretty(&init::container_config(spec, BUNDLE)).map_err(io::Error::other)?;
        std::fs::write(format!("{BUNDLE}/config.json"), config).map_err(fs_error("writing config.json".into()))?;

        // With a terminal, crun makes it in the container and sends the agent its other end over this socket.
        let socket = format!("{BUNDLE}/console.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = if spec.tty {
            Some(UnixListener::bind(&socket).map_err(fs_error("listening for the terminal".into()))?)
        } else {
            None
        };
        let mut args = vec![CRUN, "--cgroup-manager=cgroupfs", "run", "--bundle", BUNDLE];
        if spec.tty {
            args.extend(["--console-socket", socket.as_str()]);
        }
        args.push(CONTAINER);
        let argv = cstrings(&args.iter().map(|a| (*a).to_string()).collect::<Vec<_>>())?;
        let envp = cstrings(&["PATH=/bin".to_string()])?;

        let mut attr = PosixSpawnAttr::init()?;
        // The agent blocks SIGTERM, SIGINT and SIGCHLD; crun and the container must start with nothing blocked.
        attr.set_sigmask(&SigSet::empty())?;
        let mut defaults = shutdown_signals();
        defaults.add(NixSignal::SIGHUP);
        attr.set_sigdefault(&defaults)?;
        attr.set_pgroup(Pid::from_raw(0))?;
        attr.set_flags(
            PosixSpawnFlags::POSIX_SPAWN_SETSIGMASK
                | PosixSpawnFlags::POSIX_SPAWN_SETSIGDEF
                | PosixSpawnFlags::POSIX_SPAWN_SETPGROUP,
        )?;
        let mut actions = PosixSpawnFileActions::init()?;
        // crun's own output carries its errors; without a terminal its stdio is the container's too.
        let (stdout_r, stdout_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stderr_r, stderr_w) = pipe2(OFlag::O_CLOEXEC)?;
        let stdin = if spec.tty {
            actions.add_open(0, "/dev/null", OFlag::O_RDONLY, Mode::empty())?;
            None
        } else {
            let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
            actions.add_dup2(stdin_r.as_raw_fd(), 0)?;
            Some((stdin_r, stdin_w))
        };
        actions.add_dup2(stdout_w.as_raw_fd(), 1)?;
        actions.add_dup2(stderr_w.as_raw_fd(), 2)?;
        let pid = posix_spawn(Path::new(CRUN), &actions, &attr, &argv, &envp).map_err(context("starting crun"))?;
        drop((stdout_w, stderr_w));
        let mut stdin = stdin.map(|(_, w)| File::from(w));
        let mut outputs = vec![
            (Stream::Stdout, File::from(stdout_r)),
            (Stream::Stderr, File::from(stderr_r)),
        ];

        if let Some(listener) = listener {
            let terminal = receive_terminal(&listener, &outputs[1].1);
            let _ = std::fs::remove_file(&socket);
            match terminal {
                Ok(Some(master)) => {
                    stdin = Some(master.try_clone()?);
                    outputs.insert(0, (Stream::Stdout, master));
                }
                // crun ended first: what it wrote on stderr says why, and its exit code follows.
                Ok(None) => {}
                Err(e) => {
                    let _ = kill(pid, NixSignal::SIGKILL);
                    return Err(e);
                }
            }
        }
        self.run = Some(Running {
            tty: spec.tty,
            stdin,
            outputs,
        });
        Ok(pid.as_raw())
    }

    fn write_stdin(&mut self, data: &[u8]) -> io::Result<()> {
        let stdin = self
            .run
            .as_mut()
            .and_then(|r| r.stdin.as_mut())
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "the command's stdin is closed"))?;
        stdin.write_all(data)
    }

    fn close_stdin(&mut self) {
        // A terminal stays open: closing it would hang up the command.
        if let Some(run) = self.run.as_mut().filter(|r| !r.tty) {
            run.stdin = None;
        }
    }

    fn drain_output(&mut self) -> Vec<(Stream, Vec<u8>)> {
        // Output already read but still queued behind the SIGCHLD comes first, so nothing follows `Exited`.
        let mut drained = Vec::new();
        self.pending.retain(|event| match event {
            Event::Output(stream, bytes) => {
                drained.push((*stream, bytes.clone()));
                false
            }
            _ => true,
        });
        if let Some(mut run) = self.run.take() {
            for (stream, file) in &mut run.outputs {
                let mut buf = [0u8; 4096];
                loop {
                    let readable = {
                        let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
                        matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0)
                            && fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLIN))
                    };
                    if !readable {
                        break;
                    }
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => drained.push((*stream, buf[..n].to_vec())),
                    }
                }
            }
        }
        drained
    }

    fn send_control(&mut self, line: &str) -> io::Result<()> {
        let port = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "no control channel"))?;
        port.write_all(line.as_bytes())?;
        port.flush()
    }

    fn reap(&mut self) -> io::Result<Vec<Reaped>> {
        let mut reaped = Vec::new();
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => reaped.push(Reaped {
                    pid: pid.as_raw(),
                    code,
                    status: format!("exited with status {code}"),
                }),
                Ok(WaitStatus::Signaled(pid, sig, _)) => reaped.push(Reaped {
                    pid: pid.as_raw(),
                    code: 128 + sig as i32,
                    status: format!("killed by {sig}"),
                }),
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return Ok(reaped),
                Ok(_) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn sync(&mut self) {
        nix::unistd::sync();
    }

    fn power_off(&mut self) -> io::Error {
        match reboot(RebootMode::RB_POWER_OFF) {
            Ok(never) => match never {},
            Err(e) => e.into(),
        }
    }
}

/// Run as PID 1 and never return: a PID 1 that exits or panics panics the kernel.
pub fn run_as_init() -> ! {
    let mut sys = Linux::new();
    let outcome = std::panic::catch_unwind(|| init::run(&mut Linux::new(), &mut io::stdout()));
    let mut out = io::stdout();
    if outcome.is_err() {
        let _ = writeln!(out, "hermit-agent: fatal: panicked, powering off");
        sys.sync();
        let err = sys.power_off();
        let _ = writeln!(out, "hermit-agent: power off failed: {err}");
    }
    let _ = writeln!(out, "hermit-agent: halted");
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_is_not_pid1() {
        assert!(!is_pid1());
    }

    #[test]
    fn mkdir_all_creates_parents() {
        let dir = std::env::temp_dir().join(format!("hermit-agent-{}", std::process::id()));
        let nested = dir.join("a/b");
        Linux::new().mkdir_all(nested.to_str().unwrap()).unwrap();
        assert!(nested.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mount_without_privileges_fails() {
        let m = Mount {
            target: "/nonexistent-hermit-agent-target",
            ..init::EARLY_MOUNTS[5]
        };
        assert!(Linux::new().mount(&m).is_err());
    }

    #[test]
    fn finds_the_control_port_by_name() {
        let dir = std::env::temp_dir().join(format!("hermit-agent-ports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (port, name) in [
            ("vport0p1", "org.example.other\n"),
            ("vport0p2", "org.oci-hermit.control\n"),
        ] {
            std::fs::create_dir_all(dir.join(port)).unwrap();
            std::fs::write(dir.join(port).join("name"), name).unwrap();
        }
        let found = find_port(&dir, Path::new("/dev"), CONTROL_PORT).unwrap();
        assert_eq!(found, Path::new("/dev/vport0p2"));
        let err = find_port(&dir, Path::new("/dev"), "org.example.missing").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(find_port(&dir.join("absent"), Path::new("/dev"), CONTROL_PORT).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn control_lines_become_events_and_hangup_closes_the_channel() {
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sys = Linux::new();
        sys.block_signals().unwrap();
        sys.control = Some(File::from(std::os::fd::OwnedFd::from(ours)));
        theirs.write_all(b"{\"type\":\"shutdown\"}\nsecond").unwrap();
        assert_eq!(
            sys.next_event().unwrap(),
            Event::Control(hermit_proto::Frame::Line(r#"{"type":"shutdown"}"#.into()))
        );
        sys.send_control("reply\n").unwrap();
        let mut got = [0u8; 6];
        theirs.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"reply\n");
        theirs.write_all(b" line\n").unwrap();
        assert_eq!(
            sys.next_event().unwrap(),
            Event::Control(hermit_proto::Frame::Line("second line".into()))
        );
        drop(theirs);
        assert_eq!(sys.next_event().unwrap(), Event::ControlClosed);
        assert_eq!(sys.send_control("x\n").unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[test]
    fn finds_commands_inside_the_image_root_through_its_path() {
        let root = std::env::temp_dir().join(format!("hermit-agent-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["bin", "usr/bin", "srv"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (path, mode) in [("bin/busybox", 0o755), ("usr/bin/data", 0o644), ("srv/run.sh", 0o755)] {
            std::fs::write(root.join(path), "").unwrap();
            std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        // An absolute symlink, as busybox images have: it points into the image, not the agent's root.
        std::os::unix::fs::symlink("/bin/busybox", root.join("bin/sh")).unwrap();
        std::os::unix::fs::symlink("/../../../../etc/passwd", root.join("bin/escape")).unwrap();
        let root_s = root.to_str().unwrap();
        let env = ["PATH=/usr/bin:/bin".to_string()];
        assert!(find_executable(root_s, "sh", &env, "/").is_ok());
        assert!(find_executable(root_s, "/bin/sh", &[], "/").is_ok());
        assert!(find_executable(root_s, "./run.sh", &[], "/srv").is_ok());
        assert!(
            find_executable(root_s, "busybox", &[], "/").is_ok(),
            "the default PATH has /bin"
        );
        let missing = find_executable(root_s, "nosuchcmd", &env, "/").unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        assert_eq!(missing.to_string(), "executable file not found in $PATH");
        assert!(find_executable(root_s, "data", &env, "/").is_err(), "not executable");
        assert!(
            find_executable(root_s, "escape", &env, "/").is_err(),
            "stays inside the root"
        );
        assert!(
            find_executable(root_s, "bin", &["PATH=/".into()], "/").is_err(),
            "a directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_nul_in_the_command() {
        assert_eq!(
            cstrings(&["ok".into(), "bad\0".into()]).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_second_start_is_refused() {
        let mut sys = Linux::new();
        sys.run = Some(Running {
            tty: false,
            stdin: None,
            outputs: Vec::new(),
        });
        let spec = RunSpec {
            argv: vec!["true".into()],
            env: Vec::new(),
            cwd: "/".into(),
            tty: false,
            size: None,
            user: None,
            hostname: init::DEFAULT_HOSTNAME.into(),
        };
        assert_eq!(sys.start(&spec).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(sys.write_stdin(b"x").unwrap_err().kind(), ErrorKind::NotConnected);
        sys.close_stdin();
        assert!(sys.drain_output().is_empty());
    }

    #[test]
    fn queued_output_is_drained_before_the_exit_is_reported() {
        let mut sys = Linux::new();
        sys.pending.push_back(Event::Signal(Signal::Child));
        sys.pending
            .push_back(Event::Output(Stream::Stdout, b"last words".to_vec()));
        sys.run = Some(Running {
            tty: false,
            stdin: None,
            outputs: Vec::new(),
        });
        assert_eq!(sys.drain_output(), [(Stream::Stdout, b"last words".to_vec())]);
        assert!(matches!(sys.pending.as_slices().0, [Event::Signal(Signal::Child)]));
        assert!(sys.run.is_none());
    }

    #[test]
    fn waiting_before_blocking_signals_is_an_error() {
        assert_eq!(Linux::new().next_event().unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[test]
    #[expect(clippy::zombie_processes, reason = "the code under test reaps the child")]
    fn reap_collects_exited_children() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let mut reaped = Vec::new();
        for _ in 0..200 {
            reaped.extend(Linux::new().reap().unwrap());
            if reaped.iter().any(|r| r.pid == pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            reaped
                .iter()
                .any(|r| r.pid == pid && r.status == "exited with status 0")
        );
    }
}
