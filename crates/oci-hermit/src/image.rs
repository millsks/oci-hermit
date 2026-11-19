//! Reading an OCI image from an image layout: a directory, or a `.tar` archive of one (what `pixi oci build` and
//! `pixi oci pull` write).
//!
//! The manifest for the guest's architecture is picked from `index.json` (following nested indexes), and its
//! config and layers are read from `blobs/`. Layers may be plain or gzip tarballs. Every blob is checked against
//! its sha256 digest as it is read, so a corrupt or altered image is an error, never a sandbox. A layout can hold
//! several images, told apart by their `org.opencontainers.image.ref.name` annotation; the image store (`store`) is
//! one such layout.

use crate::qemu::GuestArch;
use crate::rootfs::{LayerError, Rootfs};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
/// The annotation that names an image in a layout's `index.json`.
pub const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Why an image could not be read.
#[derive(Debug)]
pub enum ImageError {
    Io(String),
    /// The layout is missing a file or holds one that is not what the OCI spec says.
    Invalid(String),
    /// No manifest in the image is for the requested architecture.
    NoPlatform {
        arch: GuestArch,
        found: Vec<String>,
    },
    /// A layer's media type cannot be read yet.
    Unsupported(String),
    Layer(LayerError),
    /// A blob's contents do not hash to its digest.
    Mismatch(String),
    /// The layout holds no image with this name.
    NotFound(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "{e}"),
            ImageError::Invalid(what) => write!(f, "not a valid OCI image layout: {what}"),
            ImageError::NoPlatform { arch, found } => write!(
                f,
                "the image has no linux/{} variant (it has: {})",
                oci_arch(*arch),
                if found.is_empty() {
                    "none listed".into()
                } else {
                    found.join(", ")
                }
            ),
            ImageError::Unsupported(what) => write!(f, "{what} is not supported yet"),
            ImageError::Layer(e) => write!(f, "{e}"),
            ImageError::Mismatch(digest) => write!(
                f,
                "blob {digest} does not match its digest: the image is corrupt or has been altered"
            ),
            ImageError::NotFound(name) => write!(f, "no image named {name}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<io::Error> for ImageError {
    fn from(e: io::Error) -> Self {
        ImageError::Io(e.to_string())
    }
}

impl From<LayerError> for ImageError {
    fn from(e: LayerError) -> Self {
        ImageError::Layer(e)
    }
}

/// The OCI name of a guest architecture.
pub fn oci_arch(arch: GuestArch) -> &'static str {
    match arch {
        GuestArch::X86_64 => "amd64",
        GuestArch::Aarch64 => "arm64",
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

/// The uncompressed tarball of a layer blob: gzip (by media type or by its magic bytes) or plain. zstd is not read
/// yet.
pub fn layer_reader<'a>(media_type: &str, blob: &'a [u8]) -> Result<Box<dyn std::io::Read + 'a>, ImageError> {
    if media_type.ends_with("+zstd") || media_type.ends_with(".zstd") {
        return Err(ImageError::Unsupported(format!("a zstd layer ({media_type})")));
    }
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") || blob.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(blob)))
    } else {
        Ok(Box::new(blob))
    }
}

/// `sha256:<hex>` of `bytes`.
pub fn digest_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(bytes);
    let mut digest = String::from("sha256:");
    for byte in hash.iter() {
        digest.push_str(&format!("{byte:02x}"));
    }
    digest
}

