//! The per-container monitor (#22): a hidden `oci-hermit __monitor <data> <id>` process, one per running container,
//! that owns its QEMU. No daemon.
//!
//! `run` prepares everything (image, initramfs, QEMU's command line, the command) and writes it to
//! `<id>/launch.json`, starts the monitor on its own (a new process group on Unix; detached, out of oci-hermit's
//! console and, where allowed, its Job Object on Windows), connects to the container's [`ipc`] channel and attaches.
//! The monitor takes the container's running lock, boots the sandbox, starts the command, appends its output to
//! `<id>/container.log` and records how it ended. QEMU's lifetime is tied to the monitor's, as it was to `run`'s.
//!
//! A foreground `run` stays attached for the output and the exit code, and if it goes away (the terminal closed, the
//! process killed) the monitor stops the sandbox. `run -d` detaches once the command started, and the container runs
//! until it exits or `stop` asks the monitor to stop it.
//!
//! `exec` connects too, and asks the monitor to start another command in the container (protocol 1.5): that
//! command's output goes to that `exec` only, not to the container's log, as with Docker.
//!
//! The monitor and its clients talk in lines of JSON: a client sends a [`Request`], the monitor answers with
//! [`Notice`]s.

use crate::containers::Containers;
use crate::ipc;
use crate::supervisor::{self, Input, Relayed, Sandbox, SupervisorError};
use crate::users;
use hermit_proto::{Frame, Framer, MAIN, Process as ProcessNumber, Stream, b64};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// What `run` hands the monitor, in the container's folder.
pub const LAUNCH: &str = "launch.json";
/// The container's output, as records `logs` reads.
pub const LOG: &str = "container.log";
/// The monitor's own stdout and stderr, beside the sandbox's console and QEMU logs: kept when `--rm` removes the
/// container.
pub const MONITOR_LOG: &str = "monitor.log";

/// How long the monitor waits for the `run` that started it to attach.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the agent may take to start the command once asked.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the guest may take to power off.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a client waits for a new monitor to listen.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `stop` waits for a container to stop once it was killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `stop` gives a container's command after its stop signal before killing it, as Docker does.
pub const STOP_GRACE: u64 = 10;
/// SIGTERM, the stop signal of an image that names none.
pub const SIGTERM: i32 = 15;
/// SIGKILL.
pub const SIGKILL: i32 = 9;
/// The exit code a stopped container gets: its processes were killed, as with SIGKILL.
pub const STOPPED: i32 = 137;

/// Everything the monitor needs to run a container.
#[derive(Debug, Serialize, Deserialize)]
pub struct Launch {
    pub qemu: PathBuf,
    pub args: Vec<OsString>,
    pub qemu_log: PathBuf,
    pub console_log: PathBuf,
    pub command: supervisor::Command,
    /// `-i`: keep the command's stdin open.
    pub interactive: bool,
    /// `-d`: the container keeps running when the `run` that started it goes away.
    pub detach: bool,
    pub boot_timeout_secs: u64,
    /// The image's `/etc/passwd` and `/etc/group`, for `exec -u`.
    #[serde(default)]
    pub accounts: users::Accounts,
    /// The signal `stop` sends first: the image's `StopSignal`, or SIGTERM.
    #[serde(default = "sigterm")]
    pub stop_signal: i32,
}

fn sigterm() -> i32 {
    SIGTERM
}

/// What a client asks of a monitor. The first one on a connection says what the connection is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// The `run` that started the container: it gets every [`Notice`] and, in the foreground, the output.
    Attach,
    /// Bytes for the command's stdin, base64-encoded.
    Stdin {
        data: String,
    },
    StdinClosed,
    /// Stop the container: send it `signal` (its stop signal when absent), and kill it if it has not stopped after
    /// `timeout` seconds ([`STOP_GRACE`] when absent; 0 kills it at once).
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    /// Send the container's command a signal: `kill`, or Ctrl-C from a foreground `run`.
    Signal {
        signal: i32,
    },
    /// `exec`: start another command in the running container. The connection then carries that command's stdin,
    /// and its notices.
    Exec {
        argv: Vec<String>,
        /// `NAME=value` pairs over the container's environment.
        env: Vec<String>,
        /// The working directory; the container's when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workdir: Option<String>,
        /// Who to run it as (name|uid[:group|gid]), resolved against the image; the container's user when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        tty: bool,
        /// The terminal's `(rows, cols)`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<(u16, u16)>,
        /// `-i`: keep its stdin open.
        interactive: bool,
    },
    /// `wait`: answered with [`Notice::Exited`] once the container has ended and its exit code is recorded.
    Wait,
    /// `cp` out of the container: answered with [`Notice::Entry`]s and their [`Notice::EntryData`], then
    /// `Exited` (0) and `Done`, or `Failed` and `Done`.
    CopyOut {
        path: String,
    },
    /// `cp` into the container: the connection then carries `Put`, `PutData` and `PutDone`, and is answered as
    /// `CopyOut` is, without entries.
    CopyIn {
        #[serde(flatten)]
        into: supervisor::CopyInto,
    },
    Put {
        entry: hermit_proto::Entry,
    },
    PutData {
        data: String,
    },
    PutDone,
    /// `attach`: answered with [`Notice::Started`], then the container's output from now on, and
    /// [`Notice::Exited`] and [`Notice::Done`] when it ends. The connection may carry stdin (taken when the container
    /// was started with `-i`) and signals; hanging up detaches.
    Join,
}

/// What a monitor tells its clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// The agent answered: QEMU has the layer disks open.
    Booted,
    /// The command started.
    Started,
    /// Output, base64-encoded; only a foreground `run` gets it.
    Output {
        stream: Stream,
        data: String,
    },
    /// The command ended with this code, or [`STOPPED`] when it was stopped.
    Exited {
        code: i32,
    },
    /// The container could not be run.
    Failed {
        message: String,
    },
    /// The answer to [`Request::Stop`].
    Stopping,
    /// The answer to [`Request::Signal`] on a connection of its own: the signal was passed on.
    Signalled,
    /// One entry of a copy out of the container, and a file's bytes after it, base64-encoded.
    Entry {
        entry: hermit_proto::Entry,
    },
    EntryData {
        data: String,
    },
    /// Everything is recorded (and with `--rm` removed); the monitor exits.
    Done,
}

/// Writes one message as a line.
pub fn send<T: Serialize>(to: &mut dyn Write, message: &T) -> io::Result<()> {
    let line = hermit_proto::encode(message).map_err(io::Error::other)?;
    to.write_all(line.as_bytes())?;
    to.flush()
}

/// Reads messages from `from`, handing each to `each`, until the stream ends, a line is not a message, or `each`
/// returns false.
pub fn read_messages<T: DeserializeOwned>(mut from: impl Read, mut each: impl FnMut(T) -> bool) {
    let mut framer = Framer::new();
    let mut frames = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        framer.push(&buf[..n], &mut frames);
        for frame in frames.drain(..) {
            let Frame::Line(line) = frame else { return };
            let Ok(message) = hermit_proto::decode::<T>(&line) else {
                return;
            };
            if !each(message) {
                return;
            }
        }
    }
}

