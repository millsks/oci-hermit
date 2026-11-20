//! The container state store (#20): one folder per container under `<data>/containers/`, kept across commands.
//!
//! `<id>/container.json` says what the container is. It is written once, last, so a folder without it is a container
//! being created or removed, never one to list. `<id>/state.json` says what it is doing, and only the process running
//! it writes that, holding `<id>/running.lock` for as long as it runs. A container whose state says running but whose
//! lock is free was left by a process that died. Creating, starting and removing take `containers/lock`, so names
//! stay unique and nothing is removed while it starts.

use crate::store::write_atomically;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONTAINER: &str = "container.json";
const STATE: &str = "state.json";
const RUNNING: &str = "running.lock";

/// How long to wait for `running.lock` before taking it as held: a command reading the state holds it for a moment.
const PATIENCE: Duration = Duration::from_secs(1);

/// What a container is, fixed when it is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    /// 64 hex digits.
    pub id: String,
    pub name: String,
    /// The image as it was asked for: a name, an ID or a path.
    pub image: String,
    /// The image's config digest, which Docker and Podman call the image ID.
    pub image_id: String,
    /// `linux/amd64` or `linux/arm64`.
    pub platform: String,
    /// What runs: the entrypoint and command.
    pub command: Vec<String>,
    /// Seconds since the Unix epoch.
    pub created: i64,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub interactive: bool,
    /// `-u` as given, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `--rm`: removed when it exits.
    #[serde(default)]
    pub auto_remove: bool,
    /// Its limits and the sandbox sized for them.
    #[serde(default)]
    pub resources: Resources,
}

/// A container's `--cpus` and `--memory`, and the sandbox VM `run` sized for them (#24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
    /// CPU time in billionths of a CPU, as Docker's `NanoCpus`; `None` is no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nano_cpus: Option<u64>,
    /// Memory in bytes; `None` is no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    /// The sandbox's emulated CPUs.
    #[serde(default)]
    pub vcpus: u32,
    /// The sandbox's memory, in MiB.
    #[serde(default)]
    pub vm_memory_mib: u32,
}

/// What a new container should be; [`Containers::create`] gives it an ID, and a name if it has none.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub name: Option<String>,
    pub image: String,
    pub image_id: String,
    pub platform: String,
    pub command: Vec<String>,
    pub tty: bool,
    pub interactive: bool,
    pub user: Option<String>,
    pub auto_remove: bool,
    pub resources: Resources,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Created,
    Running,
    Exited,
}

/// What a container is doing, as the process running it last wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    pub status: Status,
    /// The process running it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why oci-hermit could not run it, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A container with its state, and whether a process is running it now.
#[derive(Debug, Clone)]
pub struct Listed {
    pub container: Container,
    pub state: State,
    pub running: bool,
}

impl Listed {
    /// The STATUS column, as Docker words it: `Up 5 minutes`, `Exited (0) 2 hours ago`, `Created`.
    pub fn status_text(&self, now: i64) -> String {
        if self.running {
            let since = self.state.started.unwrap_or(now);
            return format!("Up {}", crate::images::duration(now - since));
        }
        match self.state.status {
            Status::Created => "Created".into(),
            // The process running it ended without saying how the container did.
            Status::Running => "Exited (unknown)".into(),
            Status::Exited => {
                let code = self.state.exit_code.map_or("unknown".into(), |c| c.to_string());
                match self.state.finished {
                    Some(at) => format!("Exited ({code}) {}", crate::images::ago(now - at)),
                    None => format!("Exited ({code})"),
                }
            }
        }
    }
}

