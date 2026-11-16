//! The guest's initramfs: the agent as `/init`, the console device it logs to, and, for `run`, the image's files
//! under `/rootfs`.
//!
//! Written as a `newc` cpio archive in memory. Device nodes and ownership are fields in the archive, so building it
//! needs no privileges on the host.

use crate::rootfs::{Node, Rootfs};

/// `newc` file type bits.
const DIR: u32 = 0o040_000;
const FILE: u32 = 0o100_000;
const SYMLINK: u32 = 0o120_000;
const CHAR_DEVICE: u32 = 0o020_000;
const FIFO: u32 = 0o010_000;

/// `/dev/console` is character device 5:1 on Linux.
const CONSOLE: (u32, u32) = (5, 1);

/// Where `run` puts the image's root filesystem in the initramfs.
pub const ROOTFS: &str = "rootfs";

/// Owner, permission bits and modification time of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
}

impl Meta {
    /// Owned by root, with `mode` and no time.
    pub const fn root(mode: u32) -> Meta {
        Meta {
            mode,
            uid: 0,
            gid: 0,
            mtime: 0,
        }
    }
}

/// A `newc` archive being written.
#[derive(Debug)]
pub struct Archive {
    buf: Vec<u8>,
    next_ino: u32,
}

fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

impl Archive {
    pub fn new() -> Archive {
        Archive {
            buf: Vec::new(),
            next_ino: 1,
        }
    }

    fn record(&mut self, name: &str, kind: u32, meta: Meta, rdev: (u32, u32), data: &[u8]) {
        let ino = self.next_ino;
        self.next_ino += 1;
        let name_size = u32::try_from(name.len() + 1).expect("entry name fits in u32");
        let file_size = u32::try_from(data.len()).expect("entry data fits in u32");
        // magic, then ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor,
        // namesize, check: thirteen 8-digit hex fields.
        let mode = kind | (meta.mode & 0o7777);
        let fields = [
            ino, mode, meta.uid, meta.gid, 1, meta.mtime, file_size, 0, 0, rdev.0, rdev.1, name_size, 0,
        ];
        self.buf.extend_from_slice(b"070701");
        for field in fields {
            self.buf.extend_from_slice(format!("{field:08x}").as_bytes());
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        pad4(&mut self.buf);
        self.buf.extend_from_slice(data);
        pad4(&mut self.buf);
    }

    pub fn dir(&mut self, path: &str, meta: Meta) {
        self.record(path, DIR, meta, (0, 0), &[]);
    }

    pub fn file(&mut self, path: &str, meta: Meta, data: &[u8]) {
        self.record(path, FILE, meta, (0, 0), data);
    }

    pub fn symlink(&mut self, path: &str, meta: Meta, target: &str) {
        self.record(path, SYMLINK, Meta { mode: 0o777, ..meta }, (0, 0), target.as_bytes());
    }

    pub fn fifo(&mut self, path: &str, meta: Meta) {
        self.record(path, FIFO, meta, (0, 0), &[]);
    }

    pub fn char_device(&mut self, path: &str, meta: Meta, rdev: (u32, u32)) {
        self.record(path, CHAR_DEVICE, meta, rdev, &[]);
    }

    /// The archive, with its trailer.
    pub fn finish(mut self) -> Vec<u8> {
        self.next_ino = 0;
        self.record("TRAILER!!!", 0, Meta::root(0), (0, 0), &[]);
        self.buf
    }
}

fn agent_entries(archive: &mut Archive, agent: &[u8]) {
    archive.dir("dev", Meta::root(0o755));
    archive.char_device("dev/console", Meta::root(0o600), CONSOLE);
    archive.file("init", Meta::root(0o755), agent);
}

/// An initramfs that boots `agent` as PID 1, with `/dev/console` so its output reaches the serial console.
pub fn build(agent: &[u8]) -> Vec<u8> {
    let mut archive = Archive::new();
    agent_entries(&mut archive, agent);
    archive.finish()
}

/// [`build`], plus the image's files under [`ROOTFS`].
pub fn build_with_rootfs(agent: &[u8], rootfs: &Rootfs) -> Vec<u8> {
    let mut archive = Archive::new();
    agent_entries(&mut archive, agent);
    archive.dir(ROOTFS, Meta::root(0o755));
    for (path, node) in rootfs.nodes() {
        let name = format!("{ROOTFS}/{path}");
        match node {
            Node::Dir(meta) => archive.dir(&name, *meta),
            Node::File(meta, data) => archive.file(&name, *meta, data),
            Node::Symlink(meta, target) => archive.symlink(&name, *meta, target),
            Node::Fifo(meta) => archive.fifo(&name, *meta),
        }
    }
    archive.finish()
}

#[cfg(test)]
pub(crate) mod testing {
    /// One entry read back from an archive.
    #[derive(Debug, PartialEq)]
    pub(crate) struct Entry {
        pub name: String,
        pub mode: u32,
        pub uid: u32,
        pub rdev: (u32, u32),
        pub data: Vec<u8>,
    }

