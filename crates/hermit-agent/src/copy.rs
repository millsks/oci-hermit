//! Copies into and out of the container's files (`cp`, protocol 1.8), always inside its root: a path resolves as it
//! does in the container (`openat2` with `RESOLVE_IN_ROOT`), and a copy's own entries are read and made from the
//! folder above them with `O_NOFOLLOW`, so no link, `..` or absolute path takes a copy outside.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use hermit_proto::{CHUNK, Entry, EntryKind, Existing, Target, copy_target};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, OpenHow, ResolveFlag, open, openat, openat2, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, symlinkat, unlinkat};

use crate::init::{Copied, CopyOut};

/// The permission bits a copy keeps: never setuid, setgid or sticky.
const MODE_BITS: u32 = 0o777;

fn open_root(root: &str) -> io::Result<OwnedFd> {
    Ok(open(
        root,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?)
}

/// `path` as the container sees it, from its root.
fn resolve(root: &OwnedFd, path: &str, flags: OFlag) -> nix::Result<OwnedFd> {
    let how = OpenHow::new()
        .flags(flags | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_IN_ROOT);
    openat2(root, path, how)
}

fn kind_of(stat: &FileStat) -> EntryKind {
    match SFlag::from_bits_truncate(stat.st_mode & SFlag::S_IFMT.bits()) {
        SFlag::S_IFDIR => EntryKind::Dir,
        SFlag::S_IFREG => EntryKind::File,
        SFlag::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

/// What an `O_PATH` descriptor names, opened again for reading.
fn reopen(fd: &OwnedFd, flags: OFlag) -> io::Result<OwnedFd> {
    Ok(open(
        format!("/proc/self/fd/{}", fd.as_raw_fd()).as_str(),
        flags | OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?)
}

fn utf8(name: OsString) -> io::Result<String> {
    name.into_string()
        .map_err(|name| io::Error::new(ErrorKind::InvalidData, format!("a name that is not UTF-8: {name:?}")))
}

/// The names in a folder, but `.` and `..`.
fn names(dir: &OwnedFd) -> io::Result<std::vec::IntoIter<OsString>> {
    let mut listing = Dir::from_fd(dir.try_clone()?)?;
    let mut names = Vec::new();
    for entry in listing.iter() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from(std::ffi::OsStr::from_bytes(&name)));
        }
    }
    names.sort();
    Ok(names.into_iter())
}

/// A folder being sent: its descriptor, its path in the copy, and the names left to send.
struct Listing {
    dir: OwnedFd,
    path: String,
    names: std::vec::IntoIter<OsString>,
}

/// A copy out, read as it is sent: an entry, then a file's bytes, then the next entry.
struct Walk {
    first: Option<Entry>,
    reading: Option<File>,
    folders: Vec<Listing>,
}

impl Walk {
    fn next_item(&mut self) -> io::Result<Option<Copied>> {
        // What was named comes first, then its bytes if it is a file.
        if let Some(entry) = self.first.take() {
            return Ok(Some(Copied::Entry(entry)));
        }
        if let Some(file) = &mut self.reading {
            let mut buf = vec![0u8; CHUNK];
            let n = file.read(&mut buf)?;
            if n > 0 {
                buf.truncate(n);
                return Ok(Some(Copied::Data(buf)));
            }
            self.reading = None;
        }
        loop {
            let Some(folder) = self.folders.last_mut() else {
                return Ok(None);
            };
            let Some(name) = folder.names.next() else {
                self.folders.pop();
                continue;
            };
            let stat = fstatat(&folder.dir, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
            let name = utf8(name)?;
            let path = if folder.path.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", folder.path)
            };
            let kind = kind_of(&stat);
            let mut target = None;
            match kind {
                EntryKind::Dir => {
                    let dir = openat(
                        &folder.dir,
                        name.as_str(),
                        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )?;
                    let names = names(&dir)?;
                    self.folders.push(Listing {
                        dir,
                        path: path.clone(),
                        names,
                    });
                }
                EntryKind::File => {
                    let file = openat(
                        &folder.dir,
                        name.as_str(),
                        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )?;
                    self.reading = Some(File::from(file));
                }
                EntryKind::Symlink => target = Some(utf8(readlinkat(&folder.dir, name.as_str())?)?),
                EntryKind::Other => {}
            }
            return Ok(Some(Copied::Entry(Entry {
                path,
                kind,
                mode: stat.st_mode & MODE_BITS,
                target,
            })));
        }
    }
}

impl Iterator for Walk {
    type Item = io::Result<Copied>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_item() {
            Ok(item) => item.map(Ok),
            Err(e) => {
                // Nothing more after an error.
                self.folders.clear();
                self.reading = None;
                Some(Err(e))
            }
        }
    }
}

/// What `path` names under `root`: a link there is sent as a link, as `docker cp` does without `-L`.
pub fn copy_out(root: &str, path: &str) -> io::Result<CopyOut> {
    let root = open_root(root)?;
    let found = resolve(&root, path, OFlag::O_PATH | OFlag::O_NOFOLLOW)?;
    let stat = fstat(&found)?;
    let kind = kind_of(&stat);
    let mut walk = Walk {
        first: None,
        reading: None,
        folders: Vec::new(),
    };
    let mut target = None;
    match kind {
        EntryKind::Dir => {
            let dir = reopen(&found, OFlag::O_DIRECTORY)?;
            let names = names(&dir)?;
            walk.folders.push(Listing {
                dir,
                path: String::new(),
                names,
            });
        }
        EntryKind::File => walk.reading = Some(File::from(reopen(&found, OFlag::empty())?)),
        EntryKind::Symlink => target = Some(utf8(readlinkat(&found, "")?)?),
        EntryKind::Other => {}
    }
    walk.first = Some(Entry {
        path: String::new(),
        kind,
        mode: stat.st_mode & MODE_BITS,
        target,
    });
    Ok(Box::new(walk))
}

