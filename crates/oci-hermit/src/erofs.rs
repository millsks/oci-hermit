//! One image layer as a read-only EROFS filesystem image, written in user space without mounting anything (#16).
//!
//! The guest stacks these with overlayfs (#17), so a layer is written the way overlayfs reads a lower layer: an OCI
//! whiteout (`.wh.<name>`) becomes a character device 0:0 named `<name>`, and an opaque marker (`.wh..wh..opq`)
//! becomes the `trusted.overlay.opaque` xattr on its directory. The layer's own `trusted.*` xattrs are dropped, so
//! an image cannot give overlayfs instructions; `user.*` and `security.*` (file capabilities) are kept. Hard links
//! stay hard links. Device nodes are left out, as for the initramfs: the guest's devtmpfs provides `/dev`.
//!
//! The format is EROFS without compression, the parts every kernel since 5.4 reads: 4 KiB blocks, 64-byte inodes,
//! inline xattrs, and tail packing (`FLAT_INLINE`) for small files, directories and symlinks. File data is
//! streamed to the output as the tarball is read; only tails of at most [`INLINE_MAX`] bytes are kept in memory.
//! The layout is: superblock (block 0), file data, inodes (the metadata area), then directory blocks.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Bumped whenever the output changes, so cached disks from an older builder are not reused.
pub const FORMAT: u32 = 1;

const BLOCK: u64 = 4096;
const BLOCK_BITS: u8 = 12;
const SUPER_OFFSET: u64 = 1024;
const MAGIC: u32 = 0xE0F5_E1E2;
const INODE_SIZE: usize = 64;
const DIRENT_SIZE: usize = 12;
const NAME_MAX: usize = 255;
/// File tails up to this size go inline, after the inode; longer ones get a block of their own.
pub const INLINE_MAX: usize = 2048;

const LAYOUT_FLAT_PLAIN: u16 = 0;
const LAYOUT_FLAT_INLINE: u16 = 2;
const VERSION_EXTENDED: u16 = 1;

const XATTR_USER: u8 = 1;
const XATTR_TRUSTED: u8 = 4;
const XATTR_SECURITY: u8 = 6;

const S_IFIFO: u16 = 0o010000;
const S_IFCHR: u16 = 0o020000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHR: u8 = 3;
const FT_FIFO: u8 = 5;
const FT_LNK: u8 = 7;

const WHITEOUT: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";

/// Why a layer could not be written.
#[derive(Debug)]
pub enum BuildError {
    Io(io::Error),
    /// The layer is not a tarball we can take: a path climbing out with `..`, a name too long, and so on.
    Invalid(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Io(e) => write!(f, "writing the layer disk: {e}"),
            BuildError::Invalid(e) => write!(f, "the layer cannot be made a disk: {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<io::Error> for BuildError {
    fn from(e: io::Error) -> Self {
        BuildError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Meta {
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: u64,
}

#[derive(Debug)]
enum Kind {
    Dir(BTreeMap<Vec<u8>, usize>),
    /// Size, the first block of its full blocks, and the tail kept for inlining.
    File {
        size: u64,
        start: u32,
        tail: Vec<u8>,
    },
    Symlink(Vec<u8>),
    Fifo,
    Whiteout,
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    meta: Meta,
    /// `(name index, name after the prefix, value)`.
    xattrs: Vec<(u8, Vec<u8>, Vec<u8>)>,
    links: u32,
}

/// What a build wrote, for tests and `machine info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Built {
    pub inodes: u64,
    pub blocks: u64,
    /// Device nodes and hard links to other layers' files, which a layer disk cannot hold.
    pub skipped: u64,
}

/// `path` split into its components, refusing `..` and dropping empty and `.` parts.
fn components(path: &[u8]) -> Result<Vec<Vec<u8>>, BuildError> {
    let mut parts = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                return Err(BuildError::Invalid(format!(
                    "{:?} points outside the layer",
                    String::from_utf8_lossy(path)
                )));
            }
            part if part.len() > NAME_MAX => {
                return Err(BuildError::Invalid(format!(
                    "a name in {:?} is longer than {NAME_MAX} bytes",
                    String::from_utf8_lossy(path)
                )));
            }
            part => parts.push(part.to_vec()),
        }
    }
    Ok(parts)
}

struct Builder<W> {
    out: W,
    nodes: Vec<Node>,
    /// The next free block of the data area.
    next: u64,
    skipped: u64,
}

fn dir_meta() -> Meta {
    Meta {
        mode: S_IFDIR | 0o755,
        ..Meta::default()
    }
}

impl<W: Write + Seek> Builder<W> {
    fn new_node(&mut self, kind: Kind, meta: Meta) -> usize {
        self.nodes.push(Node {
            kind,
            meta,
            xattrs: Vec::new(),
            links: 1,
        });
        self.nodes.len() - 1
    }

    /// The directory at `parts`, made (as 0755 root) where missing; a non-directory in the way is replaced.
    fn dir(&mut self, parts: &[Vec<u8>]) -> usize {
        let mut at = 0;
        for part in parts {
            let existing = match &self.nodes[at].kind {
                Kind::Dir(children) => children.get(part).copied(),
                _ => unreachable!("walks only directories"),
            };
            at = match existing {
                Some(child) if matches!(self.nodes[child].kind, Kind::Dir(_)) => child,
                _ => {
                    let child = self.new_node(Kind::Dir(BTreeMap::new()), dir_meta());
                    self.link(at, part.clone(), child);
                    child
                }
            };
        }
        at
    }

    /// Names `child` `name` in directory `dir`, replacing what had that name.
    fn link(&mut self, dir: usize, name: Vec<u8>, child: usize) {
        let Kind::Dir(children) = &mut self.nodes[dir].kind else {
            unreachable!("links only into directories")
        };
        if let Some(old) = children.insert(name, child) {
            self.nodes[old].links = self.nodes[old].links.saturating_sub(1);
        }
    }

