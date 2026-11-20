//! `cp`: copies between this machine and a running container, through its monitor and the control channel (#23),
//! under the rules for bind mounts (docs/design.md): no host path that is a drive root, the home folder (or a folder
//! holding it) or inside a credentials folder; nothing that climbs out of the copy (`..`, a link pointing outside
//! it); no devices, FIFOs or sockets; no setuid bits.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use hermit_proto::{Entry, EntryKind, Existing, Target, copy_target, is_entry_path};

use crate::containers::Containers;
use crate::monitor::{self, Notice, Request};
use crate::supervisor::CopyInto;

/// One side of a copy, as `docker cp` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Host(String),
    Container { container: String, path: String },
}

/// `CONTAINER:PATH`, or a path on this machine: an absolute one, one starting with `.`, or one without a `:`.
pub fn location(arg: &str) -> Location {
    let local = Path::new(arg).is_absolute() || arg.starts_with('.') || is_drive_path(arg);
    match arg.split_once(':') {
        Some((container, path)) if !local && !container.is_empty() => Location::Container {
            container: container.to_string(),
            path: path.to_string(),
        },
        _ => Location::Host(arg.to_string()),
    }
}

/// `C:\x` or `C:/x`, a Windows path whatever this machine is.
fn is_drive_path(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

/// The folders under the home folder no copy may touch: credentials of every kind.
const CREDENTIALS: &[&str] = &[
    ".ssh",
    ".aws",
    ".kube",
    ".docker",
    ".gnupg",
    ".config/gcloud",
    ".azure",
    ".local/share/keyrings",
    "Library/Keychains",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
];

/// `path` made absolute, with every part that exists resolved as the filesystem has it.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut missing = Vec::new();
    let mut at = absolute.as_path();
    loop {
        match std::fs::canonicalize(at) {
            Ok(real) => return Ok(missing.iter().rev().fold(real, |p: PathBuf, part| p.join(part))),
            Err(_) => match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    at = parent;
                }
                _ => return Ok(absolute),
            },
        }
    }
}

/// Why `path` may not be copied to or from, if it may not: the bind-mount deny-list.
pub fn refused(path: &Path, home: Option<&Path>) -> Option<String> {
    let real = resolved(path).ok()?;
    let shown = path.display();
    if real.parent().is_none() {
        return Some(format!(
            "{shown} is a drive's root, which oci-hermit never shares with a container"
        ));
    }
    let home = home.and_then(|h| std::fs::canonicalize(h).ok())?;
    if home.starts_with(&real) {
        return Some(format!(
            "{shown} holds the home folder, which oci-hermit never shares with a container"
        ));
    }
    CREDENTIALS.iter().find_map(|folder| {
        real.starts_with(home.join(folder)).then(|| {
            format!(
                "{shown} is in {}, which oci-hermit never shares with a container",
                home.join(folder).display()
            )
        })
    })
}

/// The last part of a path a copy names, and whether it asks only for a folder's contents (`src/.`).
fn name_and_contents(path: &str) -> (String, bool) {
    let contents = path.ends_with("/.") || path.ends_with("\\.") || path == ".";
    let trimmed = path
        .trim_end_matches(['/', '\\'])
        .trim_end_matches("/.")
        .trim_end_matches("\\.");
    let trimmed = trimmed.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or_default();
    let name = if name == "." || name == ".." { "" } else { name };
    (name.to_string(), contents)
}

