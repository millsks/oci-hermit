//! Pulling images from OCI registries into the local [`Store`], on the host only: the guest never sees a registry,
//! a credential or a route out.
//!
//! The distribution API's reads over reqwest: the manifest by tag or digest, the platform's manifest from an index,
//! then the config and layers, with the registry's Basic or Bearer (token server) authorization. Every manifest and
//! blob is checked against its digest before it is stored; a mismatch is an error and nothing is named. Credentials
//! come from the files Podman and Docker already use (`podman login`, `docker login`), including Docker credential
//! helpers; with none, the pull is anonymous.

use crate::image::{self, ImageError};
use crate::store::Store;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub use crate::reference::Reference;

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

/// One place an auth file is looked for: how it is named, and the file if the variables it needs are set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthLocation {
    /// The location as written in Podman's and Docker's docs, e.g. `$XDG_RUNTIME_DIR/containers/auth.json`.
    pub name: &'static str,
    pub path: Option<PathBuf>,
}

/// reqwest is built without a default crypto provider; this picks ring. A second call finds one installed.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Where registry settings come from.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Podman and Docker auth files, in the order they are consulted.
    pub auth_files: Vec<PathBuf>,
    /// Every place an auth file is looked for, set or not, in the same order; for `machine info`.
    pub auth_locations: Vec<AuthLocation>,
    /// Registries reached over plain HTTP.
    pub insecure: Vec<String>,
    /// How long to wait before each retry of a request the registry turned away for now (rate limits, 5xx).
    pub retry_delays: Vec<Duration>,
    /// The `certs.d` folders searched for a registry's CAs and client certificate, in order.
    pub cert_locations: Vec<crate::certs::CertLocation>,
    /// `--cert-dir`: one registry's certificates, used instead of the `certs.d` folders.
    pub cert_dir: Option<PathBuf>,
}

/// The waits between retries of a throttled or failing request: 1, 2, 4 and 8 seconds.
pub fn default_retry_delays() -> Vec<Duration> {
    [1, 2, 4, 8].map(Duration::from_secs).to_vec()
}

impl Settings {
    /// The usual auth files and insecure registries, from environment variables read through `var`.
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Settings {
        let path = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = path("HOME").or_else(|| path("USERPROFILE"));
        // A variable that overrides a home-folder default is named, so `machine info` shows which one applied.
        let (config_name, config) = match path("XDG_CONFIG_HOME") {
            Some(dir) => ("$XDG_CONFIG_HOME/containers/auth.json", Some(dir)),
            None => (
                "~/.config/containers/auth.json",
                home.as_ref().map(|h| h.join(".config")),
            ),
        };
        let (docker_name, docker) = match path("DOCKER_CONFIG") {
            Some(dir) => ("$DOCKER_CONFIG/config.json", Some(dir)),
            None => ("~/.docker/config.json", home.as_ref().map(|h| h.join(".docker"))),
        };
        let auth_locations = vec![
            AuthLocation {
                name: "$REGISTRY_AUTH_FILE",
                path: path("REGISTRY_AUTH_FILE"),
            },
            AuthLocation {
                name: "$XDG_RUNTIME_DIR/containers/auth.json",
                path: path("XDG_RUNTIME_DIR").map(|d| d.join("containers/auth.json")),
            },
            AuthLocation {
                name: config_name,
                path: config.map(|d| d.join("containers/auth.json")),
            },
            AuthLocation {
                name: docker_name,
                path: docker.map(|d| d.join("config.json")),
            },
        ];
        let auth_files = auth_locations.iter().filter_map(|l| l.path.clone()).collect();
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
            auth_locations,
            insecure,
            retry_delays: default_retry_delays(),
            cert_locations: crate::certs::locations(&var),
            cert_dir: None,
        }
    }

    /// The CAs and client certificate for `registry`, if it has any.
    pub(crate) fn certs_for(&self, registry: &str) -> Result<Option<crate::certs::RegistryCerts>, String> {
        crate::certs::find(&self.cert_locations, self.cert_dir.as_deref(), registry)
    }

    /// `http` for a registry listed in `$OCI_HERMIT_INSECURE_REGISTRIES`, `https` for every other.
    pub(crate) fn scheme(&self, registry: &str) -> &'static str {
        if self.insecure.iter().any(|r| r == registry) {
            "http"
        } else {
            "https"
        }
    }
}

