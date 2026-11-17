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
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store under `data_dir`, created if it does not exist yet.
    pub fn open(data_dir: &Path) -> std::io::Result<Store> {
        let dir = data_dir.join("images");
        std::fs::create_dir_all(dir.join("blobs").join("sha256"))?;
        let layout = dir.join("oci-layout");
        if !layout.exists() {
            std::fs::write(&layout, r#"{"imageLayoutVersion":"1.0.0"}"#)?;
        }
        if !dir.join("index.json").exists() {
            write_atomically(&dir.join("index.json"), br#"{"schemaVersion":2,"manifests":[]}"#)?;
        }
        Ok(Store { dir })
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
        let mut index = self.index()?;
        let manifests = index["manifests"]
            .as_array_mut()
            .ok_or_else(|| ImageError::Invalid("store index.json has no manifests".into()))?;
        manifests.retain(|m| {
            !(m["annotations"][REF_NAME] == name && m["platform"]["os"] == os && m["platform"]["architecture"] == arch)
        });
        manifests.push(json!({
            "mediaType": media_type,
            "digest": digest,
            "size": size,
            "platform": { "os": os, "architecture": arch },
            "annotations": { REF_NAME: name },
        }));
        let text = serde_json::to_vec_pretty(&index).map_err(|e| ImageError::Io(e.to_string()))?;
        write_atomically(&self.dir.join("index.json"), &text)?;
        Ok(())
    }

    /// Whether the store has image `name` for `os/arch`.
    pub fn has(&self, name: &str, os: &str, arch: &str) -> bool {
        self.index()
            .ok()
            .and_then(|index| index["manifests"].as_array().cloned())
            .is_some_and(|manifests| {
                manifests.iter().any(|m| {
                    m["annotations"][REF_NAME] == name
                        && m["platform"]["os"] == os
                        && m["platform"]["architecture"] == arch
                })
            })
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
    Some(Usage {
        entries: manifests.len(),
        names: names.len(),
        bytes,
    })
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