/// One message, read a byte at a time so nothing after it is taken from the reader: the first line on a new
/// connection says what it is for.
pub fn read_one<T: DeserializeOwned>(from: &mut dyn Read) -> Option<T> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    while line.len() < hermit_proto::MAX_LINE {
        match from.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return hermit_proto::decode(std::str::from_utf8(&line).ok()?).ok(),
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    None
}

/// Appends one record to a container log: the stream (1 for stdout, 2 for stderr), the length as four bytes, little
/// end first, and the bytes.
fn write_record(to: &mut impl Write, stream: Stream, bytes: &[u8]) -> io::Result<()> {
    let mut record = Vec::with_capacity(bytes.len() + 5);
    record.push(match stream {
        Stream::Stdout => 1,
        Stream::Stderr => 2,
    });
    record.extend_from_slice(&u32::try_from(bytes.len()).map_err(io::Error::other)?.to_le_bytes());
    record.extend_from_slice(bytes);
    to.write_all(&record)
}

/// Cuts the complete records off the front of `buf`, leaving a record still being written.
fn take_records(buf: &mut Vec<u8>) -> Vec<(Stream, Vec<u8>)> {
    let mut records = Vec::new();
    let mut at = 0;
    while buf.len() - at >= 5 {
        let stream = if buf[at] == 2 { Stream::Stderr } else { Stream::Stdout };
        let len = u32::from_le_bytes(buf[at + 1..at + 5].try_into().expect("four bytes")) as usize;
        if buf.len() - at - 5 < len {
            break;
        }
        records.push((stream, buf[at + 5..at + 5 + len].to_vec()));
        at += 5 + len;
    }
    buf.drain(..at);
    records
}

/// Where notices go: the attached `run`, until it goes away.
type Owner = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

fn notify(owner: &Owner, notice: &Notice) {
    let mut attached = owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(to) = attached.as_mut()
        && send(to.as_mut(), notice).is_err()
    {
        *attached = None;
    }
}

/// One `exec`'s connection.
struct ExecClient {
    to: Box<dyn Write + Send>,
    started: bool,
}

/// The `exec` clients, by the process number their command has in the guest.
struct Execs {
    /// Whether more are taken: not once the container's command has ended.
    open: bool,
    last: ProcessNumber,
    clients: HashMap<ProcessNumber, ExecClient>,
}

type SharedExecs = Arc<Mutex<Execs>>;

impl Execs {
    fn new() -> SharedExecs {
        Arc::new(Mutex::new(Execs {
            open: true,
            last: MAIN,
            clients: HashMap::new(),
        }))
    }

    /// Tells `process`'s client; one that went away is forgotten, and its command's output dropped.
    fn tell(&mut self, process: ProcessNumber, notice: &Notice) {
        if let Some(client) = self.clients.get_mut(&process)
            && send(client.to.as_mut(), notice).is_err()
        {
            self.clients.remove(&process);
        }
    }

    /// Tells `process`'s client how it ended, and lets it go.
    fn finish(&mut self, process: ProcessNumber, notice: &Notice) {
        self.tell(process, notice);
        if let Some(mut client) = self.clients.remove(&process) {
            let _ = send(client.to.as_mut(), &Notice::Done);
        }
    }