/// A copy into the container being received: the folder it lands in and, unless it merges into that folder, the
/// name it takes there; and the file being written.
pub struct Receiving {
    base: OwnedFd,
    name: Option<String>,
    file: Option<File>,
}

/// Gets ready to receive a copy of `name` (a folder when `dir`, only its contents when `contents`) at `path` under
/// `root`, placed as `docker cp` places it.
pub fn copy_in(root: &str, path: &str, name: &str, dir: bool, contents: bool) -> io::Result<Receiving> {
    let root = open_root(root)?;
    let dest = match resolve(&root, path, OFlag::O_PATH) {
        Ok(fd) => Some(fd),
        Err(Errno::ENOENT) => None,
        Err(e) => return Err(e.into()),
    };
    let existing = match &dest {
        None => Existing::Missing,
        Some(fd) if kind_of(&fstat(fd)?) == EntryKind::Dir => Existing::Dir,
        Some(_) => Existing::File,
    };
    let invalid = |e: String| io::Error::new(ErrorKind::InvalidInput, e);
    let target = copy_target(existing, path.ends_with('/'), dir, contents, name).map_err(invalid)?;
    let as_dir = |fd: &OwnedFd| reopen(fd, OFlag::O_DIRECTORY);
    let (base, name) = match (target, &dest) {
        (Target::Into(name), Some(fd)) => (as_dir(fd)?, Some(name)),
        (Target::At, Some(fd)) if existing == Existing::Dir => (as_dir(fd)?, None),
        (Target::At, _) => {
            let trimmed = path.trim_end_matches('/');
            let (parent, last) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
            if last.is_empty() || last == "." || last == ".." {
                return Err(invalid(format!("cannot copy to {path}")));
            }
            let parent = if parent.is_empty() { "/" } else { parent };
            let base = resolve(&root, parent, OFlag::O_RDONLY | OFlag::O_DIRECTORY).map_err(|e| match e {
                Errno::ENOENT | Errno::ENOTDIR => invalid(format!("the folder {parent} does not exist")),
                e => e.into(),
            })?;
            (base, Some(last.to_string()))
        }
        (Target::Into(_), None) => unreachable!("a folder to copy into exists"),
    };
    Ok(Receiving { base, name, file: None })
}

impl Receiving {
    /// Makes `entry`; a file's bytes follow through [`Receiving::write`].
    pub fn put(&mut self, entry: &Entry) -> io::Result<()> {
        self.file = None;
        let full = match (&self.name, entry.path.as_str()) {
            (Some(name), "") => name.clone(),
            (Some(name), path) => format!("{name}/{path}"),
            (None, "") if entry.kind == EntryKind::Dir => return Ok(()),
            (None, "") => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "a file cannot replace a folder",
                ));
            }
            (None, path) => path.to_string(),
        };
        let (parents, last) = full.rsplit_once('/').map_or(("", full.as_str()), |(p, l)| (p, l));
        // Down to the folder it goes in, never through a link.
        let mut folder: Option<OwnedFd> = None;
        for part in parents.split('/').filter(|p| !p.is_empty()) {
            let at = folder.as_ref().unwrap_or(&self.base);
            folder = Some(openat(
                at,
                part,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?);
        }
        let at = folder.as_ref().unwrap_or(&self.base);
        let mode = Mode::from_bits_truncate(entry.mode & MODE_BITS);
        let existing = match fstatat(at, last, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => Some(kind_of(&stat)),
            Err(Errno::ENOENT) => None,
            Err(e) => return Err(e.into()),
        };
        // What it replaces goes first, as `docker cp` overwrites; a folder is merged into, never replaced.
        let replace = |kind: EntryKind| -> io::Result<()> {
            match existing {
                Some(EntryKind::Dir) if kind != EntryKind::Dir => {
                    Err(io::Error::new(ErrorKind::AlreadyExists, format!("{full} is a folder")))
                }
                Some(EntryKind::Dir) | None => Ok(()),
                Some(_) if kind == EntryKind::File => {
                    // A regular file is truncated in place; anything else is removed first.
                    if existing == Some(EntryKind::File) {
                        Ok(())
                    } else {
                        Ok(unlinkat(at, last, UnlinkatFlags::NoRemoveDir)?)
                    }
                }
                Some(_) => Ok(unlinkat(at, last, UnlinkatFlags::NoRemoveDir)?),
            }
        };
        match entry.kind {
            EntryKind::Dir => {
                replace(EntryKind::Dir)?;
                if existing.is_none() {
                    mkdirat(at, last, mode)?;
                }
                let made = openat(
                    at,
                    last,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )?;
                fchmod(&made, mode)?;
            }
            EntryKind::File => {
                replace(EntryKind::File)?;
                let file = openat(
                    at,
                    last,
                    OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    mode,
                )?;
                fchmod(&file, mode)?;
                self.file = Some(File::from(file));
            }
            EntryKind::Symlink => {
                let target = entry
                    .target
                    .as_deref()
                    .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "a link without a target"))?;
                replace(EntryKind::Symlink)?;
                symlinkat(target, at, last)?;
            }
            EntryKind::Other => {}
        }
        Ok(())
    }

    /// Bytes for the file the last entry made.
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.write_all(data),
            None => Err(io::Error::new(ErrorKind::InvalidInput, "file data without a file")),
        }
    }
}