/// A container as `inspect` shows it, in Docker's shape: `HostConfig` holds its limits (`NanoCpus`, `Memory`; 0
/// is none), and `Sandbox` the VM it runs in.
pub fn inspect(listed: &Listed) -> Inspected {
    use crate::images::format_time;
    let (c, s) = (&listed.container, &listed.state);
    // Docker's zero time, for a container that has not started or finished.
    let time = |t: Option<i64>| t.map_or("0001-01-01T00:00:00Z".into(), format_time);
    let status = match s.status {
        _ if listed.running => "running",
        Status::Created => "created",
        Status::Running | Status::Exited => "exited",
    };
    let r = &c.resources;
    Inspected {
        id: c.id.clone(),
        created: format_time(c.created),
        path: c.command.first().cloned().unwrap_or_default(),
        args: c.command.get(1..).unwrap_or_default().to_vec(),
        state: serde_json::json!({
            "Status": status,
            "Running": listed.running,
            "ExitCode": s.exit_code.unwrap_or(0),
            "Error": s.error.as_deref().unwrap_or_default(),
            "StartedAt": time(s.started),
            "FinishedAt": time(s.finished),
        }),
        image: c.image_id.clone(),
        name: format!("/{}", c.name),
        platform: "linux",
        config: serde_json::json!({
            "Hostname": short(&c.id),
            "User": c.user.as_deref().unwrap_or_default(),
            "Tty": c.tty,
            "OpenStdin": c.interactive,
            "Cmd": c.command,
            "Image": c.image,
        }),
        host_config: serde_json::json!({
            "NanoCpus": r.nano_cpus.unwrap_or(0),
            "Memory": r.memory.unwrap_or(0),
            "AutoRemove": c.auto_remove,
        }),
        // Unknown (null) for a container recorded before sandboxes were sized per container.
        sandbox: serde_json::json!({
            "Platform": c.platform,
            "Cpus": (r.vcpus > 0).then_some(r.vcpus),
            "Memory": (r.vm_memory_mib > 0).then(|| u64::from(r.vm_memory_mib) * 1024 * 1024),
        }),
    }
}

/// One container as `inspect` shows it, in the order `docker inspect` uses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Inspected {
    pub id: String,
    pub created: String,
    pub path: String,
    pub args: Vec<String>,
    pub state: serde_json::Value,
    pub image: String,
    pub name: String,
    pub platform: &'static str,
    pub config: serde_json::Value,
    pub host_config: serde_json::Value,
    pub sandbox: serde_json::Value,
}

#[derive(Debug)]
pub enum ContainerError {
    Io(std::io::Error),
    BadName(String),
    NameInUse {
        name: String,
        id: String,
    },
    NotFound(String),
    Ambiguous(String),
    Running(String),
    /// `rm -f` could not stop a running container.
    Stop {
        name: String,
        reason: String,
    },
    Invalid(String),
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerError::Io(e) => write!(f, "the container store: {e}"),
            ContainerError::BadName(name) => write!(
                f,
                "invalid container name {name:?}: use letters, digits, `_`, `.` and `-`, starting with a letter or \
                 digit, at least two characters"
            ),
            ContainerError::NameInUse { name, id } => write!(
                f,
                "the container name {name:?} is already in use by container {}; remove it (oci-hermit rm {name}) \
                 or choose another name",
                short(id)
            ),
            ContainerError::NotFound(wanted) => write!(f, "no such container: {wanted}"),
            ContainerError::Ambiguous(wanted) => {
                write!(
                    f,
                    "{wanted} matches more than one container; give more of its ID, or its name"
                )
            }
            ContainerError::Running(name) => write!(
                f,
                "container {name} is running; stop it first (oci-hermit stop {name}), or remove it with rm -f"
            ),
            ContainerError::Stop { name, reason } => write!(f, "could not stop container {name}: {reason}"),
            ContainerError::Invalid(what) => write!(f, "the container store: {what}"),
        }
    }
}

impl std::error::Error for ContainerError {}

impl From<std::io::Error> for ContainerError {
    fn from(e: std::io::Error) -> Self {
        ContainerError::Io(e)
    }
}

/// The first 12 digits of an ID, as Docker shows it.
pub fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// The containers in a data directory.
pub struct Containers {
    dir: PathBuf,
}

impl Containers {
    /// The containers under `data_dir`, creating the folder if there is none yet.
    pub fn open(data_dir: &Path) -> std::io::Result<Containers> {
        let dir = data_dir.join("containers");
        std::fs::create_dir_all(&dir)?;
        Ok(Containers { dir })
    }

    /// The folder that holds container `id`'s files.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// Whether a process is running container `id` now.
    pub fn is_running(&self, id: &str) -> std::io::Result<bool> {
        held(&self.dir.join(id).join(RUNNING))
    }