    /// The container's command ended: what still runs was killed with it, and nothing more is taken.
    fn close(&mut self) {
        self.open = false;
        for (_, mut client) in self.clients.drain() {
            let notice = if client.started {
                Notice::Exited { code: STOPPED }
            } else {
                Notice::Failed {
                    message: "the container is not running".into(),
                }
            };
            let _ = send(client.to.as_mut(), &notice);
            let _ = send(client.to.as_mut(), &Notice::Done);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What an `exec` runs, from its request over the container's own command: the container's environment, working
/// directory and user unless the request says otherwise.
fn exec_command(launch: &Launch, request: &Request) -> Result<supervisor::Command, String> {
    let Request::Exec {
        argv,
        env,
        workdir,
        user,
        tty,
        size,
        ..
    } = request
    else {
        return Err("not an exec".into());
    };
    let base = &launch.command;
    let mut vars = base.env.clone();
    let mut set = |var: String| {
        let name = var.split('=').next().unwrap_or_default().to_string();
        match vars.iter_mut().find(|v| v.split('=').next() == Some(name.as_str())) {
            Some(old) => *old = var,
            None => vars.push(var),
        }
    };
    let user = match user {
        Some(spec) => {
            let identity = users::resolve(spec, &launch.accounts)?;
            set(format!("HOME={}", identity.home));
            (!identity.is_root()).then_some(hermit_proto::User {
                uid: identity.uid,
                gid: identity.gid,
                groups: identity.groups,
            })
        }
        None => base.user.clone(),
    };
    for var in env {
        set(var.clone());
    }
    Ok(supervisor::Command {
        argv: argv.clone(),
        env: vars,
        cwd: workdir.clone().unwrap_or_else(|| base.cwd.clone()),
        tty: *tty,
        size: *size,
        user,
        layers: Vec::new(),
        hostname: None,
        limits: hermit_proto::Limits::default(),
    })
}

/// Serves one `exec` connection: starts its command, then passes its stdin on until it hangs up.
fn serve_exec(request: &Request, conn: ipc::Conn, launch: &Launch, execs: &SharedExecs, to_run: &mpsc::Sender<Input>) {
    let ipc::Conn { reader, mut writer } = conn;
    let refuse = |writer: &mut Box<dyn Write + Send>, message: String| {
        let _ = send(writer.as_mut(), &Notice::Failed { message });
        let _ = send(writer.as_mut(), &Notice::Done);
    };
    let command = match exec_command(launch, request) {
        Ok(command) => command,
        Err(message) => return refuse(&mut writer, message),
    };
    let interactive = matches!(request, Request::Exec { interactive: true, .. });
    let process = {
        let mut execs = lock(execs);
        if !execs.open {
            drop(execs);
            return refuse(&mut writer, "the container is not running".into());
        }
        execs.last += 1;
        let process = execs.last;
        execs.clients.insert(
            process,
            ExecClient {
                to: writer,
                started: false,
            },
        );
        process
    };
    if to_run.send(Input::Exec(process, command)).is_err() {
        return;
    }
    if !interactive {
        let _ = to_run.send(Input::Eof(process));
    }
    read_messages(reader, |request: Request| match request {
        Request::Stdin { data } => {
            b64::decode(&data).is_some_and(|bytes| to_run.send(Input::Data(process, bytes)).is_ok())
        }
        Request::StdinClosed => to_run.send(Input::Eof(process)).is_ok(),
        _ => true,
    });
    // It hung up: its command runs on, as with Docker, with its stdin closed.
    let _ = to_run.send(Input::Eof(process));
}

/// Serves one `cp` connection: gives it a process number, as an `exec` has, and passes its entries on.
fn serve_copy(request: Request, conn: ipc::Conn, execs: &SharedExecs, to_run: &mpsc::Sender<Input>) {
    let ipc::Conn { reader, mut writer } = conn;
    let process = {
        let mut execs = lock(execs);
        if !execs.open {
            drop(execs);
            let message = "the container is not running".to_string();
            let _ = send(writer.as_mut(), &Notice::Failed { message });
            let _ = send(writer.as_mut(), &Notice::Done);
            return;
        }
        execs.last += 1;
        let process = execs.last;
        execs.clients.insert(
            process,
            ExecClient {
                to: writer,
                started: false,
            },
        );
        process
    };
    let input = match request {
        Request::CopyOut { path } => Input::CopyOut(process, path),
        Request::CopyIn { into } => Input::CopyIn(process, into),
        _ => return,
    };
    let receiving = matches!(input, Input::CopyIn(..));
    if to_run.send(input).is_err() || !receiving {
        return;
    }
    read_messages(reader, |request: Request| match request {
        Request::Put { entry } => to_run.send(Input::Put(process, entry)).is_ok(),
        Request::PutData { data } => {
            b64::decode(&data).is_some_and(|bytes| to_run.send(Input::PutData(process, bytes)).is_ok())
        }
        Request::PutDone => {
            let _ = to_run.send(Input::PutDone(process));
            false
        }
        _ => true,
    });
}

/// Run as the monitor of container `id`, writing what goes wrong to `log`; the process's exit code.
pub fn serve(data_dir: &Path, id: &str, log: &mut dyn Write) -> u8 {
    match monitor(data_dir, id, log) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(log, "oci-hermit monitor: {e}");
            1
        }
    }
}

fn monitor(data_dir: &Path, id: &str, log: &mut dyn Write) -> Result<(), Box<dyn std::error::Error>> {
    let containers = Containers::open(data_dir)?;
    let dir = containers.dir(id);
    let launch: Arc<Launch> = Arc::new(serde_json::from_slice(&std::fs::read(dir.join(LAUNCH))?)?);
    let auto_remove = containers.find(id)?.container.auto_remove;
    let running = containers.start(id, crate::images::now())?;
    let owner: Owner = Arc::new(Mutex::new(None));
    let (to_run, input) = mpsc::channel();
    let (attached_tx, attached) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let old_agent = Arc::new(AtomicBool::new(false));
    let execs = Execs::new();
    let waiters: Waiters = Arc::new(Mutex::new(Vec::new()));
    let joined: Waiters = Arc::new(Mutex::new(Vec::new()));
    let outcome = match ipc::Listener::bind(data_dir, id) {
        Ok(listener) => {
            let accepting = {
                let (owner, to_run, finished) = (owner.clone(), to_run.clone(), finished.clone());
                let (launch, execs, old_agent) = (launch.clone(), execs.clone(), old_agent.clone());
                let (waiters, joined) = (waiters.clone(), joined.clone());
                std::thread::spawn(move || {
                    let shared = Shared {
                        launch,
                        execs,
                        old_agent,
                        waiters,
                        joined,
                    };
                    accept(listener, &owner, attached_tx, &to_run, &finished, &shared);
                })
            };
            let shared = Shared {
                launch: launch.clone(),
                execs: execs.clone(),
                old_agent,
                waiters: waiters.clone(),
                joined: joined.clone(),
            };
            let outcome = run(&shared, &dir, &owner, &attached, to_run, &input, log);
            lock(&execs).close();
            finished.store(true, Ordering::SeqCst);
            // Wake the accepting thread so it drops the listener, which removes the socket.
            drop(ipc::connect(data_dir, id));
            let _ = accepting.join();
            outcome
        }
        Err(e) => Err(format!("listening for commands: {e}")),
    };
    lock(&execs).close();
    let (code, error) = match outcome {
        Ok(code) => (code, None),
        Err(message) => {
            let _ = writeln!(log, "oci-hermit monitor: {message}");
            notify(
                &owner,
                &Notice::Failed {
                    message: message.clone(),
                },
            );
            (125, Some(message))
        }
    };
    if let Err(e) = running.exited(Some(code), error, crate::images::now()) {
        let _ = writeln!(log, "oci-hermit monitor: recording how the container ended: {e}");
    }
    // Before `--rm` removes the record, so `wait` gets the code either way.
    for mut waiter in lock(&waiters).drain(..) {
        let _ = send(waiter.as_mut(), &Notice::Exited { code });
    }
    for mut client in lock(&joined).drain(..) {
        let _ = send(client.as_mut(), &Notice::Exited { code }).and_then(|()| send(client.as_mut(), &Notice::Done));
    }
    if auto_remove && let Err(e) = containers.remove(id) {
        let _ = writeln!(log, "oci-hermit monitor: removing the container: {e}");
    }
    notify(&owner, &Notice::Done);
    Ok(())
}

/// What the monitor's threads share.
#[derive(Clone)]
struct Shared {
    launch: Arc<Launch>,
    execs: SharedExecs,
    /// The agent answered, and speaks a protocol older than 1.6: it takes no signals.
    old_agent: Arc<AtomicBool>,
    /// `wait` clients, told the exit code once it is recorded.
    waiters: Waiters,
    /// `attach` clients, given the output as it comes.
    joined: Waiters,
}

type Waiters = Arc<Mutex<Vec<Box<dyn Write + Send>>>>;

impl Shared {
    /// Stops the container: its stop signal (or `signal`), then after `timeout` seconds the sandbox is powered off.
    /// An agent that takes no signals is powered off at once.
    fn stop(&self, to_run: &mpsc::Sender<Input>, timeout: Option<u64>, signal: Option<i32>) {
        let grace = timeout.unwrap_or(STOP_GRACE);
        if grace == 0 || self.old_agent.load(Ordering::SeqCst) {
            let _ = to_run.send(Input::Stop);
            return;
        }
        let _ = to_run.send(Input::Signal(MAIN, signal.unwrap_or(self.launch.stop_signal)));
        let to_run = to_run.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(grace));
            let _ = to_run.send(Input::Stop);
        });
    }

    /// Passes `signal` on to the container's command. An agent that takes no signals can still be killed, by
    /// powering it off; anything else it cannot take is an error.
    fn signal(&self, to_run: &mpsc::Sender<Input>, signal: i32) -> Result<(), String> {
        if !self.old_agent.load(Ordering::SeqCst) {
            let _ = to_run.send(Input::Signal(MAIN, signal));
        } else if signal == SIGKILL {
            let _ = to_run.send(Input::Stop);
        } else {
            return Err(
                "the container's guest agent is too old to pass signals on (it needs control protocol 1.6); \
                        only SIGKILL works, or stop"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Takes connections until the monitor is finished: the first `Attach` becomes the owner, a `Stop` or `Signal` is
/// passed on, an `Exec` starts its command.
fn accept(
    mut listener: ipc::Listener,
    owner: &Owner,
    attached: mpsc::Sender<Box<dyn Read + Send>>,
    to_run: &mpsc::Sender<Input>,
    finished: &AtomicBool,
    shared: &Shared,
) {
    let attached = Arc::new(Mutex::new(Some(attached)));
    while let Ok(conn) = listener.accept() {
        if finished.load(Ordering::SeqCst) {
            return;
        }
        let (owner, attached, to_run) = (owner.clone(), attached.clone(), to_run.clone());
        let shared = shared.clone();
        // A client that connects and says nothing holds only its own thread.
        std::thread::spawn(move || {
            let ipc::Conn { mut reader, mut writer } = conn;
            match read_one(&mut reader) {
                Some(request @ (Request::CopyOut { .. } | Request::CopyIn { .. })) => {
                    serve_copy(request, ipc::Conn { reader, writer }, &shared.execs, &to_run);
                }
                Some(request @ Request::Exec { .. }) => {
                    serve_exec(
                        &request,
                        ipc::Conn { reader, writer },
                        &shared.launch,
                        &shared.execs,
                        &to_run,
                    );
                }
                Some(Request::Attach) => {
                    let first = attached
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    match first {
                        Some(tx) => {
                            *owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
                            let _ = tx.send(reader);
                        }
                        None => {
                            let _ = send(
                                writer.as_mut(),
                                &Notice::Failed {
                                    message: "another oci-hermit is attached to this container".into(),
                                },
                            );
                        }
                    }
                }
                Some(Request::Stop { timeout, signal }) => {
                    shared.stop(&to_run, timeout, signal);
                    let _ = send(writer.as_mut(), &Notice::Stopping);
                }
                Some(Request::Signal { signal }) => {
                    let notice = match shared.signal(&to_run, signal) {
                        Ok(()) => Notice::Signalled,
                        Err(message) => Notice::Failed { message },
                    };
                    let _ = send(writer.as_mut(), &notice);
                }
                Some(Request::Wait) => lock(&shared.waiters).push(writer),
                Some(Request::Join) => {
                    if send(writer.as_mut(), &Notice::Started).is_err() {
                        return;
                    }
                    lock(&shared.joined).push(writer);
                    let interactive = shared.launch.interactive;
                    // Until it hangs up: its input, if the container takes any, and its signals.
                    read_messages(reader, |request: Request| match request {
                        Request::Stdin { data } if interactive => {
                            b64::decode(&data).is_some_and(|bytes| to_run.send(Input::Data(MAIN, bytes)).is_ok())
                        }
                        Request::Signal { signal } => {
                            let _ = shared.signal(&to_run, signal);
                            true
                        }
                        _ => true,
                    });
                }
                _ => {}
            }
        });
    }
}

/// Boots the sandbox and runs the command, and any `exec`'s; its exit code, or why it could not run.
fn run(
    shared: &Shared,
    dir: &Path,
    owner: &Owner,
    attached: &mpsc::Receiver<Box<dyn Read + Send>>,
    to_run: mpsc::Sender<Input>,
    input: &mpsc::Receiver<Input>,
    log: &mut dyn Write,
) -> Result<i32, String> {
    let reader = attached
        .recv_timeout(ATTACH_TIMEOUT)
        .map_err(|_| "the oci-hermit that started the container went away before it attached".to_string())?;
    let (launch, execs) = (shared.launch.as_ref(), &shared.execs);
    let detach = launch.detach;
    let owned = shared.clone();
    std::thread::spawn(move || {
        read_messages(reader, |request: Request| match request {
            Request::Stdin { data } => {
                b64::decode(&data).is_some_and(|bytes| to_run.send(Input::Data(MAIN, bytes)).is_ok())
            }
            Request::StdinClosed => to_run.send(Input::Eof(MAIN)).is_ok(),
            Request::Stop { timeout, signal } => {
                owned.stop(&to_run, timeout, signal);
                true
            }
            // Ctrl-C at a foreground run: an agent that takes no signals is stopped, as before 1.6.
            Request::Signal { signal } => {
                if owned.signal(&to_run, signal).is_err() {
                    let _ = to_run.send(Input::Stop);
                }
                true
            }
            _ => true,
        });
        // A foreground run whose oci-hermit went away stops with it.
        if !detach {
            let _ = to_run.send(Input::Stop);
        }
    });
    let see_logs = |e: SupervisorError| {
        format!(
            "{e}; see {} and {}",
            launch.console_log.display(),
            launch.qemu_log.display()
        )
    };
    let mut sandbox = Sandbox::spawn(&launch.qemu, &launch.args, &launch.qemu_log)
        .map_err(|e| format!("starting {}: {e}", launch.qemu.display()))?;
    let (_, version) = sandbox
        .handshake(Duration::from_secs(launch.boot_timeout_secs))
        .map_err(see_logs)?;
    notify(owner, &Notice::Booted);
    shared.old_agent.store(version < (1, 6), Ordering::SeqCst);
    // An agent older than 1.4 would run the command outside a container (and before 1.3 not find the layer disks,
    // before 1.2 run it as root).
    if version < (1, 4) {
        return Err(format!(
            "the guest agent speaks control protocol {}.{} and cannot run containers; it needs 1.4 or later (use \
             the guest that came with this oci-hermit)",
            version.0, version.1
        ));
    }
    let container_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG))
        .map_err(|e| format!("opening the container log: {e}"))?;
    let mut container_log = std::io::BufWriter::new(container_log);
    sandbox.start(&launch.command, START_TIMEOUT).map_err(see_logs)?;
    notify(owner, &Notice::Started);
    let mut on = |relayed: Relayed<'_>| -> io::Result<()> {
        match relayed {
            Relayed::Output {
                process: MAIN,
                stream,
                bytes,
            } => {
                // A full disk must not stop the container, so a log write that fails only loses that output.
                let _ = write_record(&mut container_log, stream, bytes).and_then(|()| container_log.flush());
                let output = Notice::Output {
                    stream,
                    data: b64::encode(bytes),
                };
                // An `attach` that has gone away is dropped.
                lock(&shared.joined).retain_mut(|client| send(client.as_mut(), &output).is_ok());
                if !detach {
                    notify(
                        owner,
                        &Notice::Output {
                            stream,
                            data: b64::encode(bytes),
                        },
                    );
                }
            }
            Relayed::Output { process, stream, bytes } => lock(execs).tell(
                process,
                &Notice::Output {
                    stream,
                    data: b64::encode(bytes),
                },
            ),
            Relayed::Started { process } => {
                let mut execs = lock(execs);
                if let Some(client) = execs.clients.get_mut(&process) {
                    client.started = true;
                }
                execs.tell(process, &Notice::Started);
            }
            Relayed::Exited { process, code } => lock(execs).finish(process, &Notice::Exited { code }),
            Relayed::Entry { process, entry } => lock(execs).tell(process, &Notice::Entry { entry }),
            Relayed::EntryData { process, bytes } => lock(execs).tell(
                process,
                &Notice::EntryData {
                    data: b64::encode(bytes),
                },
            ),
            Relayed::Failed { process, message } => lock(execs).finish(process, &Notice::Failed { message }),
        }
        Ok(())
    };
    let ended = sandbox.relay(input, launch.interactive, &mut on).map_err(see_logs)?;
    let code = ended.unwrap_or(STOPPED);
    notify(owner, &Notice::Exited { code });
    if let Err(e) = sandbox.shutdown(SHUTDOWN_TIMEOUT) {
        let _ = writeln!(log, "oci-hermit monitor: stopping the sandbox: {}", see_logs(e));
    }
    Ok(code)
}