/// What a copy into the container reads: an entry, or bytes of the file the last entry named.
pub enum Item {
    Entry(Entry),
    Data(Vec<u8>),
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    match (meta.is_dir(), meta.permissions().readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

/// Whether a link at `entry_path` in a copy, pointing at `target`, stays inside the copy.
fn stays_inside(entry_path: &str, target: &str) -> bool {
    if target.starts_with('/') || target.starts_with('\\') || is_drive_path(target) {
        return false;
    }
    // From the folder holding the link.
    let mut depth = entry_path.split('/').filter(|p| !p.is_empty()).count() as isize - 1;
    for part in target.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// Reads `source` on this machine as a copy's entries, never following a link; what may not be copied is left out
/// with a warning.
pub fn read_tree(
    source: &Path,
    each: &mut dyn FnMut(Item) -> io::Result<()>,
    warn: &mut dyn FnMut(String),
) -> io::Result<u64> {
    let mut bytes = 0;
    let mut pending = vec![(source.to_path_buf(), String::new())];
    while let Some((path, entry_path)) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path)?;
        let kind = meta.file_type();
        let mode = mode_of(&meta);
        let mut entry = Entry {
            path: entry_path.clone(),
            kind: EntryKind::Other,
            mode,
            target: None,
        };
        if kind.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let target = target.to_str().map(|t| t.replace('\\', "/"));
            match target {
                Some(target) if stays_inside(&entry_path, &target) && !entry_path.is_empty() => {
                    entry.kind = EntryKind::Symlink;
                    entry.target = Some(target);
                    entry.mode = 0o777;
                }
                _ => {
                    warn(format!("left out {}: a link to outside what is copied", path.display()));
                    continue;
                }
            }
            each(Item::Entry(entry))?;
        } else if kind.is_dir() {
            entry.kind = EntryKind::Dir;
            each(Item::Entry(entry))?;
            let mut names = Vec::new();
            for child in std::fs::read_dir(&path)? {
                let name = child?.file_name();
                let name = name.into_string().map_err(|name| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("a name that is not UTF-8: {name:?}"),
                    )
                })?;
                names.push(name);
            }
            // Read in order: the stack takes the last first.
            names.sort();
            for name in names.into_iter().rev() {
                let child = if entry_path.is_empty() {
                    name.clone()
                } else {
                    format!("{entry_path}/{name}")
                };
                pending.push((path.join(&name), child));
            }
        } else if kind.is_file() {
            entry.kind = EntryKind::File;
            each(Item::Entry(entry))?;
            let mut file = File::open(&path)?;
            let mut buf = vec![0u8; hermit_proto::CHUNK];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes += n as u64;
                each(Item::Data(buf[..n].to_vec()))?;
            }
        } else {
            warn(format!("left out {}: not a file, folder or link", path.display()));
        }
    }
    Ok(bytes)
}

/// Writes a copy out of the container on this machine, inside `base` (as `name`, or into `base` itself).
pub struct Receiver {
    base: PathBuf,
    name: Option<String>,
    file: Option<File>,
    pub bytes: u64,
}