/// Checks that `bytes` hash to `digest`, which must be a well-formed sha256 digest.
pub fn verify(digest: &str, bytes: &[u8]) -> Result<(), ImageError> {
    blob_path(digest)?;
    if digest_of(bytes).eq_ignore_ascii_case(digest) {
        Ok(())
    } else {
        Err(ImageError::Mismatch(digest.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Debug, Deserialize)]
struct Index {
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    config: RunConfig,
}

/// What an image says to run, from its config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RunConfig {
    #[serde(rename = "Entrypoint", default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default)]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Env", default)]
    pub env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    pub working_dir: Option<String>,
    #[serde(rename = "User", default)]
    pub user: Option<String>,
}

impl RunConfig {
    /// The command line: the entrypoint, then `command` if one was given, else the image's `Cmd`.
    pub fn argv(&self, command: &[String]) -> Vec<String> {
        let mut argv = self.entrypoint.clone().unwrap_or_default();
        if command.is_empty() {
            argv.extend(self.cmd.clone().unwrap_or_default());
        } else {
            argv.extend(command.iter().cloned());
        }
        argv
    }

    pub fn working_dir(&self) -> String {
        match self.working_dir.as_deref() {
            Some(dir) if !dir.is_empty() => dir.to_string(),
            _ => "/".into(),
        }
    }
}

/// Where the layout's files come from.
pub(crate) enum Source {
    Dir(PathBuf),
    /// A tar archive's files, read whole: images for smoke tests are small enough.
    Archive(HashMap<String, Vec<u8>>),
}

impl Source {
    /// The files of a directory, or of a tar archive (read whole).
    pub(crate) fn open(path: &Path) -> Result<Source, ImageError> {
        if path.is_dir() {
            return Ok(Source::Dir(path.to_path_buf()));
        }
        let mut files = HashMap::new();
        let mut archive = tar::Archive::new(File::open(path)?);
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let name = String::from_utf8_lossy(&entry.path_bytes())
                .trim_start_matches("./")
                .to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            files.insert(name, data);
        }
        Ok(Source::Archive(files))
    }

    /// Whether the file at `path` exists.
    pub(crate) fn has(&self, path: &str) -> bool {
        match self {
            Source::Dir(dir) => safe(path).is_ok() && dir.join(path).is_file(),
            Source::Archive(files) => files.contains_key(path),
        }
    }

    /// The file at `path`, a relative path that must stay inside the layout.
    pub(crate) fn read(&self, path: &str) -> Result<Vec<u8>, ImageError> {
        safe(path)?;
        match self {
            Source::Dir(dir) => std::fs::read(dir.join(path)).map_err(|e| ImageError::Invalid(format!("{path}: {e}"))),
            Source::Archive(files) => files
                .get(path)
                .cloned()
                .ok_or_else(|| ImageError::Invalid(format!("{path} is missing"))),
        }
    }

    /// The blob with `digest`, checked against it.
    pub(crate) fn read_blob(&self, digest: &str) -> Result<Vec<u8>, ImageError> {
        let bytes = self.read(&blob_path(digest)?)?;
        verify(digest, &bytes)?;
        Ok(bytes)
    }
}

/// Refuses a path that is absolute or climbs out with `..`: archive manifests name files, and they are not trusted.
fn safe(path: &str) -> Result<(), ImageError> {
    if path.starts_with('/') || path.contains('\\') || path.split('/').any(|part| part == "..") {
        return Err(ImageError::Invalid(format!("{path:?} points outside the image")));
    }
    Ok(())
}

pub(crate) fn blob_path(digest: &str) -> Result<String, ImageError> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| ImageError::Unsupported(format!("digest {digest:?} (only sha256)")))?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ImageError::Invalid(format!("malformed digest {digest:?}")));
    }
    Ok(format!("blobs/sha256/{hex}"))
}

fn json<T: for<'a> Deserialize<'a>>(what: &str, bytes: &[u8]) -> Result<T, ImageError> {
    serde_json::from_slice(bytes).map_err(|e| ImageError::Invalid(format!("{what}: {e}")))
}

/// An image opened for one architecture.
pub struct Image {
    source: Source,
    id: String,
    layers: Vec<Descriptor>,
    pub config: RunConfig,
}

impl Image {
    /// Open the image at `path` (a layout directory or a `.tar` of one) for `arch`.
    pub fn open(path: &Path, arch: GuestArch) -> Result<Image, ImageError> {
        Image::open_named(path, None, arch)
    }

    /// Open the image named `name` in the layout at `path`, or its only image when `name` is `None`, for `arch`.
    pub fn open_named(path: &Path, name: Option<&str>, arch: GuestArch) -> Result<Image, ImageError> {
        let source = Source::open(path)?;
        let layout = source.read("oci-layout")?;
        if !String::from_utf8_lossy(&layout).contains("imageLayoutVersion") {
            return Err(ImageError::Invalid("oci-layout has no imageLayoutVersion".into()));
        }
        let mut index: Index = json("index", &source.read("index.json")?)?;
        if let Some(name) = name {
            index
                .manifests
                .retain(|d| d.annotations.get(REF_NAME).is_some_and(|n| n == name));
            if index.manifests.is_empty() {
                return Err(ImageError::NotFound(name.to_string()));
            }
        }
        let manifest = Image::manifest_for(&source, index, arch, 0)?;
        let config_file: ConfigFile = json("image config", &source.read_blob(&manifest.config.digest)?)?;
        if !config_file.architecture.is_empty() && config_file.architecture != oci_arch(arch) {
            return Err(ImageError::NoPlatform {
                arch,
                found: vec![format!("linux/{}", config_file.architecture)],
            });
        }
        Ok(Image {
            source,
            id: manifest.config.digest,
            layers: manifest.layers,
            config: config_file.config,
        })
    }