/// A started monitor: a process, or in tests a thread.
pub enum Process {
    Child(std::process::Child),
    #[cfg(test)]
    Thread(std::thread::JoinHandle<u8>),
}

impl Process {
    fn has_exited(&mut self) -> bool {
        match self {
            Process::Child(child) => !matches!(child.try_wait(), Ok(None)),
            #[cfg(test)]
            Process::Thread(thread) => thread.is_finished(),
        }
    }
}

/// Starts `exe __monitor <data_dir> <id>` on its own, its output in `log`.
pub fn spawn(exe: &Path, data_dir: &Path, id: &str, log: &Path) -> io::Result<Process> {
    let dir = data_dir.join("containers").join(id);
    let log = File::create(log)?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__monitor")
        .arg(data_dir)
        .arg(id)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Out of the terminal's process group, so Ctrl-C at the terminal does not reach it: a foreground run passes that
    // on by going away.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // Out of oci-hermit's Job Object if it is in one that allows that (a terminal or CI runner that kills its
        // job when it closes); a job that does not allow it refuses the flag, and the monitor stays in it.
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = command.spawn() {
            return Ok(Process::Child(child));
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    Ok(Process::Child(command.spawn()?))
}

/// The end of a monitor's log, to explain why it stopped.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("; ")
}

