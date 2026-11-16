//! Pulling images from OCI registries into the local [`Store`], on the host only: the guest never sees a registry,
//! a credential or a route out.
//!
//! Every manifest and blob is checked against its digest before it is stored; a mismatch is an error and nothing
//! is named. Credentials come from the files Podman and Docker already use (`podman login`, `docker login`),
//! including Docker credential helpers; with none, the pull is anonymous.

use crate::image::{self, ImageError};
use crate::store::Store;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

/// Comma-separated registries (`host:port`) to reach over plain HTTP, such as a local test registry.
pub const INSECURE_ENV: &str = "OCI_HERMIT_INSECURE_REGISTRIES";

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// The full name of an image reference, as Docker spells it out: `debian` is `docker.io/library/debian:latest`.
pub fn normalize(reference: &str) -> Result<Reference, String> {
    reference
        .parse::<Reference>()
        .map_err(|e| format!("{reference:?} is not an image reference: {e}"))
}

/// Where registry settings come from.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Podman and Docker auth files, in the order they are consulted.
    pub auth_files: Vec<PathBuf>,
    /// Registries reached over plain HTTP.
    pub insecure: Vec<String>,
    /// How long to wait before each retry of a request the registry turned away for now (rate limits, 5xx).
    pub retry_delays: Vec<std::time::Duration>,
}

/// The waits between retries of a throttled or failing request: 1, 2, 4 and 8 seconds.
pub fn default_retry_delays() -> Vec<std::time::Duration> {
    [1, 2, 4, 8].map(std::time::Duration::from_secs).to_vec()
}

/// Whether a registry error is worth retrying: throttling, a gateway or server hiccup, a dropped connection.
fn transient(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "rate exceeded",
        "toomanyrequests",
        "too many requests",
        "429",
        "502",
        "503",
        "504",
        "service unavailable",
        "connection reset",
        "timed out",
    ]
    .iter()
    .any(|sign| error.contains(sign))
}

/// Runs `attempt` until it succeeds, fails for good, or the delays run out.
async fn with_retries<T, F, Fut>(
    delays: &[std::time::Duration],
    mut attempt: F,
) -> Result<T, oci_client::errors::OciDistributionError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, oci_client::errors::OciDistributionError>>,
{
    let mut delays = delays.iter();
    loop {
        match attempt().await {
            Err(e) if transient(&e.to_string()) => match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(e),
            },
            done => return done,
        }
    }
}