    /// The manifest in `index` for `arch`, following nested indexes.
    fn manifest_for(source: &Source, index: Index, arch: GuestArch, depth: usize) -> Result<Manifest, ImageError> {
        if depth > 4 {
            return Err(ImageError::Invalid("indexes nested too deep".into()));
        }
        let wanted = oci_arch(arch);
        let found: Vec<String> = index
            .manifests
            .iter()
            .filter_map(|d| d.platform.as_ref().map(|p| format!("{}/{}", p.os, p.architecture)))
            .collect();
        let pick = index
            .manifests
            .iter()
            .find(|d| {
                d.platform
                    .as_ref()
                    .is_some_and(|p| p.os == "linux" && p.architecture == wanted)
            })
            .or(match index.manifests.as_slice() {
                // A single manifest without a platform: its config says which architecture it is.
                [only] if only.platform.is_none() => Some(only),
                _ => None,
            })
            .ok_or(ImageError::NoPlatform { arch, found })?;
        let bytes = source.read_blob(&pick.digest)?;
        if pick.media_type == INDEX || pick.media_type == DOCKER_LIST {
            return Image::manifest_for(source, json("index", &bytes)?, arch, depth + 1);
        }
        json("manifest", &bytes)
    }

    /// The config's digest, which Docker and Podman call the image ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The layers, bottom first, as `(digest, media type)`.
    pub fn layers(&self) -> Vec<(&str, &str)> {
        self.layers
            .iter()
            .map(|l| (l.digest.as_str(), l.media_type.as_str()))
            .collect()
    }