/// Connects to the monitor just started for `id`, whose output goes to `log`, waiting for it to listen.
pub fn connect_new(data_dir: &Path, id: &str, process: &mut Process, log: &Path) -> Result<ipc::Conn, String> {
    let deadline = Instant::now() + LISTEN_TIMEOUT;
    loop {
        match ipc::connect(data_dir, id) {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                if process.has_exited() {
                    let tail = log_tail(log);
                    return Err(if tail.is_empty() {
                        format!(
                            "the container's monitor stopped before it was ready; see {}",
                            log.display()
                        )
                    } else {
                        format!("the container's monitor stopped before it was ready: {tail}")
                    });
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "the container's monitor did not listen within {}s ({e}); see {}",
                        LISTEN_TIMEOUT.as_secs(),
                        log.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Stops running container `id`: asks its monitor, then waits until it no longer runs. `timeout` and `signal` are
/// as in [`Request::Stop`].
pub fn stop(
    containers: &Containers,
    data_dir: &Path,
    id: &str,
    timeout: Option<u64>,
    signal: Option<i32>,
) -> Result<(), String> {
    let mut conn = match ipc::connect(data_dir, id) {
        Ok(conn) => conn,
        // It may have stopped by itself in the meantime.
        Err(_) if !containers.is_running(id).map_err(|e| e.to_string())? => return Ok(()),
        Err(e) => return Err(format!("cannot reach its monitor: {e}")),
    };
    send(conn.writer.as_mut(), &Request::Stop { timeout, signal }).map_err(|e| format!("asking its monitor: {e}"))?;
    read_messages(conn.reader, |notice: Notice| notice != Notice::Stopping);
    let limit = STOP_TIMEOUT + Duration::from_secs(timeout.unwrap_or(STOP_GRACE));
    let deadline = Instant::now() + limit;
    while containers.is_running(id).map_err(|e| e.to_string())? {
        if Instant::now() >= deadline {
            return Err(format!("it did not stop within {}s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// `stop`: stops the running containers, all at once, printing each as it was asked for, in order; 1 if any could
/// not be stopped. `timeout` and `signal` are as in [`Request::Stop`].
pub fn stop_command(
    containers: &Containers,
    data_dir: &Path,
    wanted: &[String],
    timeout: Option<u64>,
    signal: Option<i32>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let results: Vec<Result<(), String>> = std::thread::scope(|scope| {
        let stopping: Vec<_> = wanted
            .iter()
            .map(|name| {
                scope.spawn(move || {
                    containers.find(name).map_err(|e| e.to_string()).and_then(|l| {
                        if l.running {
                            stop(containers, data_dir, &l.container.id, timeout, signal)
                                .map_err(|reason| format!("could not stop container {}: {reason}", l.container.name))
                        } else {
                            Ok(())
                        }
                    })
                })
            })
            .collect();
        stopping
            .into_iter()
            .map(|t| t.join().unwrap_or_else(|_| Err("stopping it failed".into())))
            .collect()
    });
    let mut code = 0;
    for (name, stopped) in wanted.iter().zip(results) {
        match stopped {
            Ok(()) => {
                let _ = writeln!(out, "{name}");
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                code = 1;
            }
        }
    }
    code
}

/// `wait`: blocks until each container has ended and prints its exit code, in the order asked, as `docker wait`
/// does; one that has already ended prints the code it ended with (-1 when that is unknown). 1 if any is not found.
pub fn wait_command(
    containers: &Containers,
    data_dir: &Path,
    wanted: &[String],
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let results: Vec<Result<i32, String>> = std::thread::scope(|scope| {
        let waiting: Vec<_> = wanted
            .iter()
            .map(|name| scope.spawn(move || wait(containers, data_dir, name)))
            .collect();
        waiting
            .into_iter()
            .map(|t| t.join().unwrap_or_else(|_| Err("waiting for it failed".into())))
            .collect()
    });
    let mut status = 0;
    for result in results {
        match result {
            Ok(code) => {
                let _ = writeln!(out, "{code}");
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                status = 1;
            }
        }
    }
    status
}

/// One container's exit code: from its monitor when it is running, otherwise as recorded.
fn wait(containers: &Containers, data_dir: &Path, name: &str) -> Result<i32, String> {
    let listed = containers.find(name).map_err(|e| e.to_string())?;
    if listed.running
        && let Ok(mut conn) = ipc::connect(data_dir, &listed.container.id)
        && send(conn.writer.as_mut(), &Request::Wait).is_ok()
        && let Some(Notice::Exited { code }) = read_one(conn.reader.as_mut())
    {
        return Ok(code);
    }
    // It ended before its monitor could answer: once the monitor lets go of it, its record says how.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match containers.find(&listed.container.id) {
            Ok(l) if !l.running => return Ok(l.state.exit_code.unwrap_or(-1)),
            Ok(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(_) => return Err(format!("container {name}: its monitor does not answer")),
            Err(_) => {
                return Err(format!(
                    "container {name} was removed before its exit code could be read"
                ));
            }
        }
    }
}

/// `kill`: sends `signal` to each running container's command, printing each as it was asked for; 1 if any is not
/// running or could not be signalled. What the signal does is up to the command.
pub fn kill_command(
    containers: &Containers,
    data_dir: &Path,
    wanted: &[String],
    signal: i32,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let mut code = 0;
    for name in wanted {
        let sent = containers.find(name).map_err(|e| e.to_string()).and_then(|l| {
            let container = &l.container.name;
            if !l.running {
                return Err(format!(
                    "cannot kill container: {name}: container {} is not running",
                    l.container.id
                ));
            }
            let mut conn = ipc::connect(data_dir, &l.container.id)
                .map_err(|e| format!("could not signal container {container}: cannot reach its monitor: {e}"))?;
            send(conn.writer.as_mut(), &Request::Signal { signal })
                .map_err(|e| format!("could not signal container {container}: {e}"))?;
            match read_one(conn.reader.as_mut()) {
                Some(Notice::Signalled) => Ok(()),
                Some(Notice::Failed { message }) => Err(format!("could not signal container {container}: {message}")),
                _ => Err(format!(
                    "could not signal container {container}: its monitor did not answer"
                )),
            }
        });
        match sent {
            Ok(()) => {
                let _ = writeln!(out, "{name}");
            }
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                code = 1;
            }
        }
    }
    code
}

/// `logs`: a container's output, stdout and stderr each to its own; with `follow`, until the container stops.
pub fn logs(
    containers: &Containers,
    wanted: &str,
    follow: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = containers.find(wanted)?.container.id;
    let path = containers.dir(&id).join(LOG);
    let mut file = None;
    let mut pending = Vec::new();
    loop {
        // Whether it runs is read before the log, so the last read after it stopped gets everything.
        let running = follow && containers.is_running(&id)?;
        if file.is_none() {
            file = match File::open(&path) {
                Ok(file) => Some(file),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
        }
        if let Some(file) = file.as_mut() {
            file.read_to_end(&mut pending)?;
        }
        let shown = take_records(&mut pending).into_iter().try_for_each(|(stream, bytes)| {
            let to: &mut dyn Write = match stream {
                Stream::Stdout => out,
                Stream::Stderr => err,
            };
            to.write_all(&bytes)?;
            to.flush()
        });
        match shown {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            other => other?,
        }
        if !running {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Runs the monitor on a thread of this process instead, its output in `log` as a process's would be.
    pub(crate) fn in_thread(_exe: &Path, data_dir: &Path, id: &str, log: &Path) -> io::Result<Process> {
        let (data_dir, id) = (data_dir.to_path_buf(), id.to_string());
        let mut log = File::create(log)?;
        Ok(Process::Thread(std::thread::spawn(move || {
            serve(&data_dir, &id, &mut log)
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers;
    use crate::qemu::testing::scratch;

    #[test]
    fn requests_and_notices_are_tagged_lines() {
        let mut line = Vec::new();
        send(
            &mut line,
            &Request::Stop {
                timeout: None,
                signal: None,
            },
        )
        .unwrap();
        assert_eq!(line, b"{\"type\":\"stop\"}\n");
        assert_eq!(
            hermit_proto::decode::<Request>(r#"{"type":"stop","timeout":0}"#).unwrap(),
            Request::Stop {
                timeout: Some(0),
                signal: None
            }
        );
        let launch: Launch = serde_json::from_str(
            r#"{"qemu":"q","args":[],"qemu_log":"l","console_log":"c","interactive":false,"detach":true,
            "boot_timeout_secs":1,"command":{"argv":["x"],"env":[],"cwd":"/","tty":false,"size":null,"user":null,
            "layers":[],"hostname":null}}"#,
        )
        .unwrap();
        assert_eq!(launch.stop_signal, SIGTERM, "a launch.json from before #25");
        let mut lines = Vec::new();
        for notice in [Notice::Booted, Notice::Exited { code: 3 }, Notice::Done] {
            send(&mut lines, &notice).unwrap();
        }
        lines.extend_from_slice(b"not json\n{\"type\":\"done\"}\n");
        let mut seen = Vec::new();
        read_messages(&lines[..], |n: Notice| {
            seen.push(n);
            true
        });
        // Reading stops at the first line that is not a notice.
        assert_eq!(seen, [Notice::Booted, Notice::Exited { code: 3 }, Notice::Done]);
        let mut seen = 0;
        read_messages(&lines[..], |_: Notice| {
            seen += 1;
            false
        });
        assert_eq!(seen, 1);
    }

    #[test]
    fn the_first_request_leaves_the_rest_unread() {
        let mut stream = &b"{\"type\":\"attach\"}\n{\"type\":\"stdin_closed\"}\n"[..];
        assert_eq!(read_one(&mut stream), Some(Request::Attach));
        assert_eq!(stream, b"{\"type\":\"stdin_closed\"}\n");
        assert_eq!(read_one::<Request>(&mut &b"{\"type\":\"attach\"}"[..]), None);
        assert_eq!(read_one::<Request>(&mut &b"nope\n"[..]), None);
        let endless = vec![b'x'; hermit_proto::MAX_LINE + 1];
        assert_eq!(read_one::<Request>(&mut &endless[..]), None);
    }

    #[test]
    fn log_records_keep_each_streams_bytes_and_wait_for_the_rest() {
        let mut log = Vec::new();
        write_record(&mut log, Stream::Stdout, b"hi\n").unwrap();
        write_record(&mut log, Stream::Stderr, b"oops\n").unwrap();
        write_record(&mut log, Stream::Stdout, b"").unwrap();
        let (whole, partial) = log.split_at(log.len() - 2);
        let mut buf = whole.to_vec();
        buf.extend_from_slice(&partial[..1]);
        let mut taken = take_records(&mut buf);
        assert_eq!(
            taken,
            [(Stream::Stdout, b"hi\n".to_vec()), (Stream::Stderr, b"oops\n".to_vec())]
        );
        buf.extend_from_slice(&partial[1..]);
        taken = take_records(&mut buf);
        assert_eq!(taken, [(Stream::Stdout, Vec::new())]);
        assert!(buf.is_empty());
    }

    #[test]
    fn logs_shows_each_stream_and_an_empty_log_is_no_error() {
        let dir = scratch("monitor-logs");
        let containers = Containers::open(&dir).unwrap();
        let made = containers
            .create(
                containers::Spec {
                    name: Some("job".into()),
                    ..containers::Spec::default()
                },
                0,
            )
            .unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        logs(&containers, "job", true, &mut out, &mut err).unwrap();
        assert!(out.is_empty() && err.is_empty());
        let mut file = File::create(containers.dir(&made.id).join(LOG)).unwrap();
        write_record(&mut file, Stream::Stdout, b"hi\n").unwrap();
        write_record(&mut file, Stream::Stderr, b"oops\n").unwrap();
        logs(&containers, "job", false, &mut out, &mut err).unwrap();
        assert_eq!((&out[..], &err[..]), (&b"hi\n"[..], &b"oops\n"[..]));
        assert!(logs(&containers, "ghost", false, &mut out, &mut err).is_err());
    }

    #[test]
    fn a_monitor_without_its_launch_says_so() {
        let dir = scratch("monitor-no-launch");
        let containers = Containers::open(&dir).unwrap();
        let made = containers.create(containers::Spec::default(), 0).unwrap();
        let mut log = Vec::new();
        assert_eq!(serve(&dir, &made.id, &mut log), 1);
        assert!(String::from_utf8(log).unwrap().starts_with("oci-hermit monitor: "));
        let log = dir.join(MONITOR_LOG);
        let mut process = testing::in_thread(Path::new("/x"), &dir, &made.id, &log).unwrap();
        let err = connect_new(&dir, &made.id, &mut process, &log).err().unwrap();
        assert!(
            err.contains("stopped before it was ready: oci-hermit monitor:"),
            "{err}"
        );
    }

    fn next(conn: &mut ipc::Conn) -> Notice {
        read_one(conn.reader.as_mut()).expect("a notice")
    }

    fn exec_request(argv: &[&str], interactive: bool, user: Option<&str>) -> Request {
        Request::Exec {
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            env: vec!["EXTRA=1".into()],
            workdir: None,
            user: user.map(Into::into),
            tty: false,
            size: None,
            interactive,
        }
    }

    fn launch_with(env: &[&str], user: Option<hermit_proto::User>) -> Launch {
        Launch {
            qemu: PathBuf::from("/unused"),
            args: Vec::new(),
            qemu_log: PathBuf::new(),
            console_log: PathBuf::new(),
            command: supervisor::Command {
                argv: vec!["sleep".into()],
                env: env.iter().map(|e| (*e).to_string()).collect(),
                cwd: "/srv".into(),
                tty: false,
                size: None,
                user,
                layers: vec!["layer0".into()],
                hostname: Some("4f2a9c1d0b3e".into()),
                limits: hermit_proto::Limits::default(),
            },
            interactive: false,
            detach: true,
            boot_timeout_secs: 1,
            accounts: users::Accounts {
                passwd: "root:x:0:0::/root:/bin/sh\napp:x:1000:1000::/home/app:/bin/sh\n".into(),
                group: "root:x:0:\napp:x:1000:\nvideo:x:44:app\n".into(),
            },
            stop_signal: SIGTERM,
        }
    }

    /// A detached container with the fake agent, speaking `version` and stopped with `stop_signal`, running under a
    /// monitor on a thread.
    #[cfg(unix)]
    fn detached(name: &str, version: &str, stop_signal: i32) -> (PathBuf, Containers, String, Process) {
        detached_with(name, version, stop_signal, false)
    }

    /// As [`detached`], started with `-i` when `interactive`.
    #[cfg(unix)]
    fn detached_with(
        name: &str,
        version: &str,
        stop_signal: i32,
        interactive: bool,
    ) -> (PathBuf, Containers, String, Process) {
        use crate::qemu::GuestArch;
        let dir = scratch(name);
        crate::qemu::testing::fake_bundle(&dir, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        std::fs::write(dir.join("agent.version"), version).unwrap();
        let containers = Containers::open(&dir).unwrap();
        let made = containers.create(containers::Spec::default(), 0).unwrap();
        let mut launch = launch_with(&[], None);
        launch.qemu = dir.join("bin").join(GuestArch::X86_64.binary_name());
        launch.qemu_log = dir.join("qemu.log");
        launch.console_log = dir.join("console.log");
        launch.command.argv = vec!["wait".into()];
        launch.command.layers.clear();
        launch.stop_signal = stop_signal;
        launch.interactive = interactive;
        launch.boot_timeout_secs = 10;
        std::fs::write(
            containers.dir(&made.id).join(LAUNCH),
            serde_json::to_vec(&launch).unwrap(),
        )
        .unwrap();
        let log = dir.join(MONITOR_LOG);
        let mut process = testing::in_thread(Path::new("/unused"), &dir, &made.id, &log).unwrap();
        let mut conn = connect_new(&dir, &made.id, &mut process, &log).unwrap();
        send(conn.writer.as_mut(), &Request::Attach).unwrap();
        assert_eq!(next(&mut conn), Notice::Booted);
        assert_eq!(next(&mut conn), Notice::Started);
        (dir, containers, made.id, process)
    }

    #[cfg(unix)]
    #[test]
    fn attached_clients_get_the_output_from_now_on_and_pass_input_and_signals_on() {
        let _guard = crate::qemu::testing::exec_lock();
        let (dir, containers, id, _process) = detached_with("monitor-join", "1,7", SIGTERM, true);
        // Clients see output from when they join, so let the command's greeting be logged first.
        let log = containers.dir(&id).join(LOG);
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::metadata(&log).map_or(0, |m| m.len()) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let join = || {
            let mut conn = ipc::connect(&dir, &id).unwrap();
            send(conn.writer.as_mut(), &Request::Join).unwrap();
            assert_eq!(next(&mut conn), Notice::Started);
            conn
        };
        let (mut first, mut second, gone) = (join(), join(), join());
        drop(gone);
        // The fake command echoes its stdin: both attached clients see it, though only one typed it; one that
        // hung up is dropped without stopping anything.
        let ping = Request::Stdin {
            data: b64::encode(b"ping\n"),
        };
        send(first.writer.as_mut(), &ping).unwrap();
        let echoed = Notice::Output {
            stream: Stream::Stdout,
            data: b64::encode(b"ping\n"),
        };
        assert_eq!(next(&mut first), echoed);
        assert_eq!(next(&mut second), echoed);
        // Hanging up detaches: the container goes on, and the other client still gets its output.
        drop(first);
        assert!(containers.find(&id).unwrap().running);
        send(second.writer.as_mut(), &ping).unwrap();
        assert_eq!(next(&mut second), echoed);
        // A signal from an attached client reaches the command, which ends on SIGTERM; the client hears how.
        send(second.writer.as_mut(), &Request::Signal { signal: SIGTERM }).unwrap();
        assert_eq!(next(&mut second), Notice::Exited { code: 143 });
        assert_eq!(next(&mut second), Notice::Done);
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(143));
    }

    #[cfg(unix)]
    #[test]
    fn stop_sends_the_stop_signal_then_kills_after_the_grace_period() {
        let _guard = crate::qemu::testing::exec_lock();
        // A command that ends on SIGTERM ends with 143, well within its grace period.
        let (dir, containers, id, _process) = detached("monitor-stop-term", "1,6", SIGTERM);
        let started = Instant::now();
        assert_eq!(stop(&containers, &dir, &id, Some(30), None), Ok(()));
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(143));

        // One that ignores its stop signal is killed once the grace period is over; kill passes signals on.
        let (dir, containers, id, _process) = detached("monitor-stop-int", "1,6", 2);
        let wanted = [id.clone()];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(kill_command(&containers, &dir, &wanted, 10, &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out).unwrap(), format!("{id}\n"));
        assert!(containers.find(&id).unwrap().running, "SIGUSR1 is ignored");
        let started = Instant::now();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            stop_command(&containers, &dir, &wanted, Some(1), None, &mut out, &mut err),
            0
        );
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(STOPPED));
        // Its stop signal can be chosen instead.
        let (dir, containers, id, _process) = detached("monitor-stop-signal", "1,6", 2);
        assert_eq!(stop(&containers, &dir, &id, Some(30), Some(SIGTERM)), Ok(()));
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(143));
    }

    #[cfg(unix)]
    #[test]
    fn a_guest_without_signals_is_stopped_at_once_and_only_killed() {
        let _guard = crate::qemu::testing::exec_lock();
        let (dir, containers, id, _process) = detached("monitor-old-agent", "1,5", SIGTERM);
        let wanted = [id.clone()];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(kill_command(&containers, &dir, &wanted, SIGTERM, &mut out, &mut err), 1);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("too old to pass signals on"), "{err}");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(kill_command(&containers, &dir, &wanted, SIGKILL, &mut out, &mut err), 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while containers.find(&id).unwrap().running {
            assert!(Instant::now() < deadline, "SIGKILL did not stop it");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(STOPPED));
        // stop does not wait out a grace period it cannot use.
        let (dir, containers, id, _process) = detached("monitor-old-agent-stop", "1,5", SIGTERM);
        let started = Instant::now();
        assert_eq!(stop(&containers, &dir, &id, Some(30), None), Ok(()));
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(containers.find(&id).unwrap().state.exit_code, Some(STOPPED));
    }

    #[test]
    fn an_exec_runs_with_the_containers_settings_unless_it_says_otherwise() {
        let app = hermit_proto::User {
            uid: 1000,
            gid: 1000,
            groups: vec![44],
        };
        let launch = launch_with(&["PATH=/bin", "HOME=/root", "EXTRA=0"], None);
        let command = exec_command(&launch, &exec_request(&["id"], false, None)).unwrap();
        assert_eq!(command.argv, ["id"]);
        assert_eq!(command.env, ["PATH=/bin", "HOME=/root", "EXTRA=1"]);
        assert_eq!(command.cwd, "/srv");
        assert_eq!(command.user, None);
        assert!(
            command.layers.is_empty() && command.hostname.is_none(),
            "the container's own"
        );

        // -u resolves against the image's accounts, and brings that user's home.
        let command = exec_command(&launch, &exec_request(&["id"], false, Some("app"))).unwrap();
        assert_eq!(command.user, Some(app.clone()));
        assert_eq!(command.env, ["PATH=/bin", "HOME=/home/app", "EXTRA=1"]);
        let err = exec_command(&launch, &exec_request(&["id"], false, Some("ghost"))).unwrap_err();
        assert!(err.contains("ghost"), "{err}");

        // The container's user unless -u says otherwise, root included; -w and -e win.
        let launch = launch_with(&["PATH=/bin"], Some(app.clone()));
        let mut request = exec_request(&["pwd"], true, None);
        if let Request::Exec { workdir, env, .. } = &mut request {
            *workdir = Some("/tmp".into());
            env.push("PATH=/usr/bin".into());
        }
        let command = exec_command(&launch, &request).unwrap();
        assert_eq!(command.user, Some(app));
        assert_eq!(command.cwd, "/tmp");
        assert_eq!(command.env, ["PATH=/usr/bin", "EXTRA=1"]);
        let as_root = exec_command(&launch, &exec_request(&["id"], false, Some("0"))).unwrap();
        assert_eq!(as_root.user, None);
        assert!(exec_command(&launch, &Request::Signal { signal: 9 }).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_foreground_container_stops_when_its_run_goes_away() {
        use crate::qemu::GuestArch;
        let _guard = crate::qemu::testing::exec_lock();
        let dir = scratch("monitor-foreground");
        crate::qemu::testing::fake_bundle(&dir, "QEMU emulator version 11.1.3 (oci-hermit-qemu-4)", "tcg");
        let containers = Containers::open(&dir).unwrap();
        let made = containers.create(containers::Spec::default(), 0).unwrap();
        let launch = Launch {
            qemu: dir.join("bin").join(GuestArch::X86_64.binary_name()),
            args: Vec::new(),
            qemu_log: dir.join("qemu.log"),
            console_log: dir.join("console.log"),
            command: supervisor::Command {
                argv: vec!["wait".into()],
                env: Vec::new(),
                cwd: "/".into(),
                tty: false,
                size: None,
                user: None,
                layers: Vec::new(),
                hostname: None,
                limits: hermit_proto::Limits::default(),
            },
            interactive: true,
            detach: false,
            boot_timeout_secs: 10,
            accounts: users::Accounts::default(),
            stop_signal: SIGTERM,
        };
        std::fs::write(
            containers.dir(&made.id).join(LAUNCH),
            serde_json::to_vec(&launch).unwrap(),
        )
        .unwrap();
        let log = dir.join(MONITOR_LOG);
        let mut process = testing::in_thread(Path::new("/unused"), &dir, &made.id, &log).unwrap();
        let mut conn = connect_new(&dir, &made.id, &mut process, &log).unwrap();
        send(conn.writer.as_mut(), &Request::Attach).unwrap();
        assert_eq!(next(&mut conn), Notice::Booted);
        assert_eq!(next(&mut conn), Notice::Started);
        let hi = Notice::Output {
            stream: Stream::Stdout,
            data: b64::encode(b"hi\n"),
        };
        assert_eq!(next(&mut conn), hi);
        // Its stdin reaches the command.
        let yo = b64::encode(b"yo\n");
        send(conn.writer.as_mut(), &Request::Stdin { data: yo.clone() }).unwrap();
        assert_eq!(
            next(&mut conn),
            Notice::Output {
                stream: Stream::Stdout,
                data: yo
            }
        );
        // Only the run that started it attaches.
        let mut other = ipc::connect(&dir, &made.id).unwrap();
        send(other.writer.as_mut(), &Request::Attach).unwrap();
        assert!(matches!(next(&mut other), Notice::Failed { .. }));

        // An exec runs beside it: its output goes to it alone, and so does its exit code.
        let mut exec = ipc::connect(&dir, &made.id).unwrap();
        send(exec.writer.as_mut(), &exec_request(&["ls"], false, None)).unwrap();
        assert_eq!(next(&mut exec), Notice::Started);
        assert_eq!(
            next(&mut exec),
            Notice::Output {
                stream: Stream::Stdout,
                data: b64::encode(b"ex\n"),
            }
        );
        assert_eq!(next(&mut exec), Notice::Exited { code: 5 });
        assert_eq!(next(&mut exec), Notice::Done);
        // One with -i gets its stdin, and ends when that does.
        let mut cat = ipc::connect(&dir, &made.id).unwrap();
        send(cat.writer.as_mut(), &exec_request(&["cat"], true, None)).unwrap();
        assert_eq!(next(&mut cat), Notice::Started);
        let meow = b64::encode(b"meow\n");
        send(cat.writer.as_mut(), &Request::Stdin { data: meow.clone() }).unwrap();
        assert_eq!(
            next(&mut cat),
            Notice::Output {
                stream: Stream::Stdout,
                data: meow
            }
        );
        send(cat.writer.as_mut(), &Request::StdinClosed).unwrap();
        assert_eq!(next(&mut cat), Notice::Exited { code: 0 });
        assert_eq!(next(&mut cat), Notice::Done);
        // A user the image does not have is refused before anything starts.
        let mut ghost = ipc::connect(&dir, &made.id).unwrap();
        send(ghost.writer.as_mut(), &exec_request(&["id"], false, Some("ghost"))).unwrap();
        assert!(matches!(next(&mut ghost), Notice::Failed { message } if message.contains("ghost")));
        assert_eq!(next(&mut ghost), Notice::Done);
        // One still running when the container stops is killed with it.
        let mut idle = ipc::connect(&dir, &made.id).unwrap();
        send(idle.writer.as_mut(), &exec_request(&["cat"], true, None)).unwrap();
        assert_eq!(next(&mut idle), Notice::Started);

        // The run going away stops the container.
        drop(conn);
        assert_eq!(next(&mut idle), Notice::Exited { code: STOPPED });
        assert_eq!(next(&mut idle), Notice::Done);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !process.has_exited() {
            assert!(Instant::now() < deadline, "the monitor did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        let ended = containers.find(&made.id).unwrap();
        assert!(!ended.running);
        assert_eq!(ended.state.exit_code, Some(STOPPED));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        logs(&containers, &made.id, false, &mut out, &mut err).unwrap();
        assert_eq!(out, b"hi\nyo\n");
    }

    #[test]
    fn stopping_a_container_nothing_runs_is_done_at_once() {
        let dir = scratch("monitor-stop-idle");
        let containers = Containers::open(&dir).unwrap();
        let made = containers.create(containers::Spec::default(), 0).unwrap();
        assert_eq!(stop(&containers, &dir, &made.id, None, None), Ok(()));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let wanted = [made.name.clone(), "ghost".into()];
        assert_eq!(
            stop_command(&containers, &dir, &wanted, None, None, &mut out, &mut err),
            1
        );
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", made.name));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "oci-hermit: no such container: ghost\n"
        );
        // A running container whose monitor cannot be reached is reported.
        let running = containers.start(&made.id, 0).unwrap();
        let err = stop(&containers, &dir, &made.id, None, None).unwrap_err();
        assert!(err.starts_with("cannot reach its monitor"), "{err}");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(kill_command(&containers, &dir, &wanted, SIGKILL, &mut out, &mut err), 1);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("cannot reach its monitor"), "{err}");
        assert!(err.contains("no such container: ghost"), "{err}");
        drop(running);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            kill_command(&containers, &dir, &wanted[..1], SIGKILL, &mut out, &mut err),
            1
        );
        assert!(String::from_utf8(err).unwrap().contains("is not running"));
    }
}