    fn hex(bytes: &[u8]) -> u32 {
        u32::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap()
    }

    /// Every entry of a `newc` archive, checking its structure on the way.
    pub(crate) fn parse(mut archive: &[u8]) -> Vec<Entry> {
        let mut entries = Vec::new();
        loop {
            assert_eq!(&archive[..6], b"070701", "magic");
            let field = |i: usize| hex(&archive[6 + 8 * i..6 + 8 * (i + 1)]);
            let (mode, uid, size, rdev, name_size) = (
                field(1),
                field(2),
                field(6) as usize,
                (field(9), field(10)),
                field(11) as usize,
            );
            let header = 6 + 13 * 8;
            let name = String::from_utf8(archive[header..header + name_size - 1].to_vec()).unwrap();
            let data_start = (header + name_size).next_multiple_of(4);
            let data = archive[data_start..data_start + size].to_vec();
            let next = (data_start + size).next_multiple_of(4);
            archive = &archive[next..];
            if name == "TRAILER!!!" {
                assert!(archive.is_empty(), "nothing after the trailer");
                return entries;
            }
            entries.push(Entry {
                name,
                mode,
                uid,
                rdev,
                data,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Entry, parse};
    use super::*;

    #[test]
    fn holds_the_agent_as_init_and_a_console_node() {
        let archive = build(b"\x7fELF agent");
        assert_eq!(archive.len() % 4, 0);
        let entries = parse(&archive);
        assert_eq!(
            entries,
            [
                Entry {
                    name: "dev".into(),
                    mode: DIR | 0o755,
                    uid: 0,
                    rdev: (0, 0),
                    data: vec![]
                },
                Entry {
                    name: "dev/console".into(),
                    mode: CHAR_DEVICE | 0o600,
                    uid: 0,
                    rdev: (5, 1),
                    data: vec![]
                },
                Entry {
                    name: "init".into(),
                    mode: FILE | 0o755,
                    uid: 0,
                    rdev: (0, 0),
                    data: b"\x7fELF agent".to_vec()
                },
            ]
        );
    }

    #[test]
    fn pads_every_data_length() {
        for len in 0..9 {
            let agent = vec![0xab; len];
            let entries = parse(&build(&agent));
            assert_eq!(entries[2].data, agent);
        }
    }

    #[test]
    fn writes_every_kind_of_entry_with_its_owner() {
        let mut archive = Archive::new();
        let owned = Meta {
            mode: 0o4755,
            uid: 1000,
            gid: 100,
            mtime: 1_700_000_000,
        };
        archive.dir("d", owned);
        archive.file("d/f", owned, b"data");
        archive.symlink("d/l", owned, "f");
        archive.fifo("d/p", owned);
        let entries = parse(&archive.finish());
        let modes: Vec<_> = entries.iter().map(|e| e.mode).collect();
        assert_eq!(modes, [DIR | 0o4755, FILE | 0o4755, SYMLINK | 0o777, FIFO | 0o4755]);
        assert!(entries.iter().all(|e| e.uid == 1000));
        assert_eq!(entries[2].data, b"f");
    }
}
