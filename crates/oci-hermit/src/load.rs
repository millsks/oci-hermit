//! `load`: images from `docker save` archives and OCI image layouts (a directory or a `.tar`) into the [`Store`],
//! fully offline.
//!
//! An OCI layout is copied as it is, every platform it holds, each blob checked against its digest. Docker's own
//! format (`manifest.json` naming a config and layer files) is turned into OCI manifests on the way in; Docker 25 and
//! later, and pixi-oci, write both, and the OCI side is used. Images are named as Docker names them; one with no name
//! is stored under its ID (the config digest), which `run` accepts too.

use crate::image::{self, ImageError, REF_NAME, Source};
use crate::registry;
use crate::store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
/// The full image name Docker and containerd put on `index.json` entries; `ref.name` often holds only the tag.
const CONTAINERD_NAME: &str = "io.containerd.image.name";

/// One image `load` stored.
#[derive(Debug, PartialEq, Eq)]
pub struct Loaded {
    /// The full reference, or the image ID when the archive gave no name.
    pub name: String,
    /// `os/architecture`.
    pub platform: String,
}

fn parse(what: &str, bytes: &[u8]) -> Result<Value, ImageError> {
    serde_json::from_slice(bytes).map_err(|e| ImageError::Invalid(format!("{what}: {e}")))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

/// Load every image in the archive or layout at `path` into `store`, reporting each on `out`.
pub fn load(path: &Path, store: &Store, out: &mut impl Write) -> Result<Vec<Loaded>, ImageError> {
    let source = Source::open(path)?;
    // Docker's manifest.json, when there is one, names images by their config digest.
    let docker = match source.has("manifest.json") {
        true => Some(parse("manifest.json", &source.read("manifest.json")?)?),
        false => None,
    };
    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    for entry in docker.as_ref().and_then(Value::as_array).into_iter().flatten() {
        let config = text(entry, "Config");
        let digest = format!(
            "sha256:{}",
            config.rsplit('/').next().unwrap_or_default().trim_end_matches(".json")
        );
        let names = entry["RepoTags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        tags.entry(digest).or_default().extend(names.map(String::from));
    }
    let loaded = if source.has("index.json") && source.has("oci-layout") {
        load_layout(&source, store, &tags)?
    } else if let Some(docker) = &docker {
        load_docker(&source, docker, store)?
    } else {
        return Err(ImageError::Invalid(
            "neither an OCI image layout (index.json) nor a docker save archive (manifest.json)".into(),
        ));
    };
    if loaded.is_empty() {
        return Err(ImageError::Invalid("the archive holds no images".into()));
    }
    for image in &loaded {
        let kind = if image.name.starts_with("sha256:") {
            "Loaded image ID"
        } else {
            "Loaded image"
        };
        let _ = writeln!(out, "{kind}: {} ({})", image.name, image.platform);
    }
    Ok(loaded)
}

/// Copies blobs from `source` into `store`, checking each against its digest.
fn copy_blobs(source: &Source, store: &Store, digests: &[&str]) -> Result<(), ImageError> {
    for digest in digests {
        if !store.has_blob(digest) {
            store.put_blob(digest, &source.read_blob(digest)?)?;
        }
    }
    Ok(())
}

/// The names to store an image under: the full name from annotations or `docker save`, else a `ref.name` that is a
/// whole reference, else none.
fn names(annotations: &Value, config: &str, tags: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    names.extend(annotations[CONTAINERD_NAME].as_str().map(String::from));
    names.extend(tags.get(config).into_iter().flatten().cloned());
    if names.is_empty()
        && let Some(reference) = annotations[REF_NAME].as_str().filter(|r| r.contains('/'))
    {
        names.push(reference.to_string());
    }
    let mut full: Vec<String> = names
        .iter()
        .filter_map(|n| registry::normalize(n).ok().map(|r| r.whole()))
        .collect();
    full.dedup();
    full
}

fn load_layout(source: &Source, store: &Store, tags: &HashMap<String, Vec<String>>) -> Result<Vec<Loaded>, ImageError> {
    // (manifest descriptor, annotations inherited from an enclosing index)
    let mut pending: Vec<(Value, Value)> = Vec::new();
    let index = parse("index.json", &source.read("index.json")?)?;
    for entry in index["manifests"].as_array().into_iter().flatten() {
        pending.push((entry.clone(), entry["annotations"].clone()));
    }
    let mut loaded = Vec::new();
    let mut seen = 0;
    while let Some((descriptor, annotations)) = pending.pop() {
        seen += 1;
        if seen > 256 {
            return Err(ImageError::Invalid("too many manifests".into()));
        }
        let digest = text(&descriptor, "digest");
        let bytes = source.read_blob(digest)?;
        let manifest = parse("manifest", &bytes)?;
        let media_type = match text(&descriptor, "mediaType") {
            "" => text(&manifest, "mediaType"),
            given => given,
        };
        if media_type == OCI_INDEX || media_type == DOCKER_LIST || manifest["manifests"].is_array() {
            for entry in manifest["manifests"].as_array().into_iter().flatten() {
                let inherited = if entry["annotations"].is_object() {
                    &entry["annotations"]
                } else {
                    &annotations
                };
                pending.push((entry.clone(), inherited.clone()));
            }
            continue;
        }
        let config = text(&manifest["config"], "digest");
        let mut blobs = vec![config];
        blobs.extend(
            manifest["layers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| text(l, "digest")),
        );
        copy_blobs(source, store, &blobs)?;
        store.put_blob(digest, &bytes)?;
        let (os, arch) = match &descriptor["platform"] {
            Value::Object(p) => (
                p.get("os").and_then(Value::as_str).unwrap_or("linux").to_string(),
                p.get("architecture")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => {
                let config = parse("image config", &source.read_blob(config)?)?;
                (
                    text(&config, "os").to_string(),
                    text(&config, "architecture").to_string(),
                )
            }
        };
        let mut names = names(&annotations, config, tags);
        if names.is_empty() {
            names.push(config.to_string());
        }
        let media_type = if media_type.is_empty() {
            OCI_MANIFEST
        } else {
            media_type
        };
        for name in names {
            store.tag(&name, &os, &arch, media_type, digest, bytes.len() as u64)?;
            loaded.push(Loaded {
                name,
                platform: format!("{os}/{arch}"),
            });
        }
    }
    loaded.reverse();
    Ok(loaded)
}

/// Docker's format: `manifest.json` lists each image's config file, tags and layer files.
fn load_docker(source: &Source, docker: &Value, store: &Store) -> Result<Vec<Loaded>, ImageError> {
    let mut loaded = Vec::new();
    for entry in docker.as_array().into_iter().flatten() {
        let config_bytes = source.read(text(entry, "Config"))?;
        let config_digest = image::digest_of(&config_bytes);
        let config = parse("image config", &config_bytes)?;
        store.put_blob(&config_digest, &config_bytes)?;
        let mut layers = Vec::new();
        for path in entry["Layers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let bytes = source.read(path)?;
            let digest = image::digest_of(&bytes);
            let media_type = if bytes.starts_with(&[0x1f, 0x8b]) {
                "application/vnd.oci.image.layer.v1.tar+gzip"
            } else {
                "application/vnd.oci.image.layer.v1.tar"
            };
            store.put_blob(&digest, &bytes)?;
            layers.push(json!({ "mediaType": media_type, "digest": digest, "size": bytes.len() }));
        }
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len(),
            },
            "layers": layers,
        }))
        .map_err(|e| ImageError::Io(e.to_string()))?;
        let digest = image::digest_of(&manifest);
        store.put_blob(&digest, &manifest)?;
        let (os, arch) = (text(&config, "os"), text(&config, "architecture"));
        let tags = HashMap::from([(
            config_digest.clone(),
            entry["RepoTags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
        )]);
        let mut names = names(&Value::Null, &config_digest, &tags);
        if names.is_empty() {
            names.push(config_digest.clone());
        }
        for name in names {
            store.tag(&name, os, arch, OCI_MANIFEST, &digest, manifest.len() as u64)?;
            loaded.push(Loaded {
                name,
                platform: format!("{os}/{arch}"),
            });
        }
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Image;
    use crate::image::testing::{archive, layer, layout, manifest};
    use crate::qemu::GuestArch;
    use crate::qemu::testing::scratch;
    use crate::rootfs::testing::Layer;

    const CONFIG: &str = r#"{"architecture":"ARCH","os":"linux","config":{"Cmd":["loaded"]}}"#;

    fn tar_of(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, &data[..]).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn cmd(store: &Store, name: &str, arch: GuestArch) -> Option<Vec<String>> {
        Image::open_named(store.path(), Some(name), arch).unwrap().config.cmd
    }

    #[test]
    fn loads_every_platform_of_an_oci_layout() {
        let dir = layout(&scratch("load-oci"), &[("amd64", layer()), ("arm64", layer())], CONFIG);
        // Name it the way skopeo does (tag only) and the way containerd does (full name).
        let index = std::fs::read_to_string(dir.join("index.json")).unwrap().replacen(
            r#""platform":{"os":"linux","architecture":"amd64"}"#,
            &format!(
                r#""platform":{{"os":"linux","architecture":"amd64"}},"annotations":{{"{REF_NAME}":"1.0","{CONTAINERD_NAME}":"example.com/app:1.0"}}"#
            ),
            1,
        );
        std::fs::write(dir.join("index.json"), index).unwrap();
        for (i, path) in [dir.clone(), archive(&dir)].into_iter().enumerate() {
            let store = Store::open(&scratch(&format!("load-oci-store-{i}"))).unwrap();
            let mut out = Vec::new();
            let loaded = load(&path, &store, &mut out).unwrap();
            assert_eq!(loaded.len(), 2);
            assert!(loaded.contains(&Loaded {
                name: "example.com/app:1.0".into(),
                platform: "linux/amd64".into()
            }));
            let unnamed = loaded.iter().find(|l| l.platform == "linux/arm64").unwrap();
            assert!(unnamed.name.starts_with("sha256:"), "{unnamed:?}");
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("Loaded image: example.com/app:1.0 (linux/amd64)\n"),
                "{out}"
            );
            assert!(out.contains(&format!("Loaded image ID: {} (linux/arm64)\n", unnamed.name)));
            assert_eq!(
                cmd(&store, "example.com/app:1.0", GuestArch::X86_64),
                Some(vec!["loaded".into()])
            );
            assert_eq!(
                cmd(&store, &unnamed.name, GuestArch::Aarch64),
                Some(vec!["loaded".into()])
            );
        }
    }

    #[test]
    fn loads_what_docker_save_and_pixi_oci_write() {
        // Docker 25 and pixi-oci: an OCI layout plus manifest.json, whose RepoTags name the image.
        let dir = scratch("load-docker25");
        let (digest, bytes) = manifest(&dir, "arm64", &layer(), CONFIG);
        let config = String::from_utf8(bytes).unwrap();
        let config = config.split("sha256:").nth(1).unwrap()[..64].to_string();
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        std::fs::write(
            dir.join("index.json"),
            format!(r#"{{"manifests":[{{"mediaType":"{OCI_MANIFEST}","digest":"{digest}"}}]}}"#),
        )
        .unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"[{{"Config":"blobs/sha256/{config}","RepoTags":["debian:bookworm-slim"],"Layers":[]}}]"#),
        )
        .unwrap();
        let store = Store::open(&scratch("load-docker25-store")).unwrap();
        let loaded = load(&archive(&dir), &store, &mut Vec::new()).unwrap();
        assert_eq!(
            loaded,
            [Loaded {
                name: "docker.io/library/debian:bookworm-slim".into(),
                platform: "linux/arm64".into()
            }]
        );
    }

    #[test]
    fn converts_the_older_docker_save_format() {
        let config = CONFIG.replace("ARCH", "amd64").into_bytes();
        let plain = Layer::new().dir("bin").file("bin/sh", b"#!elf").bytes();
        let tarball = tar_of(&[
            ("abc.json", config),
            ("1111/layer.tar", plain),
            (
                "manifest.json",
                br#"[{"Config":"abc.json","RepoTags":["app:1","app:latest"],"Layers":["1111/layer.tar"]},
                    {"Config":"abc.json","RepoTags":null,"Layers":["1111/layer.tar"]}]"#
                    .to_vec(),
            ),
        ]);
        let path = scratch("load-legacy").join("app.tar");
        std::fs::write(&path, tarball).unwrap();
        let store = Store::open(&scratch("load-legacy-store")).unwrap();
        let loaded = load(&path, &store, &mut Vec::new()).unwrap();
        let names: Vec<&str> = loaded.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names[..2], ["docker.io/library/app:1", "docker.io/library/app:latest"]);
        assert!(names[2].starts_with("sha256:"));
        let image = Image::open_named(store.path(), Some("docker.io/library/app:1"), GuestArch::X86_64).unwrap();
        assert!(image.rootfs().unwrap().get("bin/sh").is_some());
    }

    #[test]
    fn refuses_what_it_cannot_load() {
        let store = Store::open(&scratch("load-bad-store")).unwrap();
        let empty = scratch("load-bad-empty");
        let err = load(&empty, &store, &mut Vec::new()).err().unwrap();
        assert!(err.to_string().contains("neither an OCI image layout"), "{err}");

        let climbing = scratch("load-bad").join("climb.tar");
        std::fs::write(
            &climbing,
            tar_of(&[(
                "manifest.json",
                br#"[{"Config":"../../etc/passwd","Layers":[]}]"#.to_vec(),
            )]),
        )
        .unwrap();
        let err = load(&climbing, &store, &mut Vec::new()).err().unwrap();
        assert!(err.to_string().contains("points outside the image"), "{err}");

        let none = scratch("load-bad").join("none.tar");
        std::fs::write(&none, tar_of(&[("manifest.json", b"[]".to_vec())])).unwrap();
        assert!(
            load(&none, &store, &mut Vec::new())
                .err()
                .unwrap()
                .to_string()
                .contains("no images")
        );

        // A layout whose blob was altered after it was written.
        let dir = layout(&scratch("load-bad-tampered"), &[("amd64", layer())], CONFIG);
        for entry in std::fs::read_dir(dir.join("blobs/sha256")).unwrap() {
            let path = entry.unwrap().path();
            let mut bytes = std::fs::read(&path).unwrap();
            if bytes.starts_with(&[0x1f, 0x8b]) {
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                std::fs::write(&path, bytes).unwrap();
            }
        }
        let err = load(&dir, &store, &mut Vec::new()).err().unwrap();
        assert!(matches!(err, ImageError::Mismatch(_)), "{err}");
    }
}