/// The HTTP client for one registry: rustls with the system's trust store, plus that registry's own CAs and client
/// certificate from `certs.d`. A client is never shared between registries, so their CAs never apply elsewhere.
pub(crate) fn http_client(certs: Option<&crate::certs::RegistryCerts>) -> Result<reqwest::Client, String> {
    install_crypto();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!("oci-hermit/", env!("CARGO_PKG_VERSION")));
    if let Some(certs) = certs {
        builder = certs.apply(builder)?;
    }
    builder.build().map_err(|e| describe(&e))
}

/// What to do about a TLS failure with `registry`: where its CA or client certificate goes.
pub(crate) fn tls_hint(described: &str, registry: &str, certs: Option<&crate::certs::RegistryCerts>) -> String {
    if !tls_failure(described) {
        return String::new();
    }
    let folder = certs.map_or_else(
        || format!("~/.config/containers/certs.d/{registry}"),
        |c| c.dir.display().to_string(),
    );
    if described.contains("CertificateRequired") || (described.contains("HandshakeFailure") && certs.is_none()) {
        format!(
            " (the registry may want a client certificate: put it in {folder}/ as client.cert with client.key; \
             `hrmt machine info` lists the folders searched)"
        )
    } else {
        format!(
            " (if {registry} uses its own CA, put that CA in {folder}/ as ca.crt; `hrmt machine info` lists the \
             folders searched)"
        )
    }
}

/// A reqwest error with its causes, which is where TLS and connection problems are spelled out.
pub(crate) fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Whether a failed request is worth sending again: a dropped connection or a timeout is, a certificate the
/// system does not trust is not.
fn retryable(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect() && !tls_failure(&describe(error))
}

/// Whether a request failed in the TLS handshake: rustls says so in its message (`invalid peer certificate`,
/// `received fatal alert`), however many layers the platform verifier and hyper wrap it in.
fn tls_failure(described: &str) -> bool {
    ["certificate", "received fatal alert"]
        .iter()
        .any(|sign| described.contains(sign))
}

/// How to authenticate to a registry.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Anonymous,
    /// A user name and password (or token), as `login` stores them.
    Basic(String, String),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Anonymous => write!(f, "Anonymous"),
            Auth::Basic(user, _) => write!(f, "Basic({user:?}, <hidden>)"),
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

/// The names auth files may keep a registry's login under.
pub(crate) fn keys(registry: &str) -> Vec<String> {
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
) -> Auth {
    for path in &settings.auth_files {
        let Ok(text) = std::fs::read(path) else { continue };
        let Ok(file) = serde_json::from_slice::<AuthFile>(&text) else {
            continue;
        };
        for key in keys(registry) {
            if let Some(name) = file.cred_helpers.get(&key)
                && let Some((user, secret)) = helper(name, &key)
            {
                return Auth::Basic(user, secret);
            }
            let decoded = file
                .auths
                .get(&key)
                .and_then(|entry| entry.auth.as_deref())
                .and_then(hermit_proto::b64::decode)
                .and_then(|bytes| String::from_utf8(bytes).ok());
            if let Some((user, secret)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
                return Auth::Basic(user.to_string(), secret.to_string());
            }
        }
        if let Some(store) = &file.creds_store {
            for key in keys(registry) {
                if let Some((user, secret)) = helper(store, &key) {
                    return Auth::Basic(user, secret);
                }
            }
        }
    }
    Auth::Anonymous
}

/// The `key="value"` pairs of a `WWW-Authenticate: Bearer ...` header.
pub(crate) fn bearer_challenge(header: &str) -> Option<Vec<(String, String)>> {
    let header = header.trim();
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let mut pairs = Vec::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (value, tail) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"')?;
                (&quoted[..end], &quoted[end + 1..])
            }
            None => after.split_once(',').unwrap_or((after, "")),
        };
        pairs.push((key.trim().to_ascii_lowercase(), value.to_string()));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    Some(pairs)
}