    fn lookup(&self, parts: &[Vec<u8>]) -> Option<usize> {
        let mut at = 0;
        for part in parts {
            match &self.nodes[at].kind {
                Kind::Dir(children) => at = *children.get(part)?,
                _ => return None,
            }
        }
        Some(at)
    }

    /// Streams a file's contents: whole blocks to the data area, the tail into memory when it is short enough to go
    /// inline after the inode and its `xattr_len` bytes of xattrs.
    fn write_data(&mut self, mut data: impl Read, size: u64, xattr_len: usize) -> Result<Kind, BuildError> {
        let start = self.next;
        let mut full = size / BLOCK;
        let mut tail_len = size % BLOCK;
        if tail_len > INLINE_MAX as u64 || INODE_SIZE as u64 + xattr_len as u64 + tail_len > BLOCK {
            // Too long to inline: the tail gets its own block.
            full += 1;
            tail_len = 0;
        }
        let whole = std::cmp::min(size, full * BLOCK);
        if whole > 0 {
            self.out.seek(SeekFrom::Start(start * BLOCK))?;
            let copied = io::copy(&mut (&mut data).take(whole), &mut self.out)?;
            if copied != whole {
                return Err(BuildError::Invalid(
                    "a file in the layer is shorter than its header says".into(),
                ));
            }
            let pad = full * BLOCK - whole;
            self.out.write_all(&vec![0; usize::try_from(pad).unwrap_or(0)])?;
            self.next += full;
        }
        let mut tail = Vec::with_capacity(usize::try_from(tail_len).unwrap_or(0));
        (&mut data).take(tail_len).read_to_end(&mut tail)?;
        if tail.len() as u64 != tail_len {
            return Err(BuildError::Invalid(
                "a file in the layer is shorter than its header says".into(),
            ));
        }
        let start = u32::try_from(if full > 0 { start } else { 0 })
            .map_err(|_| BuildError::Invalid("the layer is larger than 16 TiB".into()))?;
        Ok(Kind::File { size, start, tail })
    }

    fn add_entry<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<(), BuildError> {
        let raw = entry.path_bytes().into_owned();
        let parts = components(&raw)?;
        let header = entry.header();
        let meta = Meta {
            mode: u16::try_from(header.mode()? & 0o7777).unwrap_or(0o644),
            uid: u32::try_from(header.uid()?).unwrap_or(0),
            gid: u32::try_from(header.gid()?).unwrap_or(0),
            mtime: header.mtime()?,
        };
        let kind = header.entry_type();
        let size = header.size()?;
        let link_target = entry.link_name_bytes().map(|t| t.into_owned());
        let mut xattrs = Vec::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for extension in extensions {
                let extension = extension?;
                let Some(name) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") else {
                    continue;
                };
                let (index, rest) = if let Some(rest) = name.strip_prefix(b"user.") {
                    (XATTR_USER, rest)
                } else if let Some(rest) = name.strip_prefix(b"security.") {
                    (XATTR_SECURITY, rest)
                } else {
                    // trusted.* would be read by overlayfs; system.* (ACLs) are left for later.
                    continue;
                };
                xattrs.push((index, rest.to_vec(), extension.value_bytes().to_vec()));
            }
        }

        let Some((base, dir_parts)) = parts.split_last() else {
            // The root itself: keep its owner and mode.
            if kind == tar::EntryType::Directory {
                self.nodes[0].meta = Meta {
                    mode: S_IFDIR | meta.mode,
                    ..meta
                };
                self.nodes[0].xattrs = xattrs;
            }
            return Ok(());
        };
        let dir = self.dir(dir_parts);
        if base.as_slice() == OPAQUE {
            self.nodes[dir]
                .xattrs
                .retain(|(index, name, _)| !(*index == XATTR_TRUSTED && name == b"overlay.opaque"));
            self.nodes[dir]
                .xattrs
                .push((XATTR_TRUSTED, b"overlay.opaque".to_vec(), b"y".to_vec()));
            return Ok(());
        }
        if let Some(hidden) = base.strip_prefix(WHITEOUT) {
            if hidden.is_empty() || hidden == b"." || hidden == b".." {
                return Err(BuildError::Invalid(format!(
                    "{:?} is not a whiteout",
                    String::from_utf8_lossy(&raw)
                )));
            }
            let node = self.new_node(Kind::Whiteout, Meta { mode: S_IFCHR, ..meta });
            self.link(dir, hidden.to_vec(), node);
            return Ok(());
        }
        let node = match kind {
            tar::EntryType::Directory => {
                // A directory over a directory keeps what this layer already put in it.
                let existing = match &self.nodes[dir].kind {
                    Kind::Dir(children) => children.get(base).copied(),
                    _ => None,
                };
                if let Some(existing) = existing.filter(|&e| matches!(self.nodes[e].kind, Kind::Dir(_))) {
                    self.nodes[existing].meta = Meta {
                        mode: S_IFDIR | meta.mode,
                        ..meta
                    };
                    let opaque: Vec<_> = self.nodes[existing]
                        .xattrs
                        .iter()
                        .filter(|(index, _, _)| *index == XATTR_TRUSTED)
                        .cloned()
                        .collect();
                    self.nodes[existing].xattrs = xattrs.into_iter().chain(opaque).collect();
                    return Ok(());
                }
                self.new_node(
                    Kind::Dir(BTreeMap::new()),
                    Meta {
                        mode: S_IFDIR | meta.mode,
                        ..meta
                    },
                )
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let xattr_len = xattr_body(&xattrs)?.len();
                let data = self.write_data(&mut entry, size, xattr_len)?;
                self.new_node(
                    data,
                    Meta {
                        mode: S_IFREG | meta.mode,
                        ..meta
                    },
                )
            }
            tar::EntryType::Symlink => {
                let target = link_target.unwrap_or_default();
                if target.is_empty() || target.len() >= BLOCK as usize {
                    return Err(BuildError::Invalid(format!(
                        "the symbolic link {:?} has no usable target",
                        String::from_utf8_lossy(&raw)
                    )));
                }
                self.new_node(
                    Kind::Symlink(target),
                    Meta {
                        mode: S_IFLNK | 0o777,
                        ..meta
                    },
                )
            }
            tar::EntryType::Link => {
                let target = components(&link_target.unwrap_or_default())?;
                match self.lookup(&target) {
                    Some(existing) if matches!(self.nodes[existing].kind, Kind::File { .. }) => {
                        self.nodes[existing].links += 1;
                        self.link(dir, base.clone(), existing);
                    }
                    // A hard link to a file in a lower layer cannot be expressed in this one.
                    _ => self.skipped += 1,
                }
                return Ok(());
            }
            tar::EntryType::Fifo => self.new_node(
                Kind::Fifo,
                Meta {
                    mode: S_IFIFO | meta.mode,
                    ..meta
                },
            ),
            tar::EntryType::Char | tar::EntryType::Block => {
                self.skipped += 1;
                return Ok(());
            }
            _ => return Ok(()),
        };
        self.nodes[node].xattrs = xattrs;
        self.link(dir, base.clone(), node);
        Ok(())
    }
}

