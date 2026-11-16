//! An image's root filesystem, assembled in memory from its layers.
//!
//! Layers are applied in order, following the OCI layer rules: a `.wh.<name>` entry deletes `<name>` and everything
//! under it, and a `.wh..wh..opq` entry hides everything lower layers put in its directory. Hard links become
//! copies, and device nodes are skipped (the guest's devtmpfs provides `/dev`). Nothing is written to the host:
//! the result goes into the initramfs (`initramfs::build_with_rootfs`).
//!
//! Every path is checked: one that climbs out of the root with `..` is refused, so an image cannot reach the
//! agent's own files beside the root filesystem in the initramfs.

use crate::initramfs::Meta;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read};

const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// One entry of the root filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir(Meta),
    File(Meta, Vec<u8>),
    Symlink(Meta, String),
    Fifo(Meta),
}

/// Why a layer could not be applied.
#[derive(Debug)]
pub enum LayerError {
    Io(io::Error),
    /// The path is not a plain relative path inside the root.
    UnsafePath(String),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerError::Io(e) => write!(f, "reading a layer: {e}"),
            LayerError::UnsafePath(path) => write!(f, "layer entry {path:?} points outside the image root"),
        }
    }
}

impl std::error::Error for LayerError {}

impl From<io::Error> for LayerError {
    fn from(e: io::Error) -> Self {
        LayerError::Io(e)
    }
}

/// The root filesystem: paths relative to `/`, without a leading slash, in sorted order so parents come first.
#[derive(Debug, Default)]
pub struct Rootfs {
    nodes: BTreeMap<String, Node>,
    skipped_devices: usize,
}