    /// The root filesystem: every layer applied in order.
    pub fn rootfs(&self) -> Result<Rootfs, ImageError> {
        let mut rootfs = Rootfs::new();
        for layer in &self.layers {
            let blob = self.source.read_blob(&layer.digest)?;
            rootfs.apply_layer(layer_reader(&layer.media_type, &blob)?)?;
        }
        Ok(rootfs)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::rootfs::testing::Layer;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// Writes `bytes` as a blob of the layout in `dir` and returns its digest.
    pub(crate) fn blob(dir: &Path, bytes: &[u8]) -> String {
        let digest = super::digest_of(bytes);
        std::fs::write(dir.join("blobs/sha256").join(&digest[7..]), bytes).unwrap();
        digest
    }

    /// An image layout directory with one manifest per `(arch, layer)` and the given config.
    pub(crate) fn layout(dir: &Path, images: &[(&str, Vec<u8>)], config: &str) -> PathBuf {
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let mut manifests = Vec::new();
        for (arch, layer) in images {
            let (manifest, _) = manifest(dir, arch, layer, config);
            manifests.push(format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{manifest}","platform":{{"os":"linux","architecture":"{arch}"}}}}"#
            ));
        }
        std::fs::write(
            dir.join("index.json"),
            format!(r#"{{"schemaVersion":2,"manifests":[{}]}}"#, manifests.join(",")),
        )
        .unwrap();
        dir.to_path_buf()
    }

    /// Writes a config, a gzip layer and a manifest for `arch` into the layout in `dir`; returns the manifest's
    /// digest and bytes.
    pub(crate) fn manifest(dir: &Path, arch: &str, layer: &[u8], config: &str) -> (String, Vec<u8>) {
        std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(layer).unwrap();
        let gz = gz.finish().unwrap();
        let config = config.replace("ARCH", arch);
        let (config_digest, layer_digest) = (blob(dir, config.as_bytes()), blob(dir, &gz));
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{}}}]}}"#,
            config.len(),
            gz.len()
        )
        .into_bytes();
        (blob(dir, &manifest), manifest)
    }

    /// The same layout, packed into a tar archive next to it.
    pub(crate) fn archive(dir: &Path) -> PathBuf {
        let tar_path = dir.with_extension("tar");
        let mut builder = tar::Builder::new(std::fs::File::create(&tar_path).unwrap());
        builder.append_dir_all(".", dir).unwrap();
        builder.finish().unwrap();
        tar_path
    }

    /// A small debian-like layer.
    pub(crate) fn layer() -> Vec<u8> {
        Layer::new()
            .dir("bin")
            .file("bin/sh", b"#!elf")
            .file("etc/os-release", b"ID=test")
            .bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{archive, layer, layout};
    use super::*;
    use crate::qemu::testing::scratch;

    const CONFIG: &str =
        r#"{"architecture":"ARCH","os":"linux","config":{"Cmd":["bash"],"Env":["PATH=/bin"],"WorkingDir":"/work"}}"#;

    #[test]
    fn reads_the_manifest_for_the_guest_arch_from_a_directory_and_an_archive() {
        let dir = layout(&scratch("img-multi"), &[("amd64", layer()), ("arm64", layer())], CONFIG);
        for path in [dir.clone(), archive(&dir)] {
            let image = Image::open(&path, GuestArch::Aarch64).unwrap();
            assert_eq!(image.config.cmd, Some(vec!["bash".to_string()]));
            assert_eq!(image.config.working_dir(), "/work");
            let rootfs = image.rootfs().unwrap();
            assert!(rootfs.get("bin/sh").is_some());
            assert!(rootfs.get("etc/os-release").is_some());
        }
    }

    #[test]
    fn says_which_platforms_an_image_has() {
        let dir = layout(&scratch("img-amd64"), &[("amd64", layer())], CONFIG);
        let err = Image::open(&dir, GuestArch::Aarch64).err().unwrap();
        assert_eq!(
            err.to_string(),
            "the image has no linux/arm64 variant (it has: linux/amd64)"
        );
    }

    #[test]
    fn rejects_what_is_not_a_layout() {
        let dir = scratch("img-empty");
        let err = Image::open(&dir, GuestArch::X86_64).err().unwrap();
        assert!(err.to_string().starts_with("not a valid OCI image layout"), "{err}");
        assert!(Image::open(&dir.join("absent.tar"), GuestArch::X86_64).is_err());
        assert!(matches!(blob_path("md5:00"), Err(ImageError::Unsupported(_))));
        assert!(matches!(blob_path("sha256:../../etc"), Err(ImageError::Invalid(_))));
    }

    #[test]
    fn a_blob_that_does_not_match_its_digest_is_refused() {
        let dir = layout(&scratch("img-tampered"), &[("amd64", layer())], CONFIG);
        let index = std::fs::read_to_string(dir.join("index.json")).unwrap();
        let manifest = index.split("sha256:").nth(1).unwrap()[..64].to_string();
        let manifest: String = std::fs::read_to_string(dir.join("blobs/sha256").join(&manifest)).unwrap();
        let layer = manifest.rsplit("sha256:").next().unwrap()[..64].to_string();
        let path = dir.join("blobs/sha256").join(&layer);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let image = Image::open(&dir, GuestArch::X86_64).unwrap();
        let err = image.rootfs().err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("blob sha256:{layer} does not match its digest: the image is corrupt or has been altered")
        );
        assert!(verify("sha256:00", b"").is_err());
        assert_eq!(
            digest_of(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_layout_can_hold_images_by_name() {
        let dir = scratch("img-named");
        std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let mut entries = Vec::new();
        for (name, cmd) in [("example.com/a:1", "first"), ("example.com/b:1", "second")] {
            let config = format!(r#"{{"architecture":"ARCH","config":{{"Cmd":["{cmd}"]}}}}"#);
            let (digest, _) = super::testing::manifest(&dir, "amd64", &layer(), &config);
            entries.push(format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{digest}","platform":{{"os":"linux","architecture":"amd64"}},"annotations":{{"{REF_NAME}":"{name}"}}}}"#
            ));
        }
        std::fs::write(
            dir.join("index.json"),
            format!(r#"{{"manifests":[{}]}}"#, entries.join(",")),
        )
        .unwrap();
        let open = |name| Image::open_named(&dir, Some(name), GuestArch::X86_64);
        assert_eq!(
            open("example.com/b:1").unwrap().config.cmd,
            Some(vec!["second".to_string()])
        );
        assert_eq!(
            open("example.com/a:1").unwrap().config.cmd,
            Some(vec!["first".to_string()])
        );
        assert_eq!(
            open("example.com/c:1").err().unwrap().to_string(),
            "no image named example.com/c:1"
        );
    }

    #[test]
    fn the_command_follows_docker_rules() {
        let config = RunConfig {
            entrypoint: Some(vec!["/entry".into()]),
            cmd: Some(vec!["default".into()]),
            ..RunConfig::default()
        };
        assert_eq!(config.argv(&[]), ["/entry", "default"]);
        assert_eq!(config.argv(&["sh".into()]), ["/entry", "sh"]);
        assert_eq!(RunConfig::default().argv(&[]), Vec::<String>::new());
        assert_eq!(RunConfig::default().working_dir(), "/");
    }
}