/// The inline xattr body of a node: the 12-byte header, then each entry padded to 4 bytes.
fn xattr_body(xattrs: &[(u8, Vec<u8>, Vec<u8>)]) -> Result<Vec<u8>, BuildError> {
    if xattrs.is_empty() {
        return Ok(Vec::new());
    }
    let mut body = vec![0u8; 12];
    for (index, name, value) in xattrs {
        let name_len = u8::try_from(name.len()).map_err(|_| BuildError::Invalid("an xattr name is too long".into()))?;
        let value_len =
            u16::try_from(value.len()).map_err(|_| BuildError::Invalid("an xattr value is too long".into()))?;
        body.push(name_len);
        body.push(*index);
        body.extend(value_len.to_le_bytes());
        body.extend(name);
        body.extend(value);
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
    }
    Ok(body)
}

/// One directory entry: name, the node it names, and its file type.
type Dirent = (Vec<u8>, usize, u8);
/// The entries of one directory block.
type DirBlock = Vec<Dirent>;

/// A directory's entries (`.` and `..` included) packed into blocks.
fn dir_blocks(entries: Vec<Dirent>) -> Vec<DirBlock> {
    let mut blocks: Vec<DirBlock> = vec![Vec::new()];
    let mut used = 0;
    for entry in entries {
        let size = DIRENT_SIZE + entry.0.len();
        if used + size > BLOCK as usize {
            blocks.push(Vec::new());
            used = 0;
        }
        used += size;
        blocks.last_mut().expect("one block at least").push(entry);
    }
    blocks
}

fn dir_block_bytes(block: &[Dirent], nids: &[u64]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut nameoff = block.len() * DIRENT_SIZE;
    for (name, child, file_type) in block {
        bytes.extend(nids[*child].to_le_bytes());
        bytes.extend(u16::try_from(nameoff).unwrap_or(0).to_le_bytes());
        bytes.push(*file_type);
        bytes.push(0);
        nameoff += name.len();
    }
    for (name, _, _) in block {
        bytes.extend(name);
    }
    bytes
}

fn file_type(kind: &Kind) -> u8 {
    match kind {
        Kind::Dir(_) => FT_DIR,
        Kind::File { .. } => FT_REG,
        Kind::Symlink(_) => FT_LNK,
        Kind::Fifo => FT_FIFO,
        Kind::Whiteout => FT_CHR,
    }
}

/// How a node's data is laid out once the metadata is placed.
struct Placed {
    layout: u16,
    size: u64,
    start: u32,
    /// Bytes after the inode and its xattrs.
    inline: Vec<u8>,
    xattrs: Vec<u8>,
}

/// Writes the EROFS image of the uncompressed layer tarball `layer` to `out`, which should be empty.
#[cfg_attr(not(test), expect(dead_code, reason = "run attaches the disks in #17"))]
pub fn build(layer: impl Read, out: impl Write + Seek) -> Result<Built, BuildError> {
    let mut builder = Builder {
        out,
        nodes: Vec::new(),
        next: 1,
        skipped: 0,
    };
    builder.new_node(Kind::Dir(BTreeMap::new()), dir_meta());
    let mut archive = tar::Archive::new(layer);
    for entry in archive.entries()? {
        builder.add_entry(entry?)?;
    }
    finish(builder)
}