impl Receiver {
    /// Where a copy whose first entry is `first` lands at `dest`, as `docker cp` places it.
    pub fn new(dest: &str, first: &Entry, name: &str, contents: bool) -> io::Result<Receiver> {
        let path = Path::new(dest);
        let existing = match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => Existing::Dir,
            Ok(_) => Existing::File,
            Err(_) => Existing::Missing,
        };
        let slash = dest.ends_with('/') || dest.ends_with('\\');
        let target = copy_target(existing, slash, first.kind == EntryKind::Dir, contents, name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{dest}: {e}")))?;
        let (base, name) = match target {
            Target::Into(name) => (path.to_path_buf(), Some(name)),
            Target::At if existing == Existing::Dir => (path.to_path_buf(), None),
            Target::At => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("cannot copy to {dest}")))?;
                let parent = match path.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => PathBuf::from("."),
                };
                if !parent.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("the folder {} does not exist", parent.display()),
                    ));
                }
                (parent, Some(name.to_string()))
            }
        };
        Ok(Receiver {
            base,
            name,
            file: None,
            bytes: 0,
        })
    }

    /// Where an entry goes: under `base`, through real folders only.
    fn place(&self, entry: &Entry) -> io::Result<Option<PathBuf>> {
        if !is_entry_path(&entry.path) || entry.path.contains(':') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the container sent a path outside the copy: {:?}", entry.path),
            ));
        }
        let parts: Vec<&str> = self
            .name
            .iter()
            .map(String::as_str)
            .chain(entry.path.split('/').filter(|p| !p.is_empty()))
            .collect();
        let Some((last, parents)) = parts.split_last() else {
            // The root merging into an existing folder.
            return Ok(None);
        };
        let mut at = self.base.clone();
        for part in parents {
            at.push(part);
            let meta = std::fs::symlink_metadata(&at)?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a folder", at.display()),
                ));
            }
        }
        Ok(Some(at.join(last)))
    }

    pub fn put(&mut self, entry: &Entry, warn: &mut dyn FnMut(String)) -> io::Result<()> {
        self.file = None;
        let Some(path) = self.place(entry)? else {
            return match entry.kind {
                EntryKind::Dir => Ok(()),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a file cannot replace a folder",
                )),
            };
        };
        let existing = std::fs::symlink_metadata(&path).ok();
        let is_dir = existing.as_ref().is_some_and(std::fs::Metadata::is_dir);
        // Never through a link: what is there and is not a folder is replaced, as `docker cp` overwrites.
        let clear = |path: &Path| -> io::Result<()> {
            match &existing {
                Some(_) if is_dir => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is a folder", path.display()),
                )),
                Some(_) => std::fs::remove_file(path),
                None => Ok(()),
            }
        };
        match entry.kind {
            EntryKind::Dir => {
                if !is_dir {
                    clear(&path)?;
                    std::fs::create_dir(&path)?;
                }
                set_mode(&path, entry.mode)?;
            }
            EntryKind::File => {
                clear(&path)?;
                let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                set_mode(&path, entry.mode)?;
                self.file = Some(file);
            }
            EntryKind::Symlink => {
                let target = entry.target.as_deref().unwrap_or_default();
                if !stays_inside(&entry.path, target) || self.name.is_none() && entry.path.is_empty() {
                    warn(format!("left out {}: a link to outside what is copied", path.display()));
                    return Ok(());
                }
                clear(&path)?;
                link(target, &path, warn)?;
            }
            EntryKind::Other => warn(format!("left out {}: not a file, folder or link", path.display())),
        }
        Ok(())
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.file {
            Some(file) => {
                self.bytes += data.len() as u64;
                file.write_all(data)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidData, "file data without a file")),
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    if mode & 0o200 == 0 && !path.is_dir() {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
fn link(target: &str, path: &Path, _warn: &mut dyn FnMut(String)) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// Windows lets only some users make links, so a link is left out, with a warning.
#[cfg(not(unix))]
fn link(_target: &str, path: &Path, warn: &mut dyn FnMut(String)) -> io::Result<()> {
    warn(format!("left out {}: links are not copied to Windows", path.display()));
    Ok(())
}

/// `cp SOURCE DEST`: one side a running container's `CONTAINER:PATH`, the other a path on this machine.
pub fn cp(
    containers: &Containers,
    data_dir: &Path,
    source: &str,
    dest: &str,
    home: Option<&Path>,
    err: &mut impl Write,
) -> Result<(), String> {
    let (container, path, host, out_of) = match (location(source), location(dest)) {
        (Location::Container { container, path }, Location::Host(host)) => (container, path, host, true),
        (Location::Host(host), Location::Container { container, path }) => (container, path, host, false),
        (Location::Container { .. }, Location::Container { .. }) => {
            return Err("copying between containers is not supported".into());
        }
        (Location::Host(_), Location::Host(_)) => {
            return Err("one side must be a container, as CONTAINER:PATH".into());
        }
    };
    if host == "-" {
        return Err("copying to or from a tar stream (-) is not supported yet".into());
    }
    if !path.starts_with('/') {
        return Err(format!("{path}: a path in a container must be absolute"));
    }
    if let Some(reason) = refused(Path::new(&host), home) {
        return Err(reason);
    }
    let listed = containers.find(&container).map_err(|e| e.to_string())?;
    let name = &listed.container.name;
    if !listed.running {
        return Err(format!(
            "container {name} is not running: a stopped container's files are gone with its sandbox"
        ));
    }
    let mut conn = crate::ipc::connect(data_dir, &listed.container.id)
        .map_err(|e| format!("cannot reach the monitor of container {name}: {e}"))?;
    let mut warn = |text: String| {
        let _ = writeln!(err, "oci-hermit: {text}");
    };
    let copied = if out_of {
        receive(&mut conn, &path, &host, &mut warn)?
    } else {
        give(&mut conn, &host, &path, &mut warn)?
    };
    let to = if out_of { host } else { format!("{container}:{path}") };
    let _ = writeln!(err, "Successfully copied {} to {to}", crate::registry::size(copied));
    Ok(())
}

/// The end of a copy, from the monitor: `Ok` once the agent says it is complete.
fn ended(notice: Option<Notice>) -> Option<Result<(), String>> {
    match notice? {
        Notice::Exited { code: 0 } => Some(Ok(())),
        Notice::Exited { code } => Some(Err(format!("the copy ended with {code}"))),
        Notice::Failed { message } => Some(Err(message)),
        _ => None,
    }
}

fn receive(conn: &mut crate::ipc::Conn, path: &str, host: &str, warn: &mut dyn FnMut(String)) -> Result<u64, String> {
    monitor::send(conn.writer.as_mut(), &Request::CopyOut { path: path.to_string() }).map_err(|e| e.to_string())?;
    let (name, contents) = name_and_contents(path);
    let mut receiver: Option<Receiver> = None;
    loop {
        let notice = monitor::read_one::<Notice>(conn.reader.as_mut());
        match notice {
            Some(Notice::Entry { entry }) => {
                let receiver = match &mut receiver {
                    Some(r) => r,
                    None => receiver.insert(Receiver::new(host, &entry, &name, contents).map_err(|e| e.to_string())?),
                };
                receiver.put(&entry, warn).map_err(|e| e.to_string())?;
            }
            Some(Notice::EntryData { data }) => {
                let bytes = hermit_proto::b64::decode(&data).ok_or("the monitor sent data that is not base64")?;
                receiver
                    .as_mut()
                    .ok_or("file data before any entry")?
                    .write(&bytes)
                    .map_err(|e| e.to_string())?;
            }
            None => return Err("the container's monitor went away during the copy".into()),
            other => match ended(other) {
                Some(Ok(())) => return Ok(receiver.map_or(0, |r| r.bytes)),
                Some(Err(e)) => return Err(format!("{path}: {e}")),
                None => {}
            },
        }
    }
}

fn give(conn: &mut crate::ipc::Conn, host: &str, path: &str, warn: &mut dyn FnMut(String)) -> Result<u64, String> {
    let source = Path::new(host);
    let meta = std::fs::symlink_metadata(source).map_err(|e| format!("{host}: {e}"))?;
    let (name, contents) = name_and_contents(host);
    let into = CopyInto {
        path: path.to_string(),
        name,
        dir: meta.is_dir(),
        contents,
    };
    let writer = &mut conn.writer;
    monitor::send(writer.as_mut(), &Request::CopyIn { into }).map_err(|e| e.to_string())?;
    let mut send = |item: Item| {
        let request = match item {
            Item::Entry(entry) => Request::Put { entry },
            Item::Data(bytes) => Request::PutData {
                data: hermit_proto::b64::encode(&bytes),
            },
        };
        monitor::send(writer.as_mut(), &request)
    };
    let bytes = read_tree(source, &mut send, warn).map_err(|e| format!("{host}: {e}"))?;
    monitor::send(writer.as_mut(), &Request::PutDone).map_err(|e| e.to_string())?;
    loop {
        match ended(monitor::read_one::<Notice>(conn.reader.as_mut())) {
            Some(Ok(())) => return Ok(bytes),
            Some(Err(e)) => return Err(format!("{path}: {e}")),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    #[test]
    fn arguments_are_read_as_docker_cp_reads_them() {
        let container = |c: &str, p: &str| Location::Container {
            container: c.into(),
            path: p.into(),
        };
        assert_eq!(location("web:/etc/hosts"), container("web", "/etc/hosts"));
        assert_eq!(location("4f2a:/"), container("4f2a", "/"));
        for local in ["./web:/x", "notes.txt", "C:\\data\\x", "c:/data", ":x", "-"] {
            assert_eq!(location(local), Location::Host(local.into()), "{local}");
        }
        assert_eq!(location("/tmp/a:b"), Location::Host("/tmp/a:b".into()));
        assert_eq!(name_and_contents("/srv/site"), ("site".into(), false));
        assert_eq!(name_and_contents("/srv/site/"), ("site".into(), false));
        assert_eq!(name_and_contents("/srv/site/."), ("site".into(), true));
        assert_eq!(name_and_contents("dir\\sub\\."), ("sub".into(), true));
        assert_eq!(name_and_contents("/"), (String::new(), false));
    }

    #[test]
    fn the_bind_mount_deny_list_applies() {
        let home = scratch("cp-home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("work")).unwrap();
        let home = home.as_path();
        assert!(refused(Path::new("/"), Some(home)).unwrap().contains("root"));
        assert!(refused(home, Some(home)).unwrap().contains("holds the home folder"));
        assert!(
            refused(home.parent().unwrap(), Some(home))
                .unwrap()
                .contains("holds the home folder")
        );
        assert!(
            refused(&home.join(".ssh/id_ed25519"), Some(home))
                .unwrap()
                .contains(".ssh")
        );
        assert!(refused(&home.join(".ssh"), Some(home)).is_some());
        // Through a link, a credentials folder is still refused.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(home.join(".ssh"), home.join("work/keys")).unwrap();
            assert!(refused(&home.join("work/keys/x"), Some(home)).is_some());
        }
        assert_eq!(refused(&home.join("work"), Some(home)), None);
        assert_eq!(
            refused(&home.join("work/new.txt"), Some(home)),
            None,
            "a path not made yet"
        );
        assert_eq!(refused(&home.join(".sshkeys"), Some(home)), None);
    }

    #[test]
    fn links_stay_inside_the_copy() {
        assert!(stays_inside("a/link", "b"));
        assert!(stays_inside("a/link", "../b"));
        assert!(stays_inside("a/b/link", "../../c"));
        assert!(!stays_inside("a/link", "../../b"));
        assert!(!stays_inside("link", "../b"));
        assert!(!stays_inside("a/link", "/etc/passwd"));
        assert!(!stays_inside("a/link", "C:\\x"));
        assert!(stays_inside("a/link", "./b/../c"));
    }

    fn entry(path: &str, kind: EntryKind, mode: u32) -> Entry {
        Entry {
            path: path.into(),
            kind,
            mode,
            target: None,
        }
    }

    #[test]
    fn a_tree_is_read_and_written_back_the_same() {
        let dir = scratch("cp-tree");
        let source = dir.join("site");
        std::fs::create_dir_all(source.join("css")).unwrap();
        std::fs::write(source.join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::write(source.join("css/a.css"), vec![b'x'; hermit_proto::CHUNK + 5]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(source.join("index.html"), std::fs::Permissions::from_mode(0o4755)).unwrap();
            std::os::unix::fs::symlink("css/a.css", source.join("style")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", source.join("out")).unwrap();
            std::os::unix::net::UnixListener::bind(source.join("sock")).unwrap();
        }
        let mut items = Vec::new();
        let mut warnings = Vec::new();
        let bytes = read_tree(
            &source,
            &mut |item| {
                items.push(item);
                Ok(())
            },
            &mut |w| warnings.push(w),
        )
        .unwrap();
        assert_eq!(bytes, 11 + hermit_proto::CHUNK as u64 + 5);
        let entries: Vec<(String, EntryKind)> = items
            .iter()
            .filter_map(|i| match i {
                Item::Entry(e) => Some((e.path.clone(), e.kind)),
                Item::Data(_) => None,
            })
            .collect();
        let mut expected = vec![
            (String::new(), EntryKind::Dir),
            ("css".into(), EntryKind::Dir),
            ("css/a.css".into(), EntryKind::File),
            ("index.html".into(), EntryKind::File),
        ];
        if cfg!(unix) {
            expected.push(("style".into(), EntryKind::Symlink));
            assert_eq!(warnings.len(), 2, "the outward link and the socket: {warnings:?}");
        }
        assert_eq!(entries, expected);

        // Written into a folder that exists, it keeps its name; setuid bits are gone.
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let first = match &items[0] {
            Item::Entry(e) => e.clone(),
            Item::Data(_) => unreachable!(),
        };
        let mut receiver = Receiver::new(dest.to_str().unwrap(), &first, "site", false).unwrap();
        for item in &items {
            match item {
                Item::Entry(e) => receiver.put(e, &mut |w| warnings.push(w)).unwrap(),
                Item::Data(d) => receiver.write(d).unwrap(),
            }
        }
        drop(receiver);
        assert_eq!(std::fs::read(dest.join("site/index.html")).unwrap(), b"<h1>hi</h1>");
        assert_eq!(
            std::fs::read(dest.join("site/css/a.css")).unwrap().len(),
            hermit_proto::CHUNK + 5
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest.join("site/index.html"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7777, 0o755);
            assert_eq!(
                std::fs::read_link(dest.join("site/style")).unwrap(),
                Path::new("css/a.css")
            );
        }
    }

    #[test]
    fn what_the_container_sends_cannot_leave_the_copy() {
        let dir = scratch("cp-hostile");
        let dest = dir.join("out");
        let root = entry("", EntryKind::Dir, 0o755);
        let mut receiver = Receiver::new(dest.to_str().unwrap(), &root, "x", false).unwrap();
        let mut warnings = Vec::new();
        receiver.put(&root, &mut |w| warnings.push(w)).unwrap();
        for bad in ["../escape", "/etc/passwd", "a/../../b", "C:", "a//b"] {
            assert!(
                receiver.put(&entry(bad, EntryKind::File, 0o644), &mut |_| {}).is_err(),
                "{bad}"
            );
        }
        assert!(!dir.join("escape").exists());
        // A link that points out is left out, and nothing is written through one.
        let outward = Entry {
            target: Some("../../secret".into()),
            ..entry("link", EntryKind::Symlink, 0o777)
        };
        receiver.put(&outward, &mut |w| warnings.push(w)).unwrap();
        assert!(std::fs::symlink_metadata(dest.join("link")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("victim"), dest.join("trap")).unwrap();
            std::fs::write(dir.join("victim"), b"keep").unwrap();
            receiver
                .put(&entry("trap", EntryKind::File, 0o644), &mut |_| {})
                .unwrap();
            receiver.write(b"new").unwrap();
            assert_eq!(
                std::fs::read(dir.join("victim")).unwrap(),
                b"keep",
                "the link was replaced, not followed"
            );
            std::os::unix::fs::symlink(dir.join("elsewhere"), dest.join("via")).unwrap();
            std::fs::create_dir(dir.join("elsewhere")).unwrap();
            assert!(
                receiver
                    .put(&entry("via/file", EntryKind::File, 0o644), &mut |_| {})
                    .is_err()
            );
            assert!(!dir.join("elsewhere/file").exists());
        }
        assert!(
            receiver
                .put(&entry("dev", EntryKind::Other, 0o600), &mut |w| warnings.push(w))
                .is_ok()
        );
        assert!(
            warnings.iter().any(|w| w.contains("not a file, folder or link")),
            "{warnings:?}"
        );
        assert!(
            receiver.write(b"x").is_err(),
            "no file is open after an entry that made none"
        );
    }

    #[test]
    fn copies_land_as_docker_cp_places_them_here() {
        let dir = scratch("cp-place");
        let file = entry("", EntryKind::File, 0o644);
        let folder = entry("", EntryKind::Dir, 0o755);
        let put = |dest: &Path, first: &Entry, contents: bool| {
            let mut r = Receiver::new(dest.to_str().unwrap(), first, "thing", contents)?;
            r.put(first, &mut |_| {})?;
            if first.kind == EntryKind::File {
                r.write(b"data")?;
            }
            Ok::<_, io::Error>(())
        };
        put(&dir.join("new.txt"), &file, false).unwrap();
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"data");
        put(&dir, &file, false).unwrap();
        assert!(dir.join("thing").is_file());
        put(&dir.join("made"), &folder, false).unwrap();
        assert!(dir.join("made").is_dir());
        assert!(
            put(&dir.join("new.txt"), &folder, false).is_err(),
            "a folder onto a file"
        );
        assert!(
            put(&dir.join("missing/x"), &file, false).is_err(),
            "its folder must exist"
        );
        let merged = dir.join("merged");
        std::fs::create_dir(&merged).unwrap();
        put(&merged, &folder, true).unwrap();
        assert!(!merged.join("thing").exists(), "src/. merges into the folder");
    }
}
