//! The local image store: one OCI image layout under the user's data directory, holding every pulled image.
//!
//! Blobs are shared between images and written only after their digest checks out; each image is an entry in
//! `index.json` named by its full reference (`docker.io/library/debian:bookworm-slim`) and its platform. Nothing
//! here needs privileges: it is a folder the user owns.

use crate::image::{self, ImageError, REF_NAME};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Overrides where the store lives.
pub const DIR_ENV: &str = "OCI_HERMIT_DATA_DIR";

/// Where oci-hermit keeps its data: `$OCI_HERMIT_DATA_DIR`, else the platform's per-user data folder.
pub fn data_dir(override_dir: Option<OsString>, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        PathBuf::from(var("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(var("HOME")?).join("Library/Application Support")
    } else {
        match var("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(var("HOME")?).join(".local/share"),
        }
    };
    Some(base.join("oci-hermit"))
}

/// The image store in a data directory.
///
/// An open store holds a shared lock on `images/lock`, so commands that add to it or read from it (`pull`, `load`,
/// `run`) can run side by side; [`Store::open_exclusive`] takes it alone, for commands that delete blobs.
pub struct Store {
    dir: PathBuf,
    _lock: std::fs::File,
}

/// One entry in `index.json`: a name for one platform's manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The full reference, or the image ID for an image loaded without a name.
    pub name: String,
    pub os: String,
    pub arch: String,
    pub media_type: String,
    /// The manifest's digest.
    pub digest: String,
    pub size: u64,
}

impl Entry {
    fn from_json(m: &Value) -> Option<Entry> {
        let text = |v: &Value| v.as_str().map(String::from);
        Some(Entry {
            name: text(&m["annotations"][REF_NAME])?,
            os: text(&m["platform"]["os"]).unwrap_or_default(),
            arch: text(&m["platform"]["architecture"]).unwrap_or_default(),
            media_type: text(&m["mediaType"]).unwrap_or_default(),
            digest: text(&m["digest"])?,
            size: m["size"].as_u64().unwrap_or(0),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "mediaType": self.media_type,
            "digest": self.digest,
            "size": self.size,
            "platform": { "os": self.os, "architecture": self.arch },
            "annotations": { REF_NAME: self.name },
        })
    }
}

/// Another oci-hermit is using the store, so blobs cannot be deleted now.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the image store is in use by another oci-hermit (a pull, load or run); try again when it has finished"
        )
    }
}

impl std::error::Error for Busy {}

impl Store {
    /// The store under `data_dir`, created if it does not exist yet, shared with other readers and writers.
    pub fn open(data_dir: &Path) -> std::io::Result<Store> {
        let dir = Store::create(data_dir)?;
        let lock = lock_file(&dir)?;
        lock.lock_shared()?;
        Ok(Store { dir, _lock: lock })
    }