    /// Holds `containers/lock` until dropped.
    fn lock(&self) -> std::io::Result<File> {
        let file = open_lock(&self.dir.join("lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Records a new container, `Created`, with a fresh ID and its name (or a generated one).
    pub fn create(&self, spec: Spec, now: i64) -> Result<Container, ContainerError> {
        if let Some(name) = &spec.name {
            check_name(name)?;
        }
        let _lock = self.lock()?;
        self.sweep();
        let existing = self.list()?;
        let taken = |name: &str| existing.iter().find(|l| l.container.name == name);
        let name = match spec.name {
            Some(name) => {
                if let Some(other) = taken(&name) {
                    return Err(ContainerError::NameInUse {
                        name,
                        id: other.container.id.clone(),
                    });
                }
                name
            }
            None => generate_name(|name| taken(name).is_none()),
        };
        let (id, dir) = loop {
            let id = new_id();
            let dir = self.dir.join(&id);
            match std::fs::create_dir(&dir) {
                Ok(()) => break (id, dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        };
        let container = Container {
            id,
            name,
            image: spec.image,
            image_id: spec.image_id,
            platform: spec.platform,
            command: spec.command,
            created: now,
            tty: spec.tty,
            interactive: spec.interactive,
            user: spec.user,
            auto_remove: spec.auto_remove,
            resources: spec.resources,
        };
        write_json(&dir.join(STATE), &State::default())?;
        write_json(&dir.join(CONTAINER), &container)?;
        Ok(container)
    }

    /// Marks the container running in this process until the returned [`Running`] says it exited or is dropped.
    pub fn start(&self, id: &str, now: i64) -> Result<Running, ContainerError> {
        let _lock = self.lock()?;
        let dir = self.dir.join(id);
        let container = read_container(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_string()))?;
        let running = open_lock(&dir.join(RUNNING))?;
        if !take(&running, PATIENCE)? {
            return Err(ContainerError::Running(container.name));
        }
        let state = State {
            status: Status::Running,
            pid: Some(std::process::id()),
            started: Some(now),
            ..State::default()
        };
        write_json(&dir.join(STATE), &state)?;
        Ok(Running {
            _lock: running,
            path: dir.join(STATE),
            state,
        })
    }

    /// Every container, newest first.
    pub fn list(&self) -> Result<Vec<Listed>, ContainerError> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let dir = entry.path();
            let Some(container) = read_container(&dir)? else {
                continue;
            };
            let state = match std::fs::read(dir.join(STATE)) {
                Ok(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|e| ContainerError::Invalid(format!("{}: {e}", dir.join(STATE).display())))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
                Err(e) => return Err(e.into()),
            };
            let running = held(&dir.join(RUNNING))?;
            found.push(Listed {
                container,
                state,
                running,
            });
        }
        found.sort_by(|a, b| {
            b.container
                .created
                .cmp(&a.container.created)
                .then_with(|| a.container.name.cmp(&b.container.name))
        });
        Ok(found)
    }

    /// The container `wanted` names: its whole ID, its name, or a prefix of its ID that no other container has.
    pub fn find(&self, wanted: &str) -> Result<Listed, ContainerError> {
        let all = self.list()?;
        if let Some(found) = all
            .iter()
            .find(|l| l.container.id == wanted)
            .or_else(|| all.iter().find(|l| l.container.name == wanted))
        {
            return Ok(found.clone());
        }
        let mut matches = all.iter().filter(|l| {
            !wanted.is_empty() && wanted.bytes().all(|b| b.is_ascii_hexdigit()) && l.container.id.starts_with(wanted)
        });
        match (matches.next(), matches.next()) {
            (Some(found), None) => Ok(found.clone()),
            (Some(_), Some(_)) => Err(ContainerError::Ambiguous(wanted.to_string())),
            _ => Err(ContainerError::NotFound(wanted.to_string())),
        }
    }

    /// Removes a container that is not running.
    pub fn remove(&self, id: &str) -> Result<(), ContainerError> {
        let _lock = self.lock()?;
        let dir = self.dir.join(id);
        let container = read_container(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_string()))?;
        let running = open_lock(&dir.join(RUNNING))?;
        if !take(&running, PATIENCE)? {
            return Err(ContainerError::Running(container.name));
        }
        // Without container.json the folder is no container any more, even if the rest cannot go yet (Windows
        // keeps a file another command has open); the next create or remove clears what is left.
        std::fs::remove_file(dir.join(CONTAINER))?;
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Removes every container that is not running, as `container prune`: their IDs, oldest first, and the bytes
    /// their folders (record, launch files and logs) took. One that starts meanwhile is left alone.
    pub fn prune(&self) -> Result<(Vec<String>, u64), ContainerError> {
        let mut stopped: Vec<Listed> = self.list()?.into_iter().filter(|l| !l.running).collect();
        stopped.sort_by_key(|l| l.container.created);
        let (mut removed, mut freed) = (Vec::new(), 0);
        for listed in stopped {
            let id = &listed.container.id;
            let bytes = folder_bytes(&self.dir.join(id));
            match self.remove(id) {
                Ok(()) => {
                    removed.push(id.clone());
                    freed += bytes;
                }
                Err(ContainerError::Running(_) | ContainerError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok((removed, freed))
    }

    /// What `system df` says of containers.
    pub fn usage(&self) -> Result<Usage, ContainerError> {
        let mut usage = Usage::default();
        for listed in self.list()? {
            let bytes = folder_bytes(&self.dir.join(&listed.container.id));
            usage.total += 1;
            usage.bytes += bytes;
            if listed.running {
                usage.running += 1;
            } else {
                usage.reclaimable += bytes;
            }
            if !usage.images.contains(&listed.container.image_id) {
                usage.images.push(listed.container.image_id);
            }
        }
        Ok(usage)
    }

    /// Clears folders left without a container.json by a remove that could not finish. Callers hold the lock.
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !path.join(CONTAINER).exists() {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// The containers' share of the data folder, for `system df`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub total: usize,
    pub running: usize,
    /// What their folders take.
    pub bytes: u64,
    /// What the stopped ones' folders take, which `container prune` would free.
    pub reclaimable: u64,
    /// The image IDs they were made from.
    pub images: Vec<String>,
}

/// The bytes the files under `dir` take.
fn folder_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => folder_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// A container this process is running; dropping it without [`Running::exited`] leaves it `Exited (unknown)`.
pub struct Running {
    _lock: File,
    path: PathBuf,
    state: State,
}

impl Running {
    /// Records how the container ended: its exit code, or why oci-hermit could not run it.
    pub fn exited(mut self, exit_code: Option<i32>, error: Option<String>, now: i64) -> std::io::Result<()> {
        self.state.status = Status::Exited;
        self.state.pid = None;
        self.state.finished = Some(now);
        self.state.exit_code = exit_code;
        self.state.error = error;
        write_json(&self.path, &self.state)
    }
}

/// `ps`: running containers, or all of them with `all`.
pub fn ps(
    containers: &Containers,
    all: bool,
    quiet: bool,
    no_trunc: bool,
    now: i64,
    out: &mut impl Write,
) -> Result<(), ContainerError> {
    let listed: Vec<Listed> = containers.list()?.into_iter().filter(|l| all || l.running).collect();
    let id = |l: &Listed| {
        if no_trunc {
            l.container.id.clone()
        } else {
            short(&l.container.id).to_string()
        }
    };
    if quiet {
        for l in &listed {
            writeln!(out, "{}", id(l))?;
        }
        return Ok(());
    }
    let rows: Vec<Vec<String>> = listed
        .iter()
        .map(|l| {
            vec![
                id(l),
                l.container.image.clone(),
                command_text(&l.container.command, no_trunc),
                crate::images::ago(now - l.container.created),
                l.status_text(now),
                String::new(),
                l.container.name.clone(),
            ]
        })
        .collect();
    let headers = [
        "CONTAINER ID",
        "IMAGE",
        "COMMAND",
        "CREATED",
        "STATUS",
        "PORTS",
        "NAMES",
    ];
    out.write_all(crate::images::table(&headers, &rows).as_bytes())?;
    Ok(())
}

/// Stops a running container, given its ID, for `rm -f`.
pub type Stopper<'a> = &'a dyn Fn(&str) -> Result<(), String>;

/// `rm`: removes each container, printing what was asked for; 1 if any could not go. A running container is
/// refused, or with `stop` (`rm -f`) stopped first.
pub fn rm(
    containers: &Containers,
    wanted: &[String],
    stop: Option<Stopper<'_>>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let mut code = 0;
    for name in wanted {
        let removed = containers.find(name).and_then(|l| {
            if l.running {
                let stop = stop.ok_or_else(|| ContainerError::Running(l.container.name.clone()))?;
                stop(&l.container.id).map_err(|reason| ContainerError::Stop {
                    name: l.container.name.clone(),
                    reason,
                })?;
            }
            containers.remove(&l.container.id)
        });
        match removed {
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

/// The COMMAND column: the command line in quotes, cut to 20 characters unless `no_trunc`, as Docker shows it.
fn command_text(command: &[String], no_trunc: bool) -> String {
    let line = command.join(" ");
    let shown = if !no_trunc && line.chars().count() > 20 {
        format!("{}…", line.chars().take(19).collect::<String>())
    } else {
        line
    };
    format!("\"{shown}\"")
}

/// Docker's rule for names: a letter or digit, then letters, digits, `_`, `.` or `-`; two characters at least.
pub fn check_name(name: &str) -> Result<(), ContainerError> {
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest = chars.as_str();
    if first && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        Ok(())
    } else {
        Err(ContainerError::BadName(name.to_string()))
    }
}

const ADJECTIVES: &[&str] = &[
    "amber",
    "bold",
    "brisk",
    "calm",
    "clever",
    "cosy",
    "curious",
    "dapper",
    "eager",
    "gentle",
    "glad",
    "hardy",
    "jolly",
    "keen",
    "kind",
    "lively",
    "lucky",
    "mellow",
    "merry",
    "nimble",
    "patient",
    "plucky",
    "proud",
    "quiet",
    "quick",
    "rustic",
    "serene",
    "shy",
    "sleepy",
    "steady",
    "sturdy",
    "sunny",
    "tidy",
    "tranquil",
    "trusty",
    "vivid",
    "wandering",
    "wary",
    "wise",
    "witty",
];

const NOUNS: &[&str] = &[
    "abalone",
    "anemone",
    "barnacle",
    "clam",
    "cockle",
    "conch",
    "coral",
    "cowrie",
    "crab",
    "cuttlefish",
    "dolphin",
    "eel",
    "gull",
    "kelp",
    "krill",
    "limpet",
    "lobster",
    "manatee",
    "mussel",
    "narwhal",
    "nautilus",
    "octopus",
    "otter",
    "oyster",
    "pelican",
    "periwinkle",
    "plover",
    "puffin",
    "scallop",
    "seahorse",
    "seal",
    "shrimp",
    "sponge",
    "squid",
    "starfish",
    "tern",
    "turtle",
    "urchin",
    "walrus",
    "whelk",
];

/// `adjective_noun`, or with a number after it once the plain ones seem taken.
fn generate_name(free: impl Fn(&str) -> bool) -> String {
    for attempt in 0u64.. {
        let n = random();
        let name = format!(
            "{}_{}",
            ADJECTIVES[(n % ADJECTIVES.len() as u64) as usize],
            NOUNS[((n >> 32) % NOUNS.len() as u64) as usize]
        );
        let name = if attempt < 16 {
            name
        } else {
            format!("{name}{}", attempt - 15)
        };
        if free(&name) {
            return name;
        }
    }
    unreachable!("names run out after u64::MAX attempts")
}

/// 64 random hex digits.
fn new_id() -> String {
    let mut hasher = Sha256::new();
    for _ in 0..4 {
        hasher.update(random().to_le_bytes());
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random number from the standard library's per-process random hash keys, mixed with the time.
fn random() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.write_u128(nanos);
    h.write_u32(std::process::id());
    h.finish()
}

fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).truncate(false).write(true).open(path)
}

/// Takes `file`'s lock alone, waiting up to `patience` for commands that are only reading it.
fn take(file: &File, patience: Duration) -> std::io::Result<bool> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Whether some process holds the lock at `path` alone, which only the process running a container does.
fn held(path: &Path) -> std::io::Result<bool> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn read_container(dir: &Path) -> Result<Option<Container>, ContainerError> {
    let path = dir.join(CONTAINER);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ContainerError::Invalid(format!("{}: {e}", path.display()))),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    write_atomically(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    fn spec(name: Option<&str>) -> Spec {
        Spec {
            name: name.map(String::from),
            image: "alpine:3.20".into(),
            image_id: "sha256:abc".into(),
            platform: "linux/amd64".into(),
            command: vec!["sh".into(), "-c".into(), "echo hello from the sandbox".into()],
            ..Spec::default()
        }
    }

    #[test]
    fn inspect_shows_limits_and_the_sandbox_as_docker_shapes_them() {
        let dir = scratch("containers-inspect");
        let containers = Containers::open(&dir).unwrap();
        let made = containers
            .create(
                Spec {
                    user: Some("app".into()),
                    resources: Resources {
                        nano_cpus: Some(1_500_000_000),
                        memory: Some(1 << 30),
                        vcpus: 2,
                        vm_memory_mib: 1152,
                    },
                    ..spec(Some("sized"))
                },
                1_735_689_600,
            )
            .unwrap();
        let shown = serde_json::to_value(inspect(&containers.find("sized").unwrap())).unwrap();
        assert_eq!(shown["Id"], made.id);
        assert_eq!(shown["Name"], "/sized");
        assert_eq!(shown["Created"], "2025-01-01T00:00:00Z");
        assert_eq!(shown["Path"], "sh");
        assert_eq!(shown["Args"], serde_json::json!(["-c", "echo hello from the sandbox"]));
        assert_eq!(shown["State"]["Status"], "created");
        assert_eq!(shown["State"]["StartedAt"], "0001-01-01T00:00:00Z");
        assert_eq!(shown["Config"]["User"], "app");
        assert_eq!(shown["Config"]["Hostname"], short(&made.id));
        assert_eq!(shown["HostConfig"]["NanoCpus"], 1_500_000_000u64);
        assert_eq!(shown["HostConfig"]["Memory"], 1u64 << 30);
        assert_eq!(shown["Sandbox"]["Cpus"], 2);
        assert_eq!(shown["Sandbox"]["Memory"], 1152u64 << 20);
        assert_eq!(shown["Sandbox"]["Platform"], "linux/amd64");

        let running = containers.start(&made.id, 1_735_689_601).unwrap();
        let shown = serde_json::to_value(inspect(&containers.find("sized").unwrap())).unwrap();
        assert_eq!(
            (shown["State"]["Status"].as_str(), shown["State"]["Running"].as_bool()),
            (Some("running"), Some(true))
        );
        assert_eq!(shown["State"]["StartedAt"], "2025-01-01T00:00:01Z");
        running.exited(Some(3), None, 1_735_689_662).unwrap();
        let shown = serde_json::to_value(inspect(&containers.find("sized").unwrap())).unwrap();
        assert_eq!(shown["State"]["Status"], "exited");
        assert_eq!(shown["State"]["ExitCode"], 3);
        assert_eq!(shown["State"]["FinishedAt"], "2025-01-01T00:01:02Z");

        // Without limits they read 0, as in Docker; a container from before #24 has no sandbox size.
        let plain = containers.create(spec(Some("plain")), 0).unwrap();
        let shown = serde_json::to_value(inspect(&containers.find(&plain.id).unwrap())).unwrap();
        assert_eq!(shown["HostConfig"]["NanoCpus"], 0);
        assert_eq!(shown["HostConfig"]["Memory"], 0);
        assert!(shown["Sandbox"]["Cpus"].is_null() && shown["Sandbox"]["Memory"].is_null());
    }

    #[test]
    fn containers_outlive_the_command_that_made_them() {
        let dir = scratch("containers-persist");
        let made = Containers::open(&dir).unwrap().create(spec(Some("web")), 100).unwrap();
        assert_eq!(made.id.len(), 64);
        assert!(made.id.bytes().all(|b| b.is_ascii_hexdigit()));

        // Another command, later: a new handle on the same folder sees the same container.
        let again = Containers::open(&dir).unwrap();
        let listed = again.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].container, made);
        assert_eq!(listed[0].state, State::default());
        assert!(!listed[0].running);
        assert_eq!(listed[0].status_text(200), "Created");
        for wanted in [made.id.as_str(), "web", short(&made.id), &made.id[..3]] {
            assert_eq!(again.find(wanted).unwrap().container.id, made.id, "{wanted}");
        }
        assert!(matches!(again.find("nothing"), Err(ContainerError::NotFound(_))));
        assert!(matches!(again.find(""), Err(ContainerError::NotFound(_))));

        again.remove(&made.id).unwrap();
        assert!(again.list().unwrap().is_empty());
        assert!(!dir.join("containers").join(&made.id).exists());
        assert!(matches!(again.remove(&made.id), Err(ContainerError::NotFound(_))));
    }

    #[test]
    fn names_are_checked_and_unique() {
        let containers = Containers::open(&scratch("containers-names")).unwrap();
        for bad in ["", "a", "-web", "_web", "web server", "web/1", "wéb"] {
            assert!(
                matches!(containers.create(spec(Some(bad)), 0), Err(ContainerError::BadName(_))),
                "{bad}"
            );
        }
        for good in ["ab", "web", "web-1", "Web_1.2", "9lives"] {
            containers.create(spec(Some(good)), 0).unwrap();
        }
        let first = containers.find("web").unwrap().container.id;
        let err = containers.create(spec(Some("web")), 0).unwrap_err();
        assert!(matches!(&err, ContainerError::NameInUse { id, .. } if *id == first));
        assert!(err.to_string().contains(short(&first)), "{err}");

        // Generated names follow the same rule and do not repeat.
        let mut names = std::collections::HashSet::new();
        for _ in 0..30 {
            let made = containers.create(spec(None), 0).unwrap();
            check_name(&made.name).unwrap();
            assert!(made.name.contains('_'), "{}", made.name);
            assert!(names.insert(made.name));
        }
    }

    #[test]
    fn generated_names_get_a_number_once_the_plain_ones_are_taken() {
        let name = generate_name(|name| name.ends_with(|c: char| c.is_ascii_digit()));
        assert!(name.ends_with('1'), "{name}");
        check_name(&name).unwrap();
    }

    #[test]
    fn a_running_container_is_seen_as_running_and_kept() {
        let containers = Containers::open(&scratch("containers-running")).unwrap();
        let made = containers.create(spec(Some("job")), 1000).unwrap();
        let running = containers.start(&made.id, 1000).unwrap();
        let seen = containers.find("job").unwrap();
        assert!(seen.running);
        assert_eq!(seen.state.status, Status::Running);
        assert_eq!(seen.state.pid, Some(std::process::id()));
        assert_eq!(seen.status_text(1300), "Up 5 minutes");

        assert!(matches!(
            containers.start(&made.id, 1000),
            Err(ContainerError::Running(_))
        ));
        assert!(matches!(containers.remove(&made.id), Err(ContainerError::Running(_))));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(rm(&containers, &["job".into()], None, &mut out, &mut err), 1);
        assert!(String::from_utf8(err).unwrap().contains("container job is running"));
        // rm -f stops it first; a stop that fails leaves it.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let refuse = |_: &str| Err("the monitor is not answering".to_string());
        assert_eq!(rm(&containers, &["job".into()], Some(&refuse), &mut out, &mut err), 1);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "oci-hermit: could not stop container job: the monitor is not answering\n"
        );

        running.exited(Some(3), None, 1400).unwrap();
        let seen = containers.find("job").unwrap();
        assert!(!seen.running);
        assert_eq!(seen.state.exit_code, Some(3));
        assert_eq!(seen.status_text(1400 + 7200), "Exited (3) 2 hours ago");

        // It can run again, and an oci-hermit error is kept with the state.
        let running = containers.start(&made.id, 9000).unwrap();
        running
            .exited(Some(125), Some("the sandbox did not boot".into()), 9001)
            .unwrap();
        let seen = containers.find("job").unwrap();
        assert_eq!(seen.state.error.as_deref(), Some("the sandbox did not boot"));
        assert_eq!(seen.state.started, Some(9000));

        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            rm(&containers, &["job".into(), "nothing".into()], None, &mut out, &mut err),
            1
        );
        assert_eq!(String::from_utf8(out).unwrap(), "job\n");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "oci-hermit: no such container: nothing\n"
        );
        assert!(containers.list().unwrap().is_empty());
    }

    #[test]
    fn rm_force_stops_a_running_container_then_removes_it() {
        let containers = Containers::open(&scratch("containers-rm-force")).unwrap();
        let made = containers.create(spec(Some("job")), 0).unwrap();
        let running = std::sync::Mutex::new(Some(containers.start(&made.id, 0).unwrap()));
        assert!(containers.is_running(&made.id).unwrap());
        let stop = |id: &str| {
            assert_eq!(id, made.id);
            let ran = running.lock().unwrap().take().unwrap();
            ran.exited(Some(137), None, 1).map_err(|e| e.to_string())
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(rm(&containers, &["job".into()], Some(&stop), &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out).unwrap(), "job\n");
        assert!(containers.list().unwrap().is_empty());
        assert!(!containers.is_running(&made.id).unwrap());
        assert!(
            containers
                .dir(&made.id)
                .ends_with(Path::new("containers").join(&made.id))
        );
    }

    #[test]
    fn a_container_whose_process_died_is_not_running() {
        let containers = Containers::open(&scratch("containers-died")).unwrap();
        let made = containers.create(spec(None), 0).unwrap();
        drop(containers.start(&made.id, 0).unwrap());
        let seen = containers.find(&made.name).unwrap();
        assert!(!seen.running);
        assert_eq!(seen.state.status, Status::Running);
        assert_eq!(seen.status_text(10), "Exited (unknown)");
        containers.remove(&made.id).unwrap();
    }

    #[test]
    fn concurrent_commands_keep_names_unique() {
        let dir = scratch("containers-concurrent");
        Containers::open(&dir).unwrap();
        let results: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let containers = Containers::open(&dir).unwrap();
                    let named = containers.create(spec(Some("shared")), 0);
                    let unnamed = containers.create(spec(None), 0).unwrap();
                    (named.is_ok(), unnamed)
                })
            })
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|(ok, _)| *ok).count(), 1);
        let listed = Containers::open(&dir).unwrap().list().unwrap();
        assert_eq!(listed.len(), 9);
        let names: std::collections::HashSet<_> = listed.iter().map(|l| l.container.name.clone()).collect();
        let ids: std::collections::HashSet<_> = listed.iter().map(|l| l.container.id.clone()).collect();
        assert_eq!((names.len(), ids.len()), (9, 9));
    }

    #[test]
    fn a_short_id_shared_by_two_containers_is_ambiguous() {
        let containers = Containers::open(&scratch("containers-ambiguous")).unwrap();
        // Seventeen IDs share a first digit somewhere.
        let ids: Vec<String> = (0..17).map(|_| containers.create(spec(None), 0).unwrap().id).collect();
        let shared = ids
            .iter()
            .find(|id| ids.iter().filter(|other| other[..1] == id[..1]).count() > 1)
            .unwrap();
        let err = containers.find(&shared[..1]).unwrap_err();
        assert!(matches!(err, ContainerError::Ambiguous(_)));
        assert!(err.to_string().contains("more than one container"));
    }

    #[test]
    fn leftovers_of_an_unfinished_remove_are_cleared() {
        let dir = scratch("containers-sweep");
        let containers = Containers::open(&dir).unwrap();
        let left = containers.dir.join("0123");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::write(left.join(STATE), "{}").unwrap();
        assert!(
            containers.list().unwrap().is_empty(),
            "a folder without container.json is not listed"
        );
        containers.create(spec(None), 0).unwrap();
        assert!(!left.exists());
    }

    #[test]
    fn a_damaged_record_is_an_error() {
        let containers = Containers::open(&scratch("containers-damaged")).unwrap();
        let made = containers.create(spec(None), 0).unwrap();
        std::fs::write(containers.dir.join(&made.id).join(STATE), "not json").unwrap();
        let err = containers.list().unwrap_err();
        assert!(err.to_string().contains("state.json"), "{err}");
        std::fs::write(containers.dir.join(&made.id).join(CONTAINER), "not json").unwrap();
        assert!(containers.list().unwrap_err().to_string().contains("container.json"));
    }

    #[test]
    fn ps_lists_like_docker() {
        let containers = Containers::open(&scratch("containers-ps")).unwrap();
        let old = containers.create(spec(Some("old")), 0).unwrap();
        containers
            .start(&old.id, 10)
            .unwrap()
            .exited(Some(0), None, 20)
            .unwrap();
        let new = containers.create(spec(Some("new")), 3000).unwrap();
        let _running = containers.start(&new.id, 3000).unwrap();
        let now = 3600 * 3;

        let mut out = Vec::new();
        ps(&containers, false, false, false, now, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("CONTAINER ID   IMAGE         COMMAND"), "{text}");
        assert!(lines[1].starts_with(short(&new.id)), "{text}");
        assert!(lines[1].contains("\"sh -c echo hello fr…\""), "{text}");
        assert!(lines[1].contains("Up 2 hours"), "{text}");
        assert!(lines[1].ends_with("new"), "{text}");

        let mut out = Vec::new();
        ps(&containers, true, false, false, now, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(
            text.lines().nth(2).unwrap().contains("Exited (0) 3 hours ago"),
            "{text}"
        );

        let mut out = Vec::new();
        ps(&containers, true, true, true, now, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n{}\n", new.id, old.id));

        let mut out = Vec::new();
        ps(&containers, false, false, true, now, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\"sh -c echo hello from the sandbox\"")
        );
    }
}