/// A registry's error answer as people read it: `429 TOOMANYREQUESTS: Rate exceeded`.
fn answer_text(status: reqwest::StatusCode, body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Errors {
        errors: Vec<ErrorEntry>,
    }
    #[derive(Deserialize)]
    struct ErrorEntry {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    }
    let detail = serde_json::from_slice::<Errors>(body)
        .ok()
        .map(|e| {
            e.errors
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
    format!("{} {detail}", status.as_u16())
}

/// One repository on one registry, with the authorization its server asked for.
struct Session<'a> {
    client: reqwest::Client,
    certs: Option<crate::certs::RegistryCerts>,
    /// `https://<api host>/v2/<repository>`.
    base: String,
    registry: String,
    repository: String,
    auth: &'a Auth,
    authorization: Option<String>,
    delays: &'a [Duration],
}

impl Session<'_> {
    /// GETs `path` under the repository, authorizing when the registry asks and retrying what is worth retrying.
    async fn get(&mut self, path: &str, accept: &[&str]) -> Result<(reqwest::header::HeaderMap, Vec<u8>), String> {
        let url = format!("{}/{path}", self.base);
        let mut delays = self.delays.iter();
        let mut authorized = false;
        loop {
            let mut request = self.client.get(&url);
            if !accept.is_empty() {
                request = request.header(reqwest::header::ACCEPT, accept.join(", "));
            }
            if let Some(authorization) = &self.authorization {
                request = request.header(reqwest::header::AUTHORIZATION, authorization);
            }
            let failure = match request.send().await {
                Err(e) if retryable(&e) => describe(&e),
                Err(e) => {
                    let described = describe(&e);
                    let hint = tls_hint(&described, &self.registry, self.certs.as_ref());
                    return Err(format!("{described}{hint}"));
                }
                Ok(answer) => {
                    let status = answer.status();
                    // A 401 asks for authorization; a token can also expire during a long pull, so ask once more.
                    if status == reqwest::StatusCode::UNAUTHORIZED && !authorized {
                        authorized = true;
                        let challenge = answer
                            .headers()
                            .get(reqwest::header::WWW_AUTHENTICATE)
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        self.authorize(&challenge).await?;
                        continue;
                    }
                    let headers = answer.headers().clone();
                    match answer.bytes().await {
                        Ok(body) if status.is_success() => return Ok((headers, body.to_vec())),
                        Ok(body) => {
                            let text = answer_text(status, &body);
                            if status == reqwest::StatusCode::UNAUTHORIZED {
                                return Err(match self.auth {
                                    Auth::Anonymous => format!(
                                        "{text} (no login for {} was found; log in with `hrmt login {}`)",
                                        self.registry, self.registry
                                    ),
                                    Auth::Basic(..) => format!("{text} (the registry turned the login down)"),
                                });
                            }
                            if status != reqwest::StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                                return Err(text);
                            }
                            text
                        }
                        Err(e) => describe(&e),
                    }
                }
            };
            match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(failure),
            }
        }
    }

    /// Answers a `WWW-Authenticate` challenge: Basic with the login, or Bearer with a token from the registry's
    /// token server for pulling this repository.
    async fn authorize(&mut self, challenge: &str) -> Result<(), String> {
        let basic = match self.auth {
            Auth::Basic(user, password) => Some((user.as_str(), password.as_str())),
            Auth::Anonymous => None,
        };
        let Some(pairs) = bearer_challenge(challenge) else {
            let Some((user, password)) = basic else {
                return Err(format!(
                    "{} needs a login and none was found; log in with `hrmt login {}`",
                    self.registry, self.registry
                ));
            };
            let secret = hermit_proto::b64::encode(format!("{user}:{password}").as_bytes());
            self.authorization = Some(format!("Basic {secret}"));
            return Ok(());
        };
        let value = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let realm = value("realm").ok_or_else(|| format!("{} sent a token challenge with no realm", self.registry))?;
        let mut query = vec![(
            "scope".to_string(),
            value("scope").unwrap_or_else(|| format!("repository:{}:pull", self.repository)),
        )];
        query.extend(value("service").map(|s| ("service".to_string(), s)));
        let mut request = self.client.get(&realm).query(&query);
        if let Some((user, password)) = basic {
            request = request.basic_auth(user, Some(password));
        }
        let answer = request
            .send()
            .await
            .map_err(|e| format!("asking the token server {realm}: {}", describe(&e)))?;
        let status = answer.status();
        let body = answer
            .bytes()
            .await
            .map_err(|e| format!("reading from the token server {realm}: {}", describe(&e)))?;
        if !status.is_success() {
            return Err(format!(
                "the token server {realm} answered {}",
                answer_text(status, &body)
            ));
        }
        #[derive(Deserialize)]
        struct Token {
            #[serde(default)]
            token: String,
            #[serde(default)]
            access_token: String,
        }
        let token: Token =
            serde_json::from_slice(&body).map_err(|e| format!("the token server {realm} sent no token: {e}"))?;
        let token = if token.token.is_empty() {
            token.access_token
        } else {
            token.token
        };
        if token.is_empty() {
            return Err(format!("the token server {realm} sent an empty token"));
        }
        self.authorization = Some(format!("Bearer {token}"));
        Ok(())
    }

    /// A manifest by tag or digest, with its digest: the registry's `Docker-Content-Digest` when it sends one
    /// (checked against the bytes), else the bytes' own.
    async fn manifest(&mut self, target: &str) -> Result<(Vec<u8>, String), PullError> {
        let accepted = [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST];
        let (headers, bytes) = self
            .get(&format!("manifests/{target}"), &accepted)
            .await
            .map_err(|e| PullError::Registry(format!("pulling the manifest of {}: {e}", self.name(target))))?;
        let digest = headers
            .get("docker-content-digest")
            .and_then(|h| h.to_str().ok())
            .map_or_else(|| image::digest_of(&bytes), str::to_string);
        image::verify(&digest, &bytes)?;
        Ok((bytes, digest))
    }

    async fn blob(&mut self, digest: &str) -> Result<Vec<u8>, PullError> {
        let (_, bytes) = self
            .get(&format!("blobs/{digest}"), &[])
            .await
            .map_err(|e| PullError::Registry(format!("pulling blob {digest} of {}: {e}", self.name(""))))?;
        Ok(bytes)
    }

    fn name(&self, target: &str) -> String {
        let at = if target.starts_with("sha256:") { "@" } else { ":" };
        let target = if target.is_empty() {
            String::new()
        } else {
            format!("{at}{target}")
        };
        format!("{}/{}{target}", self.registry, self.repository)
    }
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PullError::Registry(format!("starting the network runtime: {e}")))?;
    let auth = credentials(reference.registry(), settings, run_helper);
    let certs = settings.certs_for(reference.registry()).map_err(PullError::Registry)?;
    let mut session = Session {
        client: http_client(certs.as_ref()).map_err(PullError::Registry)?,
        certs,
        base: format!(
            "{}://{}/v2/{}",
            settings.scheme(reference.registry()),
            reference.api_host(),
            reference.repository()
        ),
        registry: reference.registry().to_string(),
        repository: reference.repository().to_string(),
        auth: &auth,
        authorization: None,
        delays: &settings.retry_delays,
    };
    runtime.block_on(async {
        let (mut bytes, mut digest) = session.manifest(reference.target()).await?;
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
            let (platform_bytes, platform_digest) = session.manifest(&pick.digest).await?;
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
            let data = session.blob(&blob.digest).await?;
            if blob.size != 0 && data.len() as u64 != blob.size {
                return Err(ImageError::Mismatch(blob.digest.clone()).into());
            }
            // A single-platform image says its architecture only in its config: check it before any layer.
            if i == 0 {
                check_arch(reference, arch, &data)?;
            }
            store.put_blob(&blob.digest, &data)?;
            let _ = writeln!(out, "{short}: pulled ({})", size(data.len() as u64));
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

/// A byte count for people: `28.9 MB`, `2.6 kB`.
pub(crate) fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / 1_000_000.0;
    if bytes < 1000 {
        format!("{bytes} B")
    } else if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.1} kB", mb * 1000.0)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A small registry for tests: the distribution API's GETs over plain HTTP or TLS on 127.0.0.1.
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// One canned response.
    pub(crate) struct Response {
        pub(crate) content_type: String,
        pub(crate) body: Vec<u8>,
    }

    /// Who may read from a test registry.
    #[derive(Clone)]
    pub(crate) enum Gate {
        Open,
        /// Basic auth with this `user:password` (base64) on every request.
        Basic(String),
        /// A Bearer challenge; the registry's `/token` hands out a token for this Basic secret (or to anyone, when
        /// `None`), as Docker Hub and GHCR do.
        Bearer(Option<String>),
    }

    const TOKEN: &str = "test-token";

    /// Serves `routes` (path to response) on a free port until the test process ends; returns `127.0.0.1:<port>`.
    /// With `basic` set (`user:password`, base64), every request but `/v2/` needs that `Authorization`.
    /// The first `throttle` requests other than `/v2/` get `429 Too Many Requests`, as a rate-limited registry does.
    pub(crate) fn serve(routes: HashMap<String, Response>, basic: Option<String>, throttle: usize) -> String {
        serve_gated(routes, basic.map_or(Gate::Open, Gate::Basic), throttle)
    }

    /// [`serve`] with any [`Gate`].
    pub(crate) fn serve_gated(routes: HashMap<String, Response>, gate: Gate, throttle: usize) -> String {
        serve_on(routes, gate, throttle, None)
    }

    /// [`serve_gated`] over TLS with `tls` (its certificate, and whether it wants a client certificate) when given.
    pub(crate) fn serve_on(
        routes: HashMap<String, Response>,
        gate: Gate,
        throttle: usize,
        tls: Option<std::sync::Arc<rustls::ServerConfig>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::sync::Arc::new(Server {
            routes,
            gate,
            throttle,
            throttled: std::sync::Mutex::new(0),
            address: address.clone(),
            scheme: if tls.is_some() { "https" } else { "http" },
        });
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let server = server.clone();
                let tls = tls.clone();
                std::thread::spawn(move || match tls {
                    Some(config) => {
                        let Ok(connection) = rustls::ServerConnection::new(config) else {
                            return;
                        };
                        server.answer(rustls::StreamOwned::new(connection, stream));
                    }
                    None => server.answer(stream),
                });
            }
        });
        address
    }

    struct Server {
        routes: HashMap<String, Response>,
        gate: Gate,
        throttle: usize,
        throttled: std::sync::Mutex<usize>,
        address: String,
        scheme: &'static str,
    }

    impl Server {
        /// Answers the requests on one connection until it closes.
        fn answer(&self, stream: impl std::io::Read + Write) {
            let Server {
                routes,
                gate,
                throttle,
                throttled,
                address,
                scheme,
            } = self;
            let throttle = *throttle;
            let mut reader = BufReader::new(stream);
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
                let sent = authorization.as_deref();
                let token_request = path.starts_with("/token?");
                let allowed = match &gate {
                    Gate::Open => true,
                    Gate::Basic(secret) => sent == Some(&format!("Basic {secret}")),
                    Gate::Bearer(Some(secret)) if token_request => sent == Some(&format!("Basic {secret}")),
                    Gate::Bearer(None) if token_request => true,
                    Gate::Bearer(_) => sent == Some(&format!("Bearer {TOKEN}")),
                };
                let token_body = format!(r#"{{"token":"{TOKEN}"}}"#);
                let throttle_now = path != "/v2/" && !token_request && {
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
                    // Only a token for pulling `test/app` is handed out.
                    _ if token_request && path.contains("scope=repository%3Atest%2Fapp%3Apull") => {
                        ("200 OK", "application/json", token_body.as_bytes())
                    }
                    _ if token_request => ("403 Forbidden", "text/plain", b"wrong scope"),
                    Some(r) => ("200 OK", &r.content_type, &r.body),
                    None if path == "/v2/" => ("200 OK", "application/json", b"{}"),
                    None => (
                        "404 Not Found",
                        "application/json",
                        br#"{"errors":[{"code":"NAME_UNKNOWN","message":"not here"}]}"#,
                    ),
                };
                let challenge = match &gate {
                    _ if allowed => String::new(),
                    Gate::Bearer(_) if !token_request => {
                        format!("WWW-Authenticate: Bearer realm=\"{scheme}://{address}/token\",service=\"test\"\r\n")
                    }
                    _ => "WWW-Authenticate: Basic realm=\"test\"\r\n".to_string(),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{challenge}\r\n",
                    body.len()
                );
                let stream = reader.get_mut();
                if stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(body))
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
            }
        }
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
        serve(routes_of(name, tamper, arches, index), basic, throttle)
    }

    /// The routes of [`registry_of`]'s registry.
    pub(crate) fn routes_of(name: &str, tamper: bool, arches: &[&str], index: bool) -> HashMap<String, Response> {
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
        routes
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
            ..Settings::default()
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
    fn pulls_with_a_token_from_the_token_server() {
        use testing::{Gate, routes_of, serve_gated};
        let store = Store::open(&scratch("registry-bearer-store")).unwrap();
        // Anonymous tokens, as Docker Hub hands out for public images.
        let address = serve_gated(
            routes_of("registry-bearer-anon", false, &["amd64"], true),
            Gate::Bearer(None),
            0,
        );
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .unwrap();

        // A token server that wants the login: without one the pull says how to log in, with one it goes through.
        let secret = hermit_proto::b64::encode(b"me:hunter2");
        let address = serve_gated(
            routes_of("registry-bearer-login", false, &["amd64"], true),
            Gate::Bearer(Some(secret.clone())),
            0,
        );
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let store = Store::open(&scratch("registry-bearer-login-store")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("token server"), "{err}");
        let auth = scratch("registry-bearer-auth").join("auth.json");
        std::fs::write(&auth, format!(r#"{{"auths":{{"{address}":{{"auth":"{secret}"}}}}}}"#)).unwrap();
        pull(
            &reference,
            "amd64",
            &store,
            &settings(&address, vec![auth]),
            &mut Vec::new(),
        )
        .unwrap();

        // A token for another repository is not handed out.
        let other = normalize(&format!("{address}/test/other:1")).unwrap();
        let err = pull(&other, "amd64", &store, &settings(&address, vec![]), &mut Vec::new())
            .err()
            .unwrap();
        assert!(matches!(err, PullError::Registry(_)), "{err}");
    }

    #[test]
    fn registry_errors_read_as_people_write_them() {
        let throttled = br#"{"errors":[{"code":"TOOMANYREQUESTS","message":"Rate exceeded"}]}"#;
        assert_eq!(
            answer_text(reqwest::StatusCode::TOO_MANY_REQUESTS, throttled),
            "429 TOOMANYREQUESTS: Rate exceeded"
        );
        assert_eq!(answer_text(reqwest::StatusCode::NOT_FOUND, b"<html>"), "404 Not Found");
        assert_eq!(
            format!("{:?}", Auth::Basic("me".into(), "hunter2".into())),
            r#"Basic("me", <hidden>)"#
        );
        let anonymous = Settings::default();
        assert_eq!(anonymous.scheme("example.com"), "https");
        assert_eq!(settings("localhost:5000", vec![]).scheme("localhost:5000"), "http");
        // An untrusted certificate is not retried; a dropped connection is.
        assert!(tls_failure(
            "client error (Connect): invalid peer certificate: UnknownIssuer"
        ));
        assert!(tls_failure("received fatal alert: CertificateRequired"));
        assert!(!tls_failure("client error (Connect): connection reset by peer"));
    }

    #[test]
    fn a_registry_that_cannot_be_reached_fails_with_the_cause() {
        // Nothing listens on port 9 of 127.0.0.1.
        let reference = normalize("127.0.0.1:9/test/app:1").unwrap();
        let store = Store::open(&scratch("registry-unreachable-store")).unwrap();
        let err = pull(
            &reference,
            "amd64",
            &store,
            &Settings {
                retry_delays: vec![],
                ..settings("127.0.0.1:9", vec![])
            },
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string()
                .contains("pulling the manifest of 127.0.0.1:9/test/app:1"),
            "{err}"
        );
    }

    /// A CA, and a registry certificate for 127.0.0.1 and a client certificate it signed, as PEM.
    struct Pki {
        ca: String,
        server: (String, String),
        client: (String, String),
    }

    fn pki() -> Pki {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
            KeyUsagePurpose,
        };
        // Valid from yesterday for a month: platform verifiers refuse certificates valid for years.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let epoch = rcgen::date_time_ymd(1970, 1, 1);
        let dated = |mut params: CertificateParams| {
            params.not_before = epoch + (now - Duration::from_secs(86_400));
            params.not_after = epoch + (now + Duration::from_secs(30 * 86_400));
            params
        };
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = dated(CertificateParams::new(Vec::<String>::new()).unwrap());
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "oci-hermit test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf = |name: &str, purpose: ExtendedKeyUsagePurpose| {
            let key = KeyPair::generate().unwrap();
            let mut params = dated(CertificateParams::new(vec![name.to_string()]).unwrap());
            params.distinguished_name.push(DnType::CommonName, name);
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![purpose];
            let cert = params.signed_by(&key, &issuer).unwrap();
            (cert.pem(), key.serialize_pem())
        };
        Pki {
            ca: ca.pem(),
            server: leaf("127.0.0.1", ExtendedKeyUsagePurpose::ServerAuth),
            client: leaf("oci-hermit test client", ExtendedKeyUsagePurpose::ClientAuth),
        }
    }

    /// A TLS registry with `test/app:1`, its certificate signed by `pki`'s CA; with `mutual`, it wants a client
    /// certificate signed by that CA.
    fn tls_registry(name: &str, pki: &Pki, mutual: bool, gate: testing::Gate) -> String {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = if mutual {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from_pem_slice(pki.ca.as_bytes()).unwrap())
                .unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots.into(), provider)
                .build()
                .unwrap();
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let config = builder
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(pki.server.0.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(pki.server.1.as_bytes()).unwrap(),
            )
            .unwrap();
        testing::serve_on(
            testing::routes_of(name, false, &["amd64"], true),
            gate,
            0,
            Some(std::sync::Arc::new(config)),
        )
    }

    /// Settings that look for certificates only under `certs_d`, and do not retry.
    fn tls_settings(certs_d: &std::path::Path, auth_files: Vec<PathBuf>) -> Settings {
        Settings {
            auth_files,
            cert_locations: vec![crate::certs::CertLocation {
                name: "test",
                path: Some(certs_d.to_path_buf()),
            }],
            ..Settings::default()
        }
    }

    fn folder_for(certs_d: &std::path::Path, address: &str) -> PathBuf {
        let dir = certs_d.join(crate::certs::folder_names(address).pop().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_registry_with_its_own_ca_is_trusted_from_certs_d_and_only_there() {
        let pki = pki();
        let address = tls_registry("registry-tls-ca", &pki, false, testing::Gate::Open);
        let other = tls_registry("registry-tls-other", &pki, false, testing::Gate::Open);
        let certs_d = scratch("registry-tls-ca-certs");
        let store = Store::open(&scratch("registry-tls-ca-store")).unwrap();
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();

        // Without the CA the system does not trust the registry, and the error says where the CA goes.
        let err = pull(
            &reference,
            "amd64",
            &store,
            &tls_settings(&certs_d, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("uses its own CA"), "{err}");

        std::fs::write(folder_for(&certs_d, &address).join("ca.crt"), &pki.ca).unwrap();
        pull(
            &reference,
            "amd64",
            &store,
            &tls_settings(&certs_d, vec![]),
            &mut Vec::new(),
        )
        .unwrap();

        // The CA is this registry's alone: another registry with a certificate from the same CA is not trusted.
        let elsewhere = normalize(&format!("{other}/test/app:1")).unwrap();
        let err = pull(
            &elsewhere,
            "amd64",
            &store,
            &tls_settings(&certs_d, vec![]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("certificate"), "{err}");

        // --cert-dir names the folder directly.
        let given = scratch("registry-tls-cert-dir");
        std::fs::write(given.join("ca.crt"), &pki.ca).unwrap();
        let with_dir = Settings {
            cert_dir: Some(given),
            ..tls_settings(&scratch("registry-tls-empty"), vec![])
        };
        let store = Store::open(&scratch("registry-tls-cert-dir-store")).unwrap();
        pull(&elsewhere, "amd64", &store, &with_dir, &mut Vec::new()).unwrap();
    }

    #[test]
    fn a_registry_that_wants_a_client_certificate_gets_the_one_in_certs_d() {
        let pki = pki();
        let secret = hermit_proto::b64::encode(b"me:hunter2");
        let address = tls_registry("registry-mtls", &pki, true, testing::Gate::Basic(secret.clone()));
        let certs_d = scratch("registry-mtls-certs");
        let folder = folder_for(&certs_d, &address);
        std::fs::write(folder.join("ca.crt"), &pki.ca).unwrap();
        let store = Store::open(&scratch("registry-mtls-store")).unwrap();
        let reference = normalize(&format!("{address}/test/app:1")).unwrap();
        let auth = scratch("registry-mtls-auth").join("auth.json");
        std::fs::write(&auth, format!(r#"{{"auths":{{"{address}":{{"auth":"{secret}"}}}}}}"#)).unwrap();

        let err = pull(
            &reference,
            "amd64",
            &store,
            &tls_settings(&certs_d, vec![auth.clone()]),
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("client certificate"), "{err}");
        assert!(crate::login::verify(&address, "me", "hunter2", &tls_settings(&certs_d, vec![])).is_err());

        std::fs::write(folder.join("client.cert"), &pki.client.0).unwrap();
        std::fs::write(folder.join("client.key"), &pki.client.1).unwrap();
        pull(
            &reference,
            "amd64",
            &store,
            &tls_settings(&certs_d, vec![auth]),
            &mut Vec::new(),
        )
        .unwrap();
        crate::login::verify(&address, "me", "hunter2", &tls_settings(&certs_d, vec![])).unwrap();
    }

    #[test]
    fn finds_credentials_the_way_docker_and_podman_file_them() {
        let dir = scratch("registry-creds");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let basic = |user: &str, secret: &str| Auth::Basic(user.into(), secret.into());
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
            Auth::Anonymous
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
        assert_eq!(credentials("example.com", &both, helper), Auth::Anonymous);
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
        assert_eq!(settings.auth_locations.len(), 4);
        // With nothing set, nothing is searched, but every location is still named.
        let bare = Settings::from_env(|_| None);
        assert!(bare.auth_files.is_empty());
        let names: Vec<_> = bare.auth_locations.iter().map(|l| l.name).collect();
        assert_eq!(
            names,
            [
                "$REGISTRY_AUTH_FILE",
                "$XDG_RUNTIME_DIR/containers/auth.json",
                "~/.config/containers/auth.json",
                "~/.docker/config.json"
            ]
        );
        assert!(bare.auth_locations.iter().all(|l| l.path.is_none()));

        // The variables that override the home-folder defaults are named when they apply.
        let overridden = Settings::from_env(|name| match name {
            "HOME" => Some("/home/me".into()),
            "XDG_CONFIG_HOME" => Some("/cfg".into()),
            "DOCKER_CONFIG" => Some("/dock".into()),
            _ => None,
        });
        let shown: Vec<_> = overridden.auth_locations[2..]
            .iter()
            .map(|l| (l.name, l.path.clone().unwrap()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "$XDG_CONFIG_HOME/containers/auth.json",
                    PathBuf::from("/cfg/containers/auth.json")
                ),
                ("$DOCKER_CONFIG/config.json", PathBuf::from("/dock/config.json")),
            ]
        );
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
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
    }
}