    /// The store under `data_dir`, for this command alone; [`Busy`] if another oci-hermit still has it open after
    /// `patience`. A short wait rides out brief holders: a `run` reading its image, or a child process that has
    /// inherited the lock between `fork` and `exec`.
    pub fn open_exclusive(data_dir: &Path, patience: std::time::Duration) -> Result<Store, Box<dyn std::error::Error>> {
        let dir = Store::create(data_dir)?;
        let lock = lock_file(&dir)?;
        let deadline = std::time::Instant::now() + patience;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Store { dir, _lock: lock }),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(Box::new(Busy)),
                Err(std::fs::TryLockError::Error(e)) => return Err(Box::new(e)),
            }
        }
    }

    fn create(data_dir: &Path) -> std::io::Result<PathBuf> {
        let dir = data_dir.join("images");
        std::fs::create_dir_all(dir.join("blobs").join("sha256"))?;
        let layout = dir.join("oci-layout");
        if !layout.exists() {
            std::fs::write(&layout, r#"{"imageLayoutVersion":"1.0.0"}"#)?;
        }
        if !dir.join("index.json").exists() {
            write_atomically(&dir.join("index.json"), br#"{"schemaVersion":2,"manifests":[]}"#)?;
        }
        Ok(dir)
    }

    /// The layout directory, for [`image::Image::open_named`].
    pub fn path(&self) -> &Path {
        &self.dir
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.dir
            .join("blobs")
            .join("sha256")
            .join(digest.trim_start_matches("sha256:"))
    }

    /// Whether the store already has the blob with `digest`.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file()
    }

    /// The stored blob with `digest`, checked against it.
    pub fn blob(&self, digest: &str) -> Result<Vec<u8>, ImageError> {
        let bytes = std::fs::read(self.blob_path(digest))?;
        image::verify(digest, &bytes)?;
        Ok(bytes)
    }

    /// Adds a blob, refusing it unless it hashes to `digest`.
    pub fn put_blob(&self, digest: &str, bytes: &[u8]) -> Result<(), ImageError> {
        image::verify(digest, bytes)?;
        write_atomically(&self.blob_path(digest), bytes)?;
        Ok(())
    }

    fn index(&self) -> Result<Value, ImageError> {
        let text = std::fs::read(self.dir.join("index.json"))?;
        serde_json::from_slice(&text).map_err(|e| ImageError::Invalid(format!("store index.json: {e}")))
    }

    /// Names the manifest `digest` (already in the store) as image `name` for `os/arch`, replacing what had that name
    /// and platform before.
    pub fn tag(
        &self,
        name: &str,
        os: &str,
        arch: &str,
        media_type: &str,
        digest: &str,
        size: u64,
    ) -> Result<(), ImageError> {
        let entry = Entry {
            name: name.into(),
            os: os.into(),
            arch: arch.into(),
            media_type: media_type.into(),
            digest: digest.into(),
            size,
        };
        self.update(|entries| {
            entries.retain(|e| !(e.name == entry.name && e.os == entry.os && e.arch == entry.arch));
            entries.push(entry);
        })
    }

    /// Every entry in the store's index, in the order they were added.
    pub fn entries(&self) -> Result<Vec<Entry>, ImageError> {
        let index = self.index()?;
        Ok(index["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Entry::from_json)
            .collect())
    }

    /// Removes the entries `remove` picks and returns them.
    pub fn untag(&self, remove: impl Fn(&Entry) -> bool) -> Result<Vec<Entry>, ImageError> {
        let mut removed = Vec::new();
        self.update(|entries| {
            let (gone, kept): (Vec<_>, Vec<_>) = entries.drain(..).partition(|e| remove(e));
            *entries = kept;
            removed = gone;
        })?;
        Ok(removed)
    }

    /// Changes the index's entries under `index.lock`, so two commands updating it at once both take effect.
    fn update(&self, change: impl FnOnce(&mut Vec<Entry>)) -> Result<(), ImageError> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("index.lock"))?;
        lock.lock()?;
        let mut index = self.index()?;
        let mut entries = self.entries()?;
        change(&mut entries);
        index["manifests"] = Value::Array(entries.iter().map(Entry::to_json).collect());
        let text = serde_json::to_vec_pretty(&index).map_err(|e| ImageError::Io(e.to_string()))?;
        write_atomically(&self.dir.join("index.json"), &text)?;
        Ok(())
    }

    /// The blobs an entry needs: its manifest, then the config and layers it lists.
    pub fn blobs_of(&self, entry: &Entry) -> Result<Vec<(String, u64)>, ImageError> {
        let manifest: Value = serde_json::from_slice(&self.blob(&entry.digest)?)
            .map_err(|e| ImageError::Invalid(format!("manifest {}: {e}", entry.digest)))?;
        let mut blobs = vec![(entry.digest.clone(), entry.size)];
        let descriptors =
            std::iter::once(&manifest["config"]).chain(manifest["layers"].as_array().into_iter().flatten());
        for d in descriptors {
            if let Some(digest) = d["digest"].as_str() {
                blobs.push((digest.to_string(), d["size"].as_u64().unwrap_or(0)));
            }
        }
        Ok(blobs)
    }

    /// Deletes every blob no entry needs, returning their digests and total size. Needs [`Store::open_exclusive`],
    /// so no pull or load is halfway through adding blobs it has not named yet.
    pub fn collect_garbage(&self) -> Result<(Vec<String>, u64), ImageError> {
        let mut wanted = std::collections::HashSet::new();
        for entry in self.entries()? {
            for (digest, _) in self.blobs_of(&entry)? {
                wanted.insert(digest);
            }
        }
        let mut deleted = Vec::new();
        let mut freed = 0;
        for file in std::fs::read_dir(self.dir.join("blobs").join("sha256"))? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            // Only finished blobs: a write in progress is `<hex>.<pid>.tmp`.
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let digest = format!("sha256:{name}");
            if !wanted.contains(&digest) {
                freed += file.metadata()?.len();
                std::fs::remove_file(file.path())?;
                deleted.push(digest);
            }
        }
        // Layer disks go with their layers, and every disk of an older builder goes.
        let erofs = self.dir.join("erofs");
        if let Ok(formats) = std::fs::read_dir(&erofs) {
            let current = crate::erofs::FORMAT.to_string();
            for format in formats.flatten() {
                let Ok(disks) = std::fs::read_dir(format.path()) else {
                    continue;
                };
                for disk in disks.flatten() {
                    let name = disk.file_name().to_string_lossy().into_owned();
                    let hex = name.strip_suffix(".erofs").unwrap_or_default();
                    let in_use = format.file_name() == current.as_str()
                        && hex.len() == 64
                        && wanted.contains(&format!("sha256:{hex}"));
                    // A disk being written (`.tmp`) by another process is left alone.
                    if !in_use && !name.ends_with(".tmp") {
                        freed += disk.metadata().map(|m| m.len()).unwrap_or(0);
                        std::fs::remove_file(disk.path())?;
                    }
                }
                if format.file_name() != current.as_str() {
                    let _ = std::fs::remove_dir(format.path());
                }
            }
        }
        deleted.sort();
        Ok((deleted, freed))
    }

    /// The folder of layer disks the current builder writes: `erofs/<format>/`. Older formats' folders are left for
    /// [`Store::collect_garbage`].
    fn disks_dir(&self) -> PathBuf {
        self.dir.join("erofs").join(crate::erofs::FORMAT.to_string())
    }

    /// The EROFS disk of layer `digest` (#16): built from the stored blob the first time it is asked for, and kept
    /// under `erofs/<format>/<hex>.erofs` so every image with that layer reuses it. Written to a temporary name and
    /// renamed, so a half-written disk is never used.
    pub fn layer_disk(&self, digest: &str, media_type: &str) -> Result<PathBuf, ImageError> {
        // Checks the digest is a well-formed sha256 one, so the name below is plain hex.
        image::blob_path(digest)?;
        let hex = &digest["sha256:".len()..];
        let dir = self.disks_dir();
        let path = dir.join(format!("{hex}.erofs"));
        if path.is_file() {
            return Ok(path);
        }
        std::fs::create_dir_all(&dir)?;
        let blob = self.blob(digest)?;
        let temp = dir.join(format!("{hex}.erofs.{}.tmp", std::process::id()));
        let built = std::fs::File::create(&temp).map_err(ImageError::from).and_then(|file| {
            let mut out = std::io::BufWriter::new(file);
            crate::erofs::build(image::layer_reader(media_type, &blob)?, &mut out)
                .map_err(|e| ImageError::Invalid(format!("layer {digest}: {e}")))?;
            out.into_inner()
                .map_err(|e| ImageError::from(e.into_error()))?
                .sync_all()?;
            Ok(())
        });
        if let Err(e) = built.and_then(|()| std::fs::rename(&temp, &path).map_err(ImageError::from)) {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        Ok(path)
    }

    /// Whether the store has image `name` for `os/arch`.
    #[cfg(test)]
    pub fn has(&self, name: &str, os: &str, arch: &str) -> bool {
        self.entries()
            .unwrap_or_default()
            .iter()
            .any(|e| e.name == name && e.os == os && e.arch == arch)
    }
}