impl Settings {
    /// The usual auth files and insecure registries, from environment variables read through `var`.
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Settings {
        let path = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = path("HOME").or_else(|| path("USERPROFILE"));
        let mut auth_files = Vec::new();
        auth_files.extend(path("REGISTRY_AUTH_FILE"));
        auth_files.extend(path("XDG_RUNTIME_DIR").map(|d| d.join("containers/auth.json")));
        let config = path("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
        auth_files.extend(config.map(|d| d.join("containers/auth.json")));
        let docker = path("DOCKER_CONFIG").or_else(|| home.as_ref().map(|h| h.join(".docker")));
        auth_files.extend(docker.map(|d| d.join("config.json")));
        let insecure = var(INSECURE_ENV)
            .map(|v| {
                v.to_string_lossy()
                    .split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Settings {
            auth_files,
            insecure,
            retry_delays: default_retry_delays(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
}

/// The keys an auth file may file `registry` under.
fn keys(registry: &str) -> Vec<String> {
    let mut keys = vec![registry.to_string(), format!("https://{registry}")];
    if matches!(registry, "docker.io" | "index.docker.io" | "registry-1.docker.io") {
        keys.extend(["https://index.docker.io/v1/", "index.docker.io", "docker.io"].map(String::from));
    }
    keys
}

/// A credential helper's answer for `server`: `docker-credential-<name> get`, as Docker runs it.
fn run_helper(name: &str, server: &str) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(rename = "Username")]
        username: String,
        #[serde(rename = "Secret")]
        secret: String,
    }
    let mut child = std::process::Command::new(format!("docker-credential-{name}"))
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(server.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    let answer: Answer = serde_json::from_slice(&output.stdout)
        .ok()
        .filter(|_| output.status.success())?;
    // An identity token is not a password; leave such logins to a later version.
    (answer.username != "<token>").then_some((answer.username, answer.secret))
}

/// Credentials for `registry` from the first auth file that has some; anonymous otherwise.
pub fn credentials(
    registry: &str,
    settings: &Settings,
    helper: impl Fn(&str, &str) -> Option<(String, String)>,
) -> RegistryAuth {
    for path in &settings.auth_files {
        let Ok(text) = std::fs::read(path) else { continue };
        let Ok(file) = serde_json::from_slice::<AuthFile>(&text) else {
            continue;
        };
        for key in keys(registry) {
            if let Some(name) = file.cred_helpers.get(&key)
                && let Some((user, secret)) = helper(name, &key)
            {
                return RegistryAuth::Basic(user, secret);
            }
            let decoded = file
                .auths
                .get(&key)
                .and_then(|entry| entry.auth.as_deref())
                .and_then(hermit_proto::b64::decode)
                .and_then(|bytes| String::from_utf8(bytes).ok());
            if let Some((user, secret)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
                return RegistryAuth::Basic(user.to_string(), secret.to_string());
            }
        }
        if let Some(store) = &file.creds_store {
            for key in keys(registry) {
                if let Some((user, secret)) = helper(store, &key) {
                    return RegistryAuth::Basic(user, secret);
                }
            }
        }
    }
    RegistryAuth::Anonymous
}

/// Why a pull failed.
#[derive(Debug)]
pub enum PullError {
    /// The registry, the network or the credentials.
    Registry(String),
    Image(ImageError),
    /// The image exists, but not for the platform asked for.
    NoPlatform(String),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::Registry(e) => write!(f, "{e}"),
            PullError::Image(e) => write!(f, "{e}"),
            PullError::NoPlatform(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PullError {}

impl From<ImageError> for PullError {
    fn from(e: ImageError) -> Self {
        PullError::Image(e)
    }
}

fn registry_error(what: &str, reference: &Reference) -> impl Fn(oci_client::errors::OciDistributionError) -> PullError {
    let context = format!("{what} {reference}");
    move |e| PullError::Registry(format!("{context}: {e}"))
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    os: String,
    architecture: String,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default, rename = "mediaType")]
    media_type: String,
    #[serde(default)]
    manifests: Vec<Descriptor>,
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

fn parse(bytes: &[u8]) -> Result<Manifest, PullError> {
    serde_json::from_slice(bytes).map_err(|e| PullError::Image(ImageError::Invalid(format!("manifest: {e}"))))
}

/// What a pull stored.
#[derive(Debug, PartialEq, Eq)]
pub struct Pulled {
    /// The full name the image is stored under.
    pub name: String,
    /// The digest of the platform's manifest.
    pub digest: String,
}

/// Pull `reference` for `linux/<arch>` into `store`, writing progress to `out`.
pub fn pull(
    reference: &Reference,
    arch: &str,
    store: &Store,
    settings: &Settings,
    out: &mut impl Write,
) -> Result<Pulled, PullError> {
    // reqwest is built without a default crypto provider; this picks ring. A second call finds one installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PullError::Registry(format!("starting the network runtime: {e}")))?;
    let client = Client::new(ClientConfig {
        protocol: ClientProtocol::HttpsExcept(settings.insecure.clone()),
        ..ClientConfig::default()
    });
    let auth = credentials(reference.resolve_registry(), settings, run_helper);
    runtime.block_on(async {
        client
            .auth(reference, &auth, RegistryOperation::Pull)
            .await
            .map_err(registry_error("logging in to pull", reference))?;
        let accepted = [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST];
        let delays = &settings.retry_delays;
        let (mut bytes, mut digest) = with_retries(delays, || client.pull_manifest_raw(reference, &auth, &accepted))
            .await
            .map_err(registry_error("pulling the manifest of", reference))?;
        image::verify(&digest, &bytes)?;
        if let Some(wanted) = reference.digest()
            && wanted != digest
        {
            return Err(ImageError::Mismatch(wanted.to_string()).into());
        }
        let mut manifest = parse(&bytes)?;
        if manifest.media_type == OCI_INDEX || manifest.media_type == DOCKER_LIST || !manifest.manifests.is_empty() {
            let found: Vec<String> = manifest
                .manifests
                .iter()
                .filter_map(|m| m.platform.as_ref().map(|p| format!("{}/{}", p.os, p.architecture)))
                .collect();
            let pick = manifest
                .manifests
                .iter()
                .find(|m| {
                    m.platform
                        .as_ref()
                        .is_some_and(|p| p.os == "linux" && p.architecture == arch)
                })
                .ok_or_else(|| {
                    PullError::NoPlatform(format!(
                        "{reference} has no linux/{arch} variant (it has: {})",
                        if found.is_empty() {
                            "none listed".into()
                        } else {
                            found.join(", ")
                        }
                    ))
                })?;
            let by_digest = reference.clone_with_digest(pick.digest.clone());
            let (platform_bytes, platform_digest) =
                with_retries(delays, || client.pull_manifest_raw(&by_digest, &auth, &accepted))
                    .await
                    .map_err(registry_error("pulling the manifest of", reference))?;
            image::verify(&pick.digest, &platform_bytes)?;
            if platform_digest != pick.digest {
                return Err(ImageError::Mismatch(pick.digest.clone()).into());
            }
            (bytes, digest) = (platform_bytes, platform_digest);
            manifest = parse(&bytes)?;
        }
        let config = manifest
            .config
            .as_ref()
            .ok_or_else(|| PullError::Image(ImageError::Invalid("the manifest has no config".into())))?;
        for (i, blob) in std::iter::once(config).chain(&manifest.layers).enumerate() {
            let short = blob.digest.get(7..19).unwrap_or(&blob.digest);
            if store.has_blob(&blob.digest) {
                let _ = writeln!(out, "{short}: already present");
                if i == 0 {
                    check_arch(reference, arch, &store.blob(&blob.digest)?)?;
                }
                continue;
            }
            let data = with_retries(delays, || async {
                let mut data = Vec::with_capacity(usize::try_from(blob.size).unwrap_or(0));
                client
                    .pull_blob(reference, blob.digest.as_str(), &mut data)
                    .await
                    .map(|()| data)
            })
            .await
            .map_err(registry_error(&format!("pulling blob {} of", blob.digest), reference))?;
            // A single-platform image says its architecture only in its config: check it before any layer.
            if i == 0 {
                check_arch(reference, arch, &data)?;
            }
            store.put_blob(&blob.digest, &data)?;
            let _ = writeln!(out, "{short}: pulled ({})", size(data.len()));
        }
        store.put_blob(&digest, &bytes)?;
        let media_type = if manifest.media_type.is_empty() {
            OCI_MANIFEST
        } else {
            &manifest.media_type
        };
        let name = reference.whole();
        store.tag(&name, "linux", arch, media_type, &digest, bytes.len() as u64)?;
        let _ = writeln!(out, "Digest: {digest}");
        Ok(Pulled { name, digest })
    })
}

/// Refuses an image whose config is for another architecture than `arch`.
fn check_arch(reference: &Reference, arch: &str, config: &[u8]) -> Result<(), PullError> {
    let config: serde_json::Value = serde_json::from_slice(config)
        .map_err(|e| PullError::Image(ImageError::Invalid(format!("image config: {e}"))))?;
    match config["architecture"].as_str() {
        Some(found) if !found.is_empty() && found != arch => Err(PullError::NoPlatform(format!(
            "{reference} has no linux/{arch} variant (it has: linux/{found})"
        ))),
        _ => Ok(()),
    }
}

fn size(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.1} kB", mb * 1000.0)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A small registry for tests: the distribution API's GETs over plain HTTP on 127.0.0.1.
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// One canned response.
    pub(crate) struct Response {
        pub(crate) content_type: String,
        pub(crate) body: Vec<u8>,
    }

    /// Serves `routes` (path to response) on a free port until the test process ends; returns `127.0.0.1:<port>`.
    /// With `basic` set (`user:password`, base64), every request but `/v2/` needs that `Authorization`.
    /// The first `throttle` requests other than `/v2/` get `429 Too Many Requests`, as a rate-limited registry does.
    pub(crate) fn serve(routes: HashMap<String, Response>, basic: Option<String>, throttle: usize) -> String {
        let throttled = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let routes = std::sync::Arc::new(routes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = routes.clone();
                let basic = basic.clone();
                let throttled = throttled.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut request = String::new();
                        if reader.read_line(&mut request).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut authorization = None;
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                                return;
                            }
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':')
                                && name.eq_ignore_ascii_case("authorization")
                            {
                                authorization = Some(value.trim().to_string());
                            }
                        }
                        let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                        let allowed = match &basic {
                            None => true,
                            Some(secret) => authorization.as_deref() == Some(&format!("Basic {secret}")),
                        };
                        let throttle_now = path != "/v2/" && {
                            let mut count = throttled.lock().unwrap();
                            *count += 1;
                            *count <= throttle
                        };
                        let (status, content_type, body): (&str, &str, &[u8]) = match routes.get(&path) {
                            _ if throttle_now => (
                                "429 Too Many Requests",
                                "application/json",
                                br#"{"errors":[{"code":"TOOMANYREQUESTS","message":"Rate exceeded"}]}"#,
                            ),
                            _ if !allowed => ("401 Unauthorized", "text/plain", b"unauthorized"),
                            Some(r) => ("200 OK", &r.content_type, &r.body),
                            None if path == "/v2/" => ("200 OK", "application/json", b"{}"),
                            None => (
                                "404 Not Found",
                                "application/json",
                                br#"{"errors":[{"code":"NAME_UNKNOWN","message":"not here"}]}"#,
                            ),
                        };
                        let challenge = if allowed {
                            ""
                        } else {
                            "WWW-Authenticate: Basic realm=\"test\"\r\n"
                        };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{challenge}\r\n",
                            body.len()
                        );
                        if stream
                            .write_all(head.as_bytes())
                            .and_then(|()| stream.write_all(body))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        address
    }

    /// A registry holding `test/app:1` as an index of amd64 and arm64 images whose `Cmd` is `from-the-registry`;
    /// with `tamper`, every layer it serves is one bit off. Returns its address.
    pub(crate) fn registry(name: &str, basic: Option<String>, tamper: bool, throttle: usize) -> String {
        registry_of(name, basic, tamper, throttle, &["amd64", "arm64"], true)
    }

    /// The same, for the given architectures; without `index`, `test/app:1` is the first architecture's manifest
    /// alone, as single-platform images are served.
    pub(crate) fn registry_of(
        name: &str,
        basic: Option<String>,
        tamper: bool,
        throttle: usize,
        arches: &[&str],
        index: bool,
    ) -> String {
        use super::{OCI_INDEX, OCI_MANIFEST};
        use crate::image::testing::{layer, manifest};
        let dir = crate::qemu::testing::scratch(name);
        let config = r#"{"architecture":"ARCH","config":{"Cmd":["from-the-registry"]}}"#;
        let mut entries = Vec::new();
        let mut first = None;
        for arch in arches {
            let (digest, bytes) = manifest(&dir, arch, &layer(), config);
            first.get_or_insert_with(|| bytes.clone());
            entries.push(format!(
                r#"{{"mediaType":"{OCI_MANIFEST}","digest":"{digest}","size":{},"platform":{{"os":"linux","architecture":"{arch}"}}}}"#,
                bytes.len()
            ));
        }
        let index_body = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{}]}}"#,
            entries.join(",")
        );
        let mut routes = HashMap::new();
        for entry in std::fs::read_dir(dir.join("blobs/sha256")).unwrap() {
            let path = entry.unwrap().path();
            let digest = format!("sha256:{}", path.file_name().unwrap().to_string_lossy());
            let mut body = std::fs::read(&path).unwrap();
            let is_manifest = body.starts_with(b"{\"schemaVersion\"");
            if tamper && body.starts_with(&[0x1f, 0x8b]) {
                let last = body.len() - 1;
                body[last] ^= 1;
            }
            let (kind, content_type) = if is_manifest {
                ("manifests", OCI_MANIFEST)
            } else {
                ("blobs", "application/octet-stream")
            };
            routes.insert(
                format!("/v2/test/app/{kind}/{digest}"),
                Response {
                    content_type: content_type.into(),
                    body,
                },
            );
        }
        let tag = if index {
            Response {
                content_type: OCI_INDEX.into(),
                body: index_body.into_bytes(),
            }
        } else {
            Response {
                content_type: OCI_MANIFEST.into(),
                body: first.unwrap(),
            }
        };
        routes.insert("/v2/test/app/manifests/1".into(), tag);
        serve(routes, basic, throttle)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::registry;
    use super::*;
    use crate::image::Image;
    use crate::qemu::GuestArch;
    use crate::qemu::testing::scratch;

    fn settings(address: &str, auth_files: Vec<PathBuf>) -> Settings {
        Settings {
            auth_files,
            insecure: vec![address.to_string()],
            retry_delays: vec![],
        }
    }

    #[test]
    fn retries_a_registry_that_is_throttling() {
        let address = registry("registry-throttled", None, false, 3);
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let store = Store::open(&scratch("registry-throttled-store")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("Rate exceeded"), "{err}");
        let patient = Settings {
            retry_delays: vec![std::time::Duration::ZERO; 3],
            ..settings(&address, vec![])
        };
        let address = registry("registry-throttled-2", None, false, 3);
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let patient = Settings {
            insecure: vec![address.clone()],
            ..patient
        };
        pull(&reference, "amd64", &store, &patient, &mut Vec::new()).unwrap();
        assert!(transient("Registry error: OCI API error: Rate exceeded"));
        assert!(!transient("manifest unknown"));
        assert_eq!(default_retry_delays().len(), 4);
    }

    #[test]
    fn pulls_the_platform_an_image_is_run_on_and_verifies_it() {
        let address = registry("registry-anon", None, false, 0);
        let store = Store::open(&scratch("registry-anon-store")).unwrap();
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let mut out = Vec::new();
        let pulled = pull(&reference, "arm64", &store, &settings(&address, vec![]), &mut out).unwrap();
        assert_eq!(pulled.name, format!("{address}/test/app:1"));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.matches(": pulled (").count(), 2, "{out}");
        assert!(out.ends_with(&format!("Digest: {}\n", pulled.digest)));
        let image = Image::open_named(store.path(), Some(&pulled.name), GuestArch::Aarch64).unwrap();
        assert_eq!(image.config.cmd, Some(vec!["from-the-registry".to_string()]));
        assert!(image.rootfs().unwrap().get("bin/sh").is_some());
        assert!(!store.has(&pulled.name, "linux", "amd64"));

        // A second pull finds the blobs already there.
        let mut again = Vec::new();
        pull(&reference, "arm64", &store, &settings(&address, vec![]), &mut again).unwrap();
        assert_eq!(String::from_utf8(again).unwrap().matches("already present").count(), 2);

        // A platform the image does not have.
        let err = pull(
            &reference,
            "s390x",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string()
                .contains("has no linux/s390x variant (it has: linux/amd64, linux/arm64)"),
            "{err}"
        );
    }

    #[test]
    fn a_single_platform_image_is_only_stored_for_its_own_architecture() {
        let address = testing::registry_of("registry-single", None, false, 0, &["arm64"], false);
        let store = Store::open(&scratch("registry-single-store")).unwrap();
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(err, PullError::NoPlatform(_)), "{err}");
        assert!(
            err.to_string()
                .ends_with("has no linux/amd64 variant (it has: linux/arm64)"),
            "{err}"
        );
        assert!(!store.has(&reference.whole(), "linux", "amd64"));
        pull(
            &reference,
            "arm64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .unwrap();
        // With the config already stored, the check still applies.
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(err, PullError::NoPlatform(_)), "{err}");
    }

    #[test]
    fn a_tampered_blob_is_a_hard_error() {
        let address = registry("registry-tampered", None, true, 0);
        let store = Store::open(&scratch("registry-tampered-store")).unwrap();
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().to_lowercase().contains("digest"), "{err}");
        assert!(!store.has(&reference.whole(), "linux", "amd64"));
    }

    #[test]
    fn pulls_with_credentials_from_an_auth_file() {
        let secret = hermit_proto::b64::encode(b"me:hunter2");
        let address = registry("registry-auth", Some(secret.clone()), false, 0);
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();

        let store = Store::open(&scratch("registry-auth-store")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(err, PullError::Registry(_)), "{err}");

        let auth = scratch("registry-auth-file").join("auth.json");
        std::fs::write(&auth, format!(r#"{{"auths":{{"{address}":{{"auth":"{secret}"}}}}}}"#)).unwrap();
        pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![auth]),
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn finds_credentials_the_way_docker_and_podman_file_them() {
        let dir = scratch("registry-creds");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let basic = |user: &str, secret: &str| RegistryAuth::Basic(user.into(), secret.into());
        let none = |_: &str, _: &str| None;
        let files = |paths: Vec<PathBuf>| Settings {
            auth_files: paths,
            ..Settings::default()
        };
        let encoded = hermit_proto::b64::encode(b"alice:s3cret");

        let podman = write(
            "podman.json",
            &format!(r#"{{"auths":{{"ghcr.io":{{"auth":"{encoded}"}}}}}}"#),
        );
        assert_eq!(
            credentials("ghcr.io", &files(vec![podman.clone()]), none),
            basic("alice", "s3cret")
        );
        assert_eq!(
            credentials("quay.io", &files(vec![podman.clone()]), none),
            RegistryAuth::Anonymous
        );

        let docker = write(
            "docker.json",
            &format!(r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{encoded}"}}}}}}"#),
        );
        assert_eq!(
            credentials("index.docker.io", &files(vec![docker]), none),
            basic("alice", "s3cret")
        );

        // Helpers, per registry or for all; the first file with an answer wins, and a broken file is skipped.
        let helpers = write(
            "helpers.json",
            r#"{"credHelpers":{"ghcr.io":"gh"},"credsStore":"desktop"}"#,
        );
        let broken = write("broken.json", "not json");
        let helper = |name: &str, server: &str| match (name, server) {
            ("gh", "ghcr.io") => Some(("from-gh".to_string(), "x".to_string())),
            ("desktop", "quay.io") => Some(("from-desktop".to_string(), "y".to_string())),
            _ => None,
        };
        let both = files(vec![broken, dir.join("missing.json"), helpers, podman]);
        assert_eq!(credentials("ghcr.io", &both, helper), basic("from-gh", "x"));
        assert_eq!(credentials("quay.io", &both, helper), basic("from-desktop", "y"));
        assert_eq!(credentials("example.com", &both, helper), RegistryAuth::Anonymous);
        assert!(run_helper("oci-hermit-no-such-helper", "example.com").is_none());
    }

    #[test]
    fn settings_come_from_the_usual_places() {
        let env = |name: &str| {
            Some(OsString::from(match name {
                "HOME" => "/home/me",
                "REGISTRY_AUTH_FILE" => "/run/auth.json",
                "XDG_RUNTIME_DIR" => "/run/user/1000",
                INSECURE_ENV => "localhost:5000, 127.0.0.1:5001,",
                _ => return None,
            }))
        };
        let settings = Settings::from_env(env);
        let expected: Vec<PathBuf> = [
            "/run/auth.json",
            "/run/user/1000/containers/auth.json",
            "/home/me/.config/containers/auth.json",
            "/home/me/.docker/config.json",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(settings.auth_files, expected);
        assert_eq!(settings.insecure, ["localhost:5000", "127.0.0.1:5001"]);
        assert_eq!(settings.retry_delays, default_retry_delays());
        assert!(Settings::from_env(|_| None).auth_files.is_empty());
    }

    #[test]
    fn references_are_spelled_out_like_docker() {
        assert_eq!(normalize("debian").unwrap().whole(), "docker.io/library/debian:latest");
        assert_eq!(
            normalize("debian:bookworm-slim").unwrap().whole(),
            "docker.io/library/debian:bookworm-slim"
        );
        assert_eq!(normalize("ghcr.io/org/app:1.2").unwrap().whole(), "ghcr.io/org/app:1.2");
        assert!(normalize("Not A Reference").is_err());
        assert_eq!(size(1_500_000), "1.5 MB");
        assert_eq!(size(1_500), "1.5 kB");
    }
}