fn finish<W: Write + Seek>(mut builder: Builder<W>) -> Result<Built, BuildError> {
    // Only what is reachable from the root, in breadth-first order so the root is nid 0.
    let mut order = vec![0usize];
    let mut parent = vec![usize::MAX; builder.nodes.len()];
    parent[0] = 0;
    let mut seen = vec![false; builder.nodes.len()];
    seen[0] = true;
    let mut i = 0;
    while i < order.len() {
        if let Kind::Dir(children) = &builder.nodes[order[i]].kind {
            for &child in children.values() {
                if !seen[child] {
                    seen[child] = true;
                    parent[child] = order[i];
                    order.push(child);
                }
            }
        }
        i += 1;
    }

    // Directory contents, block by block, with `.` and `..`.
    let mut dirs: Vec<Option<Vec<DirBlock>>> = (0..builder.nodes.len()).map(|_| None).collect();
    let mut subdirs = vec![0u32; builder.nodes.len()];
    for &node in &order {
        if let Kind::Dir(children) = &builder.nodes[node].kind {
            let mut entries = vec![(b".".to_vec(), node, FT_DIR), (b"..".to_vec(), parent[node], FT_DIR)];
            for (name, &child) in children {
                let kind = &builder.nodes[child].kind;
                if matches!(kind, Kind::Dir(_)) {
                    subdirs[node] += 1;
                }
                entries.push((name.clone(), child, file_type(kind)));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            dirs[node] = Some(dir_blocks(entries));
        }
    }

    // Place the inodes in the metadata area, which starts after the file data. An inode, its xattrs and its
    // inline data stay inside one block.
    let meta_start = builder.next;
    let mut cursor = meta_start * BLOCK;
    let mut placed: Vec<Option<Placed>> = (0..builder.nodes.len()).map(|_| None).collect();
    let mut offsets = vec![0u64; builder.nodes.len()];
    let mut trailing: Vec<(usize, Vec<Vec<u8>>)> = Vec::new();
    for &node in &order {
        let xattrs = xattr_body(&builder.nodes[node].xattrs)?;
        let (mut layout, size, start, mut inline) = match &builder.nodes[node].kind {
            Kind::File { size, start, tail } => {
                if tail.is_empty() {
                    (LAYOUT_FLAT_PLAIN, *size, *start, Vec::new())
                } else {
                    (LAYOUT_FLAT_INLINE, *size, *start, tail.clone())
                }
            }
            Kind::Symlink(target) => (LAYOUT_FLAT_INLINE, target.len() as u64, 0, target.clone()),
            Kind::Dir(_) => {
                let blocks = dirs[node].as_ref().expect("directories have blocks");
                let last = blocks.last().expect("one block at least");
                let last_len = last.iter().map(|(name, _, _)| DIRENT_SIZE + name.len()).sum::<usize>();
                let size = (blocks.len() as u64 - 1) * BLOCK + last_len as u64;
                // Placeholder bytes of the right length; the nids are filled in once every inode is placed.
                (LAYOUT_FLAT_INLINE, size, 0, vec![0; last_len])
            }
            Kind::Fifo | Kind::Whiteout => (LAYOUT_FLAT_PLAIN, 0, 0, Vec::new()),
        };
        if INODE_SIZE + xattrs.len() + inline.len() > BLOCK as usize {
            match builder.nodes[node].kind {
                // A directory's last block then goes after the metadata with its others.
                Kind::Dir(_) => {
                    layout = LAYOUT_FLAT_PLAIN;
                    inline = Vec::new();
                }
                // File tails were sized for this when their data was written.
                _ => {
                    return Err(BuildError::Invalid(
                        "an entry with xattrs too large for its inode".into(),
                    ));
                }
            }
        }
        let total = (INODE_SIZE + xattrs.len() + inline.len()) as u64;
        cursor = cursor.div_ceil(32) * 32;
        if cursor % BLOCK + total > BLOCK {
            cursor = cursor.div_ceil(BLOCK) * BLOCK;
        }
        offsets[node] = cursor;
        cursor += total;
        placed[node] = Some(Placed {
            layout,
            size,
            start,
            inline,
            xattrs,
        });
    }
    // Entries a later one replaced are not reachable, have no inode, and are never named.
    let nids: Vec<u64> = offsets
        .iter()
        .map(|o| o.saturating_sub(meta_start * BLOCK) / 32)
        .collect();

    // Directory blocks after the metadata: all of a directory's full blocks, and its last one when not inline.
    let mut next = cursor.div_ceil(BLOCK);
    for &node in &order {
        if let Some(blocks) = &dirs[node] {
            let place = placed[node].as_mut().expect("placed");
            let in_blocks = if place.layout == LAYOUT_FLAT_INLINE {
                blocks.len() - 1
            } else {
                blocks.len()
            };
            if in_blocks > 0 {
                place.start = u32::try_from(next).map_err(|_| BuildError::Invalid("the layer is too large".into()))?;
                trailing.push((
                    node,
                    blocks[..in_blocks].iter().map(|b| dir_block_bytes(b, &nids)).collect(),
                ));
                next += in_blocks as u64;
            }
            if place.layout == LAYOUT_FLAT_INLINE {
                place.inline = dir_block_bytes(blocks.last().expect("one block at least"), &nids);
            }
        }
    }
    // Write the inodes.
    for &node in &order {
        let n = &builder.nodes[node];
        let place = placed[node].as_ref().expect("placed");
        let mode = n.meta.mode;
        let links = match &n.kind {
            Kind::Dir(_) => 2 + subdirs[node],
            _ => n.links.max(1),
        };
        let icount = if place.xattrs.is_empty() {
            0
        } else {
            u16::try_from((place.xattrs.len() - 12) / 4 + 1)
                .map_err(|_| BuildError::Invalid("too many xattrs".into()))?
        };
        let mut inode = Vec::with_capacity(INODE_SIZE);
        inode.extend((VERSION_EXTENDED | (place.layout << 1)).to_le_bytes());
        inode.extend(icount.to_le_bytes());
        inode.extend(mode.to_le_bytes());
        inode.extend(0u16.to_le_bytes()); // i_nb: startblk_hi
        inode.extend(place.size.to_le_bytes());
        let i_u = match n.kind {
            Kind::Whiteout => 0, // rdev 0:0
            _ => place.start,
        };
        inode.extend(i_u.to_le_bytes());
        inode.extend(u32::try_from(nids[node] + 1).unwrap_or(u32::MAX).to_le_bytes());
        inode.extend(n.meta.uid.to_le_bytes());
        inode.extend(n.meta.gid.to_le_bytes());
        inode.extend(n.meta.mtime.to_le_bytes());
        inode.extend(0u32.to_le_bytes()); // mtime nsec
        inode.extend(links.to_le_bytes());
        inode.extend([0u8; 16]);
        debug_assert_eq!(inode.len(), INODE_SIZE);
        builder.out.seek(SeekFrom::Start(offsets[node]))?;
        builder.out.write_all(&inode)?;
        builder.out.write_all(&place.xattrs)?;
        builder.out.write_all(&place.inline)?;
    }
    for (node, blocks) in &trailing {
        let start = u64::from(placed[*node].as_ref().expect("placed").start);
        builder.out.seek(SeekFrom::Start(start * BLOCK))?;
        for block in blocks {
            builder.out.write_all(block)?;
            builder.out.write_all(&vec![0; BLOCK as usize - block.len()])?;
        }
    }

    // The image ends on a block boundary.
    let blocks = next.max(cursor.div_ceil(BLOCK));
    builder.out.seek(SeekFrom::Start(blocks * BLOCK - 1))?;
    builder.out.write_all(&[0])?;

    let mut sb = Vec::with_capacity(128);
    sb.extend(MAGIC.to_le_bytes());
    sb.extend(0u32.to_le_bytes()); // checksum (no SB_CHKSUM)
    sb.extend(0u32.to_le_bytes()); // feature_compat
    sb.push(BLOCK_BITS);
    sb.push(0); // sb_extslots
    sb.extend(u16::try_from(nids[0]).unwrap_or(0).to_le_bytes()); // root nid
    sb.extend((order.len() as u64).to_le_bytes()); // inos
    sb.extend(0u64.to_le_bytes()); // epoch
    sb.extend(0u32.to_le_bytes()); // fixed_nsec
    sb.extend(
        u32::try_from(blocks)
            .map_err(|_| BuildError::Invalid("the layer is too large".into()))?
            .to_le_bytes(),
    );
    sb.extend(u32::try_from(meta_start).unwrap_or(0).to_le_bytes()); // meta_blkaddr
    sb.extend(0u32.to_le_bytes()); // xattr_blkaddr: no shared xattrs
    sb.extend([0u8; 16]); // uuid
    sb.extend([0u8; 16]); // volume name
    sb.extend(0u32.to_le_bytes()); // feature_incompat
    sb.resize(128, 0);
    builder.out.seek(SeekFrom::Start(SUPER_OFFSET))?;
    builder.out.write_all(&sb)?;
    builder.out.flush()?;
    Ok(Built {
        inodes: order.len() as u64,
        blocks,
        skipped: builder.skipped,
    })
}

#[cfg(test)]
pub(crate) mod reader {
    //! Reads back what [`super::build`] wrote, checking what the kernel checks, so tests can compare trees.
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Inode {
        pub(crate) mode: u16,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
        pub(crate) mtime: u64,
        pub(crate) size: u64,
        pub(crate) nlink: u32,
        pub(crate) rdev: u32,
        pub(crate) xattrs: Vec<(u8, Vec<u8>, Vec<u8>)>,
        layout: u16,
        start: u64,
        inline_at: usize,
    }

    pub(crate) struct Image {
        bytes: Vec<u8>,
        meta: usize,
        pub(crate) root: u64,
        pub(crate) inos: u64,
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    impl Image {
        pub(crate) fn open(bytes: Vec<u8>) -> Image {
            let sb = SUPER_OFFSET as usize;
            assert_eq!(u32_at(&bytes, sb), MAGIC, "magic");
            assert_eq!(bytes[sb + 12], BLOCK_BITS);
            assert_eq!(bytes[sb + 90], 0, "dirblkbits");
            assert_eq!(u32_at(&bytes, sb + 80), 0, "no incompatible features");
            let blocks = u32_at(&bytes, sb + 36) as usize;
            assert_eq!(bytes.len(), blocks * BLOCK as usize, "the image is its block count");
            Image {
                meta: u32_at(&bytes, sb + 40) as usize * BLOCK as usize,
                root: u64::from(u16_at(&bytes, sb + 14)),
                inos: u64_at(&bytes, sb + 16),
                bytes,
            }
        }

        pub(crate) fn inode(&self, nid: u64) -> Inode {
            let at = self.meta + nid as usize * 32;
            let b = &self.bytes;
            let format = u16_at(b, at);
            assert_eq!(format & 1, VERSION_EXTENDED, "extended inode at nid {nid}");
            let layout = (format >> 1) & 7;
            assert!(layout == LAYOUT_FLAT_PLAIN || layout == LAYOUT_FLAT_INLINE);
            let icount = usize::from(u16_at(b, at + 2));
            let xattr_size = if icount == 0 { 0 } else { 12 + 4 * (icount - 1) };
            let mut xattrs = Vec::new();
            let mut x = at + INODE_SIZE + 12;
            while x < at + INODE_SIZE + xattr_size {
                let name_len = usize::from(b[x]);
                let index = b[x + 1];
                let value_len = usize::from(u16_at(b, x + 2));
                let name = b[x + 4..x + 4 + name_len].to_vec();
                let value = b[x + 4 + name_len..x + 4 + name_len + value_len].to_vec();
                xattrs.push((index, name, value));
                x += (4 + name_len + value_len).div_ceil(4) * 4;
            }
            let size = u64_at(b, at + 8);
            let i_u = u32_at(b, at + 16);
            let inode = Inode {
                mode: u16_at(b, at + 4),
                uid: u32_at(b, at + 24),
                gid: u32_at(b, at + 28),
                mtime: u64_at(b, at + 32),
                size,
                nlink: u32_at(b, at + 44),
                rdev: i_u,
                xattrs,
                layout,
                start: u64::from(i_u) | (u64::from(u16_at(b, at + 6)) << 32),
                inline_at: at + INODE_SIZE + xattr_size,
            };
            if layout == LAYOUT_FLAT_INLINE {
                // The kernel reads the tail from the inode's block: it must not cross into the next one.
                let tail = (size % BLOCK) as usize;
                let tail = if tail == 0 && size > 0 { BLOCK as usize } else { tail };
                assert!(
                    inode.inline_at % BLOCK as usize + tail <= BLOCK as usize || tail == 0,
                    "inline tail of nid {nid} crosses a block"
                );
            }
            assert!(at % (BLOCK as usize) + INODE_SIZE <= BLOCK as usize || layout == LAYOUT_FLAT_PLAIN);
            inode
        }

        /// The data of a regular file, directory or symlink, mapped as `erofs_map_blocks` maps it.
        pub(crate) fn data(&self, inode: &Inode) -> Vec<u8> {
            let size = inode.size as usize;
            let blocks = size.div_ceil(BLOCK as usize);
            let in_blocks = if inode.layout == LAYOUT_FLAT_INLINE {
                blocks.saturating_sub(1)
            } else {
                blocks
            };
            let from = inode.start as usize * BLOCK as usize;
            let mut data = self.bytes[from..from + std::cmp::min(size, in_blocks * BLOCK as usize)].to_vec();
            if inode.layout == LAYOUT_FLAT_INLINE {
                let rest = size - data.len();
                data.extend(&self.bytes[inode.inline_at..inode.inline_at + rest]);
            }
            data
        }

        /// A directory's entries as the kernel's readdir reads them.
        pub(crate) fn entries(&self, nid: u64) -> Vec<(Vec<u8>, u64, u8)> {
            let inode = self.inode(nid);
            assert_eq!(inode.mode & 0o170000, S_IFDIR);
            let data = self.data(&inode);
            let mut found = Vec::new();
            for block in data.chunks(BLOCK as usize) {
                let nameoff0 = usize::from(u16_at(block, 8));
                assert!(nameoff0 >= DIRENT_SIZE && nameoff0 < BLOCK as usize && nameoff0 % DIRENT_SIZE == 0);
                let count = nameoff0 / DIRENT_SIZE;
                for i in 0..count {
                    let d = i * DIRENT_SIZE;
                    let off = usize::from(u16_at(block, d + 8));
                    let end = if i + 1 < count {
                        usize::from(u16_at(block, d + DIRENT_SIZE + 8))
                    } else {
                        off + block[off..].iter().position(|&c| c == 0).unwrap_or(block.len() - off)
                    };
                    assert!(end - off <= NAME_MAX && end <= block.len());
                    found.push((block[off..end].to_vec(), u64_at(block, d), block[d + 10]));
                }
            }
            let names: Vec<_> = found.iter().map(|e| e.0.clone()).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "entries are sorted for the kernel's binary search");
            found
        }

        /// Every path (`/` for the root) with its inode, following directories.
        pub(crate) fn walk(&self) -> BTreeMap<String, (u64, Inode)> {
            let mut all = BTreeMap::new();
            let mut todo = vec![(String::new(), self.root)];
            while let Some((path, nid)) = todo.pop() {
                let inode = self.inode(nid);
                if inode.mode & 0o170000 == S_IFDIR {
                    for (name, child, _) in self.entries(nid) {
                        if name == b"." || name == b".." {
                            continue;
                        }
                        todo.push((format!("{path}/{}", String::from_utf8_lossy(&name)), child));
                    }
                }
                all.insert(if path.is_empty() { "/".into() } else { path }, (nid, inode));
            }
            all
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reader::Image;
    use super::*;

    /// A tarball written entry by entry, with PAX xattrs where asked.
    struct Tar(tar::Builder<Vec<u8>>);

    impl Tar {
        fn new() -> Tar {
            Tar(tar::Builder::new(Vec::new()))
        }

        /// A header naming `path` as is: tar's own setter refuses `..`, which these tests need to write.
        fn header(path: &str, kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_uid(1000);
            header.set_gid(1001);
            header.set_mtime(1_700_000_000);
            header.set_size(size);
            let name = &mut header.as_old_mut().name;
            let len = std::cmp::min(path.len(), name.len());
            name[..len].copy_from_slice(&path.as_bytes()[..len]);
            header
        }

        fn xattrs(self, pairs: &[(&str, &[u8])]) -> Tar {
            let records: Vec<(String, Vec<u8>)> = pairs
                .iter()
                .map(|(name, value)| (format!("SCHILY.xattr.{name}"), value.to_vec()))
                .collect();
            self.pax(&records)
        }

        /// A PAX extended header with `records`, for the entry after it.
        fn pax(mut self, records: &[(String, Vec<u8>)]) -> Tar {
            let mut body = Vec::new();
            for (name, value) in records {
                let record = |len: usize| {
                    let mut r = format!("{len} {name}=").into_bytes();
                    r.extend(value);
                    r.push(b'\n');
                    r
                };
                // The length counts its own digits.
                let mut len = record(0).len();
                while record(len).len() != len {
                    len = record(len).len();
                }
                body.extend(record(len));
            }
            let mut header = Tar::header("pax", tar::EntryType::XHeader, 0o644, body.len() as u64);
            header.set_cksum();
            self.0.append(&header, &body[..]).unwrap();
            self
        }

        fn add(mut self, path: &str, kind: tar::EntryType, mode: u32, data: &[u8]) -> Tar {
            if path.len() > 100 {
                self = self.pax(&[("path".to_string(), path.as_bytes().to_vec())]);
            }
            let mut header = Tar::header(path, kind, mode, data.len() as u64);
            header.set_cksum();
            self.0.append(&header, data).unwrap();
            self
        }

        fn dir(self, path: &str) -> Tar {
            self.add(path, tar::EntryType::Directory, 0o750, b"")
        }

        fn file(self, path: &str, data: &[u8]) -> Tar {
            self.add(path, tar::EntryType::Regular, 0o640, data)
        }

        fn link(mut self, path: &str, kind: tar::EntryType, target: &str) -> Tar {
            let mut header = Tar::header(path, kind, 0o777, 0);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            self.0.append(&header, &b""[..]).unwrap();
            self
        }

        fn bytes(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }

    fn image_of(tar: Vec<u8>) -> (Image, Built) {
        let mut out = io::Cursor::new(Vec::new());
        let built = build(&tar[..], &mut out).unwrap();
        (Image::open(out.into_inner()), built)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn files_of_every_size_read_back_exactly() {
        let sizes = [
            0,
            1,
            100,
            2048,
            2049,
            4095,
            4096,
            4097,
            8192 + 2000,
            8192 + 3000,
            3 * 4096,
        ];
        let mut tar = Tar::new().dir("data");
        for size in sizes {
            tar = tar.file(&format!("data/f{size}"), &pattern(size));
        }
        let (image, built) = image_of(tar.bytes());
        let all = image.walk();
        for size in sizes {
            let (_, inode) = &all[&format!("/data/f{size}")];
            assert_eq!(inode.size, size as u64);
            assert_eq!(image.data(inode), pattern(size), "file of {size} bytes");
            assert_eq!((inode.mode, inode.uid, inode.gid), (S_IFREG | 0o640, 1000, 1001));
            assert_eq!((inode.mtime, inode.nlink), (1_700_000_000, 1));
        }
        assert_eq!(all["/data"].1.mode, S_IFDIR | 0o750);
        assert_eq!(built.inodes, image.inos);
        assert_eq!(built.inodes, sizes.len() as u64 + 2);
        assert_eq!(image.root, 0, "the root is the first inode");
    }

    #[test]
    fn links_symlinks_fifos_devices_and_parents() {
        let tar = Tar::new()
            .file("usr/bin/tool", b"#!/bin/sh\n")
            .link("usr/bin/same", tar::EntryType::Link, "usr/bin/tool")
            .link("usr/bin/sh", tar::EntryType::Symlink, "/bin/busybox")
            .add("run/pipe", tar::EntryType::Fifo, 0o600, b"")
            .add("dev/null", tar::EntryType::Char, 0o666, b"")
            .link("lost", tar::EntryType::Link, "in/a/lower/layer");
        let (image, built) = image_of(tar.bytes());
        let all = image.walk();
        let (tool_nid, tool) = &all["/usr/bin/tool"];
        let (same_nid, _) = &all["/usr/bin/same"];
        assert_eq!(tool_nid, same_nid, "a hard link is the same inode");
        assert_eq!(tool.nlink, 2);
        let (_, sh) = &all["/usr/bin/sh"];
        assert_eq!(sh.mode, S_IFLNK | 0o777);
        assert_eq!(image.data(sh), b"/bin/busybox");
        assert_eq!(all["/run/pipe"].1.mode, S_IFIFO | 0o600);
        // Parents the layer did not list are made 0755 root.
        assert_eq!((all["/usr"].1.mode, all["/usr"].1.uid), (S_IFDIR | 0o755, 0));
        // Devices and a hard link to a lower layer are left out.
        assert!(!all.contains_key("/dev/null") && !all.contains_key("/lost"));
        assert_eq!(built.skipped, 2);
        // Directory link counts: 2, plus one per subdirectory.
        assert_eq!(all["/"].1.nlink, 2 + 3);
        assert_eq!(all["/usr"].1.nlink, 3);
        let root_entries = image.entries(image.root);
        assert_eq!(&root_entries[0].0, b".");
        assert_eq!((&root_entries[1].0, root_entries[1].1), (&b"..".to_vec(), image.root));
    }

    #[test]
    fn whiteouts_and_opaque_directories_are_written_as_overlayfs_reads_them() {
        let tar = Tar::new()
            .file("etc/.wh.passwd", b"")
            .dir("opt/app")
            .file("opt/app/.wh..wh..opq", b"")
            .file("opt/app/new", b"x")
            .file(".wh.tmp", b"");
        let (image, _) = image_of(tar.bytes());
        let all = image.walk();
        let (_, passwd) = &all["/etc/passwd"];
        assert_eq!(
            (passwd.mode & 0o170000, passwd.rdev),
            (S_IFCHR, 0),
            "a whiteout is a 0:0 device"
        );
        assert_eq!(all["/tmp"].1.mode & 0o170000, S_IFCHR);
        assert!(!all.contains_key("/etc/.wh.passwd"));
        let (_, app) = &all["/opt/app"];
        assert_eq!(app.xattrs, [(XATTR_TRUSTED, b"overlay.opaque".to_vec(), b"y".to_vec())]);
        assert!(all.contains_key("/opt/app/new") && !all.contains_key("/opt/app/.wh..wh..opq"));
        let entries = image.entries(all["/etc"].0);
        assert_eq!(entries.iter().find(|e| e.0 == b"passwd").unwrap().2, FT_CHR);
    }

    #[test]
    fn the_layers_own_trusted_xattrs_are_dropped() {
        let tar = Tar::new()
            .xattrs(&[
                ("security.capability", b"\x01\x00\x00\x02"),
                ("user.note", b"kept"),
                ("trusted.overlay.redirect", b"/etc"),
                ("trusted.overlay.opaque", b"y"),
            ])
            .file("bin/ping", &pattern(3000))
            .xattrs(&[("user.big", &[b'v'; 3000])])
            .file("big-xattr", &pattern(1500));
        let (image, _) = image_of(tar.bytes());
        let all = image.walk();
        let (_, ping) = &all["/bin/ping"];
        assert_eq!(
            ping.xattrs,
            [
                (XATTR_SECURITY, b"capability".to_vec(), b"\x01\x00\x00\x02".to_vec()),
                (XATTR_USER, b"note".to_vec(), b"kept".to_vec()),
            ]
        );
        assert_eq!(image.data(ping), pattern(3000));
        // A tail that does not fit beside large xattrs gets a block of its own.
        let (_, big) = &all["/big-xattr"];
        assert_eq!(big.xattrs[0].2.len(), 3000);
        assert_eq!(image.data(big), pattern(1500));
    }

    #[test]
    fn large_directories_span_blocks_and_stay_sorted() {
        let mut tar = Tar::new();
        let mut names: Vec<String> = (0..700)
            .map(|i| format!("entry-{i:04}-{}", "n".repeat(i % 40)))
            .collect();
        names.push("x".repeat(NAME_MAX));
        for name in &names {
            tar = tar.file(&format!("many/{name}"), name.as_bytes());
        }
        let (image, _) = image_of(tar.bytes());
        let all = image.walk();
        assert!(all["/many"].1.size > 3 * BLOCK, "{}", all["/many"].1.size);
        for name in &names {
            let (_, inode) = &all[&format!("/many/{name}")];
            assert_eq!(image.data(inode), name.as_bytes());
        }
        assert_eq!(image.entries(all["/many"].0).len(), names.len() + 2);
    }

    #[test]
    fn later_entries_replace_earlier_ones_and_the_root_keeps_its_mode() {
        let tar = Tar::new()
            .add("./", tar::EntryType::Directory, 0o700, b"")
            .file("a", b"first")
            .file("a", b"second")
            .file("d/inner", b"x")
            .file("d", b"now a file");
        let (image, _) = image_of(tar.bytes());
        let all = image.walk();
        assert_eq!(all["/"].1.mode, S_IFDIR | 0o700);
        assert_eq!(image.data(&all["/a"].1), b"second");
        assert_eq!(image.data(&all["/d"].1), b"now a file");
        assert!(!all.contains_key("/d/inner"));
    }

    /// One layer with every kind of entry, for `fsck.erofs`: with `$OCI_HERMIT_EROFS_SAMPLES` set, the image and
    /// the expected listing (`path type mode uid gid size sha256`, as `find` and `sha256sum` print them after
    /// `fsck.erofs --extract`) are written there. CI's Linux test job checks them with erofs-utils.
    #[test]
    fn writes_a_sample_for_fsck_erofs() {
        let mut tar = Tar::new()
            .dir("etc")
            .file("etc/.wh.shadow", b"")
            .dir("opt/app")
            .file("opt/app/.wh..wh..opq", b"")
            .xattrs(&[("security.capability", b"\x01\x00\x00\x02"), ("user.note", b"kept")])
            .file("bin/tool", &pattern(9000))
            .link("bin/same", tar::EntryType::Link, "bin/tool")
            .link("bin/sh", tar::EntryType::Symlink, "/bin/busybox")
            .add("run/pipe", tar::EntryType::Fifo, 0o600, b"");
        for size in [0, 1, 2048, 2049, 4096, 4097, 12_288, 70_000] {
            tar = tar.file(&format!("data/f{size}"), &pattern(size));
        }
        for i in 0..400 {
            tar = tar.file(&format!("many/{i:04}-{}", "n".repeat(i % 60)), &pattern(i));
        }
        let tar = tar.bytes();
        let mut out = io::Cursor::new(Vec::new());
        build(&tar[..], &mut out).unwrap();
        let bytes = out.into_inner();
        let image = Image::open(bytes.clone());
        let mut listing = String::new();
        for (path, (_, inode)) in image.walk() {
            let kind = match inode.mode & 0o170000 {
                S_IFDIR => "d",
                S_IFREG => "f",
                S_IFLNK => "l",
                S_IFIFO => "p",
                S_IFCHR => "c",
                _ => "?",
            };
            let sum = if kind == "f" {
                use sha2::Digest;
                let hash = sha2::Sha256::digest(image.data(&inode));
                hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
            } else {
                "-".into()
            };
            let size = if kind == "f" { inode.size } else { 0 };
            listing.push_str(&format!(
                "{path} {kind} {:o} {} {} {size} {sum}\n",
                inode.mode & 0o7777,
                inode.uid,
                inode.gid
            ));
        }
        if let Some(dir) = std::env::var_os("OCI_HERMIT_EROFS_SAMPLES") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("sample.erofs"), &bytes).unwrap();
            std::fs::write(dir.join("sample.expected"), &listing).unwrap();
        }
        assert!(listing.contains("/bin/tool f 640 1000 1001 9000 "), "{listing}");
    }

    #[test]
    fn layers_that_climb_out_or_lie_about_sizes_are_refused() {
        let climbing = Tar::new().file("../etc/passwd", b"x").bytes();
        let err = build(&climbing[..], io::Cursor::new(Vec::new())).unwrap_err();
        assert!(err.to_string().contains("outside the layer"), "{err}");
        let long = Tar::new()
            .file(&format!("{}/f", "d".repeat(NAME_MAX + 1)), b"x")
            .bytes();
        assert!(build(&long[..], io::Cursor::new(Vec::new())).is_err());
        let mut short = Tar::new().file("f", &pattern(5000)).bytes();
        short.truncate(512 + 4096 + 100);
        assert!(build(&short[..], io::Cursor::new(Vec::new())).is_err());
        let empty = Tar::new().bytes();
        let (image, built) = image_of(empty);
        assert_eq!((built.inodes, image.walk().len()), (1, 1));
    }
}