/// `path` as a clean relative path: no leading `/` or `./`, no empty, `.` or `..` components.
fn clean(path: &str) -> Result<String, LayerError> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(LayerError::UnsafePath(path.to_string())),
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn parent(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

fn name(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

impl Rootfs {
    pub fn new() -> Rootfs {
        Rootfs::default()
    }

    /// Every entry, parents before children.
    pub fn nodes(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes.iter()
    }

    /// How many device nodes the layers held and were skipped.
    #[cfg(test)]
    pub fn skipped_devices(&self) -> usize {
        self.skipped_devices
    }

    /// The total size of the files' contents, to size the guest's memory.
    pub fn content_bytes(&self) -> u64 {
        self.nodes
            .values()
            .map(|node| match node {
                Node::File(_, data) => data.len() as u64,
                _ => 0,
            })
            .sum()
    }

    /// The entry at `path` (relative, no leading slash), without following symbolic links.
    pub fn get(&self, path: &str) -> Option<&Node> {
        self.nodes.get(path)
    }

    /// Remove `path` and everything under it.
    fn remove_tree(&mut self, path: &str) {
        self.nodes.remove(path);
        let prefix = format!("{path}/");
        let under: Vec<String> = self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .map(|(p, _)| p.clone())
            .collect();
        for p in under {
            self.nodes.remove(&p);
        }
    }

    /// Make sure every directory above `path` exists.
    fn ensure_parents(&mut self, path: &str) {
        let mut dir = parent(path);
        let mut missing = Vec::new();
        while let Some(d) = dir {
            match self.nodes.get(d) {
                Some(Node::Dir(_)) => break,
                _ => missing.push(d.to_string()),
            }
            dir = parent(d);
        }
        for d in missing {
            self.nodes.insert(d, Node::Dir(Meta::root(0o755)));
        }
    }

    fn insert(&mut self, path: String, node: Node) {
        // A non-directory replaces whatever was there; a directory over a directory keeps the children.
        match (&node, self.nodes.get(&path)) {
            (Node::Dir(_), Some(Node::Dir(_))) => {}
            _ => self.remove_tree(&path),
        }
        self.ensure_parents(&path);
        self.nodes.insert(path, node);
    }

    /// Apply one uncompressed layer tarball on top of what is already here.
    pub fn apply_layer(&mut self, layer: impl Read) -> Result<(), LayerError> {
        let mut archive = tar::Archive::new(layer);
        // Paths this layer itself added: an opaque marker hides only what lower layers put in the directory.
        let mut added: BTreeSet<String> = BTreeSet::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let path = clean(&raw)?;
            if path.is_empty() {
                continue;
            }
            let base = name(&path);
            let dir = parent(&path).unwrap_or("");
            if base == OPAQUE {
                let prefix = join(dir, "");
                let hidden: Vec<String> = self
                    .nodes
                    .keys()
                    .filter(|p| (dir.is_empty() || p.starts_with(&prefix)) && !added.contains(*p) && *p != dir)
                    .cloned()
                    .collect();
                for p in hidden {
                    self.nodes.remove(&p);
                }
                continue;
            }
            if let Some(hidden) = base.strip_prefix(WHITEOUT) {
                self.remove_tree(&join(dir, &clean(hidden)?));
                continue;
            }
            let header = entry.header();
            let meta = Meta {
                mode: header.mode()? & 0o7777,
                uid: u32::try_from(header.uid()?).unwrap_or(0),
                gid: u32::try_from(header.gid()?).unwrap_or(0),
                mtime: u32::try_from(header.mtime()?).unwrap_or(0),
            };
            let node = match header.entry_type() {
                tar::EntryType::Directory => Node::Dir(meta),
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let mut data = Vec::with_capacity(usize::try_from(header.size()?).unwrap_or(0));
                    entry.read_to_end(&mut data)?;
                    Node::File(meta, data)
                }
                tar::EntryType::Symlink => {
                    let target = entry
                        .link_name_bytes()
                        .map(|t| String::from_utf8_lossy(&t).into_owned());
                    Node::Symlink(meta, target.unwrap_or_default())
                }
                tar::EntryType::Link => {
                    let target = entry
                        .link_name_bytes()
                        .map(|t| String::from_utf8_lossy(&t).into_owned());
                    let target = clean(&target.unwrap_or_default())?;
                    match self.nodes.get(&target) {
                        Some(Node::File(_, data)) => Node::File(meta, data.clone()),
                        _ => continue,
                    }
                }
                tar::EntryType::Fifo => Node::Fifo(meta),
                tar::EntryType::Char | tar::EntryType::Block => {
                    self.skipped_devices += 1;
                    continue;
                }
                _ => continue,
            };
            added.insert(path.clone());
            self.insert(path, node);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    /// A layer tarball built in memory.
    pub(crate) struct Layer(tar::Builder<Vec<u8>>);

    impl Layer {
        pub(crate) fn new() -> Layer {
            Layer(tar::Builder::new(Vec::new()))
        }

        fn header(path: &str, kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_size(size);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            header
        }

        pub(crate) fn dir(mut self, path: &str) -> Layer {
            let header = Layer::header(path, tar::EntryType::Directory, 0o755, 0);
            self.0.append(&header, &[][..]).unwrap();
            self
        }

        pub(crate) fn file(mut self, path: &str, data: &[u8]) -> Layer {
            let header = Layer::header(path, tar::EntryType::Regular, 0o644, data.len() as u64);
            self.0.append(&header, data).unwrap();
            self
        }

        pub(crate) fn link(mut self, path: &str, kind: tar::EntryType, target: &str) -> Layer {
            let mut header = Layer::header(path, kind, 0o777, 0);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            self.0.append(&header, &[][..]).unwrap();
            self
        }

        pub(crate) fn device(mut self, path: &str) -> Layer {
            let header = Layer::header(path, tar::EntryType::Char, 0o666, 0);
            self.0.append(&header, &[][..]).unwrap();
            self
        }

        /// A raw entry whose name the builder would refuse, such as one with `..`.
        pub(crate) fn raw_file(mut self, path: &str) -> Layer {
            let mut header = tar::Header::new_old();
            header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(0);
            header.set_cksum();
            self.0.append(&header, &[][..]).unwrap();
            self
        }

        pub(crate) fn bytes(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Layer;
    use super::*;

    fn files(rootfs: &Rootfs) -> Vec<String> {
        rootfs.nodes().map(|(p, _)| p.clone()).collect()
    }

    #[test]
    fn layers_stack_and_parents_are_created() {
        let mut root = Rootfs::new();
        root.apply_layer(&Layer::new().dir("etc").file("etc/hostname", b"a").bytes()[..])
            .unwrap();
        root.apply_layer(
            &Layer::new()
                .file("./usr/bin/tool", b"bin")
                .file("etc/hostname", b"b")
                .bytes()[..],
        )
        .unwrap();
        assert_eq!(files(&root), ["etc", "etc/hostname", "usr", "usr/bin", "usr/bin/tool"]);
        assert!(matches!(root.get("etc/hostname"), Some(Node::File(_, data)) if data == b"b"));
        assert_eq!(root.content_bytes(), 4);
    }

    #[test]
    fn whiteouts_delete_a_path_and_everything_under_it() {
        let mut root = Rootfs::new();
        root.apply_layer(&Layer::new().file("a/b/c", b"1").file("a/keep", b"2").bytes()[..])
            .unwrap();
        root.apply_layer(&Layer::new().file("a/.wh.b", b"").bytes()[..])
            .unwrap();
        assert_eq!(files(&root), ["a", "a/keep"]);
    }

    #[test]
    fn an_opaque_directory_hides_only_lower_layers() {
        let mut root = Rootfs::new();
        root.apply_layer(&Layer::new().file("d/old", b"1").file("other", b"2").bytes()[..])
            .unwrap();
        root.apply_layer(&Layer::new().file("d/.wh..wh..opq", b"").file("d/new", b"3").bytes()[..])
            .unwrap();
        assert_eq!(files(&root), ["d", "d/new", "other"]);
    }

    #[test]
    fn a_file_replaces_a_directory_tree() {
        let mut root = Rootfs::new();
        root.apply_layer(&Layer::new().file("x/y", b"1").bytes()[..]).unwrap();
        root.apply_layer(&Layer::new().file("x", b"now a file").bytes()[..])
            .unwrap();
        assert_eq!(files(&root), ["x"]);
    }

    #[test]
    fn links_devices_and_symlinks() {
        let mut root = Rootfs::new();
        let layer = Layer::new()
            .file("bin/busybox", b"elf")
            .link("bin/sh", tar::EntryType::Link, "bin/busybox")
            .link("bin/ls", tar::EntryType::Symlink, "busybox")
            .link("bin/dangling", tar::EntryType::Link, "nowhere")
            .device("dev/null");
        root.apply_layer(&layer.bytes()[..]).unwrap();
        assert!(matches!(root.get("bin/sh"), Some(Node::File(_, data)) if data == b"elf"));
        assert!(matches!(root.get("bin/ls"), Some(Node::Symlink(_, target)) if target == "busybox"));
        assert!(root.get("bin/dangling").is_none());
        assert!(root.get("dev/null").is_none());
        assert_eq!(root.skipped_devices(), 1);
    }

    #[test]
    fn refuses_paths_that_climb_out_of_the_root() {
        for path in ["../init", "a/../../init"] {
            let mut root = Rootfs::new();
            let err = root.apply_layer(&Layer::new().raw_file(path).bytes()[..]).unwrap_err();
            assert!(matches!(err, LayerError::UnsafePath(_)), "{path}: {err}");
            assert!(err.to_string().contains("outside the image root"));
        }
        let mut root = Rootfs::new();
        let err = root
            .apply_layer(&Layer::new().file("d/.wh...", b"").bytes()[..])
            .unwrap_err();
        assert!(matches!(err, LayerError::UnsafePath(_)));
    }

    #[test]
    fn reports_a_broken_tarball() {
        let mut root = Rootfs::new();
        let err = root.apply_layer(&b"definitely not a tar archive"[..]);
        assert!(err.is_ok() || matches!(err, Err(LayerError::Io(_))));
        let truncated = &Layer::new().file("f", &[1; 4096]).bytes()[..700];
        assert!(matches!(Rootfs::new().apply_layer(truncated), Err(LayerError::Io(_))));
    }
}