/// What a store holds, for `machine info`.
#[derive(Debug, PartialEq, Eq)]
pub struct Usage {
    /// Entries in `index.json`: one per name and platform.
    pub entries: usize,
    /// Distinct image names.
    pub names: usize,
    /// The blobs' total size.
    pub bytes: u64,
    /// The layer disks' total size (#16), built from the blobs.
    pub disk_bytes: u64,
}

/// What the store under `data_dir` holds, read without creating anything; `None` if there is no store there yet.
pub fn usage(data_dir: &Path) -> Option<Usage> {
    let dir = data_dir.join("images");
    let index: Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
    let manifests = index["manifests"].as_array().cloned().unwrap_or_default();
    let mut names: Vec<_> = manifests
        .iter()
        .filter_map(|m| m["annotations"][REF_NAME].as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    let bytes = std::fs::read_dir(dir.join("blobs").join("sha256"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.metadata().ok())
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    let disk_bytes = std::fs::read_dir(dir.join("erofs"))
        .map(|formats| {
            formats
                .flatten()
                .filter_map(|f| std::fs::read_dir(f.path()).ok())
                .flat_map(|disks| disks.flatten().filter_map(|d| d.metadata().ok()).map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0);
    Some(Usage {
        entries: manifests.len(),
        names: names.len(),
        bytes,
        disk_bytes,
    })
}

fn lock_file(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
}

/// Writes `bytes` to a temporary file beside `path`, then renames it into place, so a reader never sees half a file.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Image;
    use crate::image::testing::{layer, manifest};
    use crate::qemu::GuestArch;
    use crate::qemu::testing::scratch;

    #[test]
    fn layer_disks_are_built_once_and_collected_with_their_layer() {
        let dir = scratch("store-disks");
        let store = Store::open(&dir).unwrap();
        let (digest, _) = manifest(&dir.join("images"), "amd64", &layer(), "{}");
        store
            .tag("example.com/app:1", "linux", "amd64", "m", &digest, 2)
            .unwrap();
        let blobs = store.blobs_of(&store.entries().unwrap()[0]).unwrap();
        let (layer_digest, _) = blobs
            .iter()
            .find(|(d, _)| d != &digest && store.blob(d).unwrap().starts_with(&[0x1f, 0x8b]))
            .unwrap();
        let disk = store
            .layer_disk(layer_digest, "application/vnd.oci.image.layer.v1.tar+gzip")
            .unwrap();
        assert!(disk.ends_with(format!("erofs/{}/{}.erofs", crate::erofs::FORMAT, &layer_digest[7..])));
        let image = crate::erofs::reader::Disk::open_path(&disk).unwrap();
        assert!(
            image.lookup(&[b"bin", b"sh"]).unwrap().is_some(),
            "the layer's files are on the disk"
        );
        // A second ask reuses the disk.
        let built = std::fs::metadata(&disk).unwrap().modified().unwrap();
        assert_eq!(store.layer_disk(layer_digest, "").unwrap(), disk);
        assert_eq!(std::fs::metadata(&disk).unwrap().modified().unwrap(), built);
        assert!(usage(&dir).unwrap().disk_bytes > 0);
        assert!(store.layer_disk("sha256:nothex", "").is_err());

        // An older builder's disk goes at the next collection, the current one stays while its layer does.
        let old = dir.join("images/erofs/0");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(format!("{}.erofs", &layer_digest[7..])), b"old").unwrap();
        store.collect_garbage().unwrap();
        assert!(disk.is_file() && !old.exists());
        assert_eq!(store.untag(|e| e.name == "example.com/app:1").unwrap().len(), 1);
        store.collect_garbage().unwrap();
        assert!(!disk.exists());
        assert_eq!(usage(&dir).unwrap().disk_bytes, 0);
    }

    #[test]
    fn the_data_dir_follows_each_platform() {
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/me")),
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\me\AppData\Local")),
            _ => None,
        };
        assert_eq!(
            data_dir(Some("/elsewhere".into()), env),
            Some(PathBuf::from("/elsewhere"))
        );
        let expected = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\AppData\Local").join("oci-hermit")
        } else if cfg!(target_os = "macos") {
            PathBuf::from("/home/me/Library/Application Support/oci-hermit")
        } else {
            PathBuf::from("/home/me/.local/share/oci-hermit")
        };
        assert_eq!(data_dir(Some("".into()), env), Some(expected));
        assert_eq!(data_dir(None, |_| None), None);
        if cfg!(all(unix, not(target_os = "macos"))) {
            let xdg = |name: &str| (name == "XDG_DATA_HOME").then(|| OsString::from("/xdg"));
            assert_eq!(data_dir(None, xdg), Some(PathBuf::from("/xdg/oci-hermit")));
        }
    }

    #[test]
    fn stores_verified_blobs_and_names_images() {
        let data = scratch("store-basic");
        let store = Store::open(&data).unwrap();
        // A second open keeps what is there.
        let store = {
            drop(store);
            Store::open(&data).unwrap()
        };
        assert!(!store.has("example.com/app:1", "linux", "amd64"));

        // Build an image's blobs elsewhere, then copy them in through put_blob.
        let source = scratch("store-basic-source");
        let config = r#"{"architecture":"ARCH","config":{"Cmd":["one"]}}"#;
        let (digest, bytes) = manifest(&source, "amd64", &layer(), config);
        for entry in std::fs::read_dir(source.join("blobs/sha256")).unwrap() {
            let path = entry.unwrap().path();
            let name = format!("sha256:{}", path.file_name().unwrap().to_string_lossy());
            store.put_blob(&name, &std::fs::read(&path).unwrap()).unwrap();
            assert!(store.has_blob(&name));
        }
        let media = "application/vnd.oci.image.manifest.v1+json";
        store
            .tag(
                "example.com/app:1",
                "linux",
                "amd64",
                media,
                &digest,
                bytes.len() as u64,
            )
            .unwrap();
        assert!(store.has("example.com/app:1", "linux", "amd64"));
        assert!(!store.has("example.com/app:1", "linux", "arm64"));
        let image = Image::open_named(store.path(), Some("example.com/app:1"), GuestArch::X86_64).unwrap();
        assert_eq!(image.config.cmd, Some(vec!["one".to_string()]));

        // Tagging again replaces the entry rather than adding a second one.
        store
            .tag(
                "example.com/app:1",
                "linux",
                "amd64",
                media,
                &digest,
                bytes.len() as u64,
            )
            .unwrap();
        let index = std::fs::read_to_string(store.path().join("index.json")).unwrap();
        assert_eq!(index.matches("example.com/app:1").count(), 1);

        // Another platform under the same name is a second entry, not a second name.
        store
            .tag(
                "example.com/app:1",
                "linux",
                "arm64",
                media,
                &digest,
                bytes.len() as u64,
            )
            .unwrap();
        let usage = usage(&data).unwrap();
        assert_eq!((usage.entries, usage.names), (2, 1));
        let blobs: u64 = std::fs::read_dir(source.join("blobs/sha256"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(usage.bytes, blobs);
    }

    #[test]
    fn deleting_waits_for_no_one_else_to_have_the_store_open() {
        let data = scratch("store-lock");
        let now = std::time::Duration::ZERO;
        let shared = Store::open(&data).unwrap();
        let busy = Store::open_exclusive(&data, now).err().unwrap();
        assert!(busy.to_string().contains("in use by another oci-hermit"), "{busy}");
        // Readers and writers share it.
        let second = Store::open(&data).unwrap();
        drop((shared, second));
        let alone = Store::open_exclusive(&data, now).unwrap();
        assert!(Store::open_exclusive(&data, now).is_err());
        drop(alone);
        assert!(Store::open_exclusive(&data, now).is_ok());

        // A holder that lets go within the patience is waited for.
        let brief = Store::open(&data).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(brief);
        });
        assert!(Store::open_exclusive(&data, std::time::Duration::from_secs(10)).is_ok());
        holder.join().unwrap();
    }

    #[test]
    fn garbage_is_what_no_entry_needs() {
        let data = scratch("store-gc");
        let store = Store::open(&data).unwrap();
        let source = scratch("store-gc-source");
        let (digest, bytes) = manifest(&source, "amd64", &layer(), r#"{"architecture":"ARCH"}"#);
        for entry in std::fs::read_dir(source.join("blobs/sha256")).unwrap() {
            let path = entry.unwrap().path();
            let name = format!("sha256:{}", path.file_name().unwrap().to_string_lossy());
            store.put_blob(&name, &std::fs::read(&path).unwrap()).unwrap();
        }
        let media = "application/vnd.oci.image.manifest.v1+json";
        store
            .tag("a:1", "linux", "amd64", media, &digest, bytes.len() as u64)
            .unwrap();
        store
            .tag("b:1", "linux", "amd64", media, &digest, bytes.len() as u64)
            .unwrap();
        let stray = image::digest_of(b"left by an interrupted pull");
        store.put_blob(&stray, b"left by an interrupted pull").unwrap();
        let blobs = store.path().join("blobs/sha256");
        std::fs::write(blobs.join("abc.123.tmp"), b"a write in progress").unwrap();

        let (deleted, freed) = store.collect_garbage().unwrap();
        assert_eq!(deleted, std::slice::from_ref(&stray));
        assert_eq!(freed, 27);
        assert!(blobs.join("abc.123.tmp").exists());
        assert_eq!(store.blobs_of(&store.entries().unwrap()[0]).unwrap().len(), 3);

        // Untagging one name keeps the blobs for the other.
        let removed = store.untag(|e| e.name == "a:1").unwrap();
        assert_eq!(removed.len(), 1);
        assert!(store.collect_garbage().unwrap().0.is_empty());
        store.untag(|_| true).unwrap();
        assert_eq!(store.collect_garbage().unwrap().0.len(), 3);
        assert!(store.entries().unwrap().is_empty());
    }

    #[test]
    fn usage_creates_nothing() {
        let data = scratch("store-usage-none");
        assert_eq!(usage(&data), None);
        assert!(!data.join("images").exists());
    }

    #[test]
    fn refuses_a_blob_that_does_not_match() {
        let store = Store::open(&scratch("store-bad")).unwrap();
        let wrong = image::digest_of(b"something else");
        let err = store.put_blob(&wrong, b"payload").err().unwrap();
        assert!(matches!(err, ImageError::Mismatch(_)), "{err}");
        assert!(!store.has_blob(&wrong));
        assert!(store.put_blob("sha256:../../x", b"").is_err());
    }
}
