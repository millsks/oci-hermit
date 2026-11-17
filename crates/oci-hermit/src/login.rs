//! `login` and `logout`, compatible with Podman: credentials go in Podman's `auth.json`, where `pull` (and Podman,
//! Docker and skopeo) read them.
//!
//! The registry is asked before anything is written, the way `docker login` does it: `/v2/` with Basic auth, and
//! for a registry that answers with a Bearer challenge, its token server with the same credentials. Passwords are
//! never printed or logged.

use crate::registry::{self, Settings};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The auth file `login` writes to, as Podman picks it: `$REGISTRY_AUTH_FILE`, else
/// `$XDG_RUNTIME_DIR/containers/auth.json`, else `~/.config/containers/auth.json` (or under `$XDG_CONFIG_HOME`).
pub fn login_file(settings: &Settings) -> Option<PathBuf> {
    settings.auth_locations.iter().take(3).find_map(|l| l.path.clone())
}

/// The registry as auth files name it: the host and port, with `docker.io` for Docker Hub.
pub fn registry_name(given: &str) -> Result<String, String> {
    let name = given
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let name = match name {
        "" | "index.docker.io" | "registry-1.docker.io" | "index.docker.io/v1" => "docker.io",
        other => other,
    };
    if name.contains('/') || name.contains(char::is_whitespace) {
        return Err(format!(
            "{given:?} is not a registry; give its host name, e.g. ghcr.io or registry.example.com:5000"
        ));
    }
    Ok(name.to_ascii_lowercase())
}

/// Why a login did not go through.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    /// The registry turned the user name or password down.
    Rejected(String),
    /// The registry could not be asked, or answered something unexpected.
    Registry(String),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::Rejected(registry) => {
                write!(
                    f,
                    "login to {registry} failed: the registry rejected the user name or password"
                )
            }
            LoginError::Registry(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoginError {}

/// The `key="value"` pairs of a `WWW-Authenticate: Bearer ...` header.
fn bearer_challenge(header: &str) -> Option<Vec<(String, String)>> {
    let rest = header
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| header.trim().strip_prefix("bearer "))?;
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

/// Checks `user` and `password` with the registry, as `docker login` does.
pub fn verify(registry_host: &str, user: &str, password: &str, settings: &Settings) -> Result<(), LoginError> {
    let unreachable = |e: String| LoginError::Registry(format!("asking {registry_host} to log in: {e}"));
    registry::install_crypto();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| unreachable(e.to_string()))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| unreachable(e.to_string()))?;
    let scheme = if settings.insecure.iter().any(|r| r == registry_host) {
        "http"
    } else {
        "https"
    };
    // Docker Hub's API lives on another host than its name.
    let host = if registry_host == "docker.io" {
        "registry-1.docker.io"
    } else {
        registry_host
    };
    runtime.block_on(async {
        let answer = client
            .get(format!("{scheme}://{host}/v2/"))
            .basic_auth(user, Some(password))
            .send()
            .await
            .map_err(|e| unreachable(e.to_string()))?;
        match answer.status().as_u16() {
            200..=299 => return Ok(()),
            401 => {}
            status => return Err(unreachable(format!("{host} answered {status}"))),
        }
        let header = answer
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let Some(challenge) = bearer_challenge(&header) else {
            // A Basic challenge after we sent Basic credentials: they are wrong.
            return Err(LoginError::Rejected(registry_host.to_string()));
        };
        let value = |key: &str| challenge.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let realm =
            value("realm").ok_or_else(|| unreachable(format!("{host} sent a token challenge with no realm")))?;
        let mut query = vec![("account".to_string(), user.to_string())];
        query.extend(value("service").map(|s| ("service".to_string(), s)));
        let token = client
            .get(&realm)
            .query(&query)
            .basic_auth(user, Some(password))
            .send()
            .await
            .map_err(|e| unreachable(e.to_string()))?;
        match token.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => Err(LoginError::Rejected(registry_host.to_string())),
            status => Err(unreachable(format!("its token server {realm} answered {status}"))),
        }
    })
}

fn read(file: &Path) -> Result<Value, String> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

/// Writes `value` to `file` through a temporary file, readable by the user only on Unix. On Windows the file is
/// under the user's profile, which only they (and administrators) can read.
fn write(file: &Path, value: &Value) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", file.display());
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(fail)?;
    }
    let mut temp = file.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let text = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let written = options
        .open(&temp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &text));
    written
        .and_then(|()| std::fs::rename(&temp, file))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .map_err(fail)
}

/// Stores the login in `file`, keeping everything else in it. Refuses a registry whose logins a credential helper
/// keeps in that file, rather than store a second, competing login.
pub fn store(file: &Path, registry_host: &str, user: &str, password: &str) -> Result<(), String> {
    let mut auth = read(file)?;
    if !auth.is_object() {
        return Err(format!("{} is not an auth file", file.display()));
    }
    if let Some(helper) = auth["credHelpers"][registry_host].as_str() {
        return Err(format!(
            "{} keeps logins for {registry_host} in docker-credential-{helper}; log in with that tool instead",
            file.display()
        ));
    }
    if !auth["auths"].is_object() {
        auth["auths"] = json!({});
    }
    let secret = hermit_proto::b64::encode(format!("{user}:{password}").as_bytes());
    auth["auths"][registry_host] = json!({ "auth": secret });
    write(file, &auth)
}

/// The user name stored in `file` for `registry_host`, if any.
pub fn user(file: &Path, registry_host: &str) -> Option<String> {
    let auth = read(file).ok()?;
    registry::keys(registry_host).iter().find_map(|key| {
        let encoded = auth["auths"][key]["auth"].as_str()?;
        let decoded = String::from_utf8(hermit_proto::b64::decode(encoded)?).ok()?;
        decoded.split_once(':').map(|(user, _)| user.to_string())
    })
}

/// Removes the login for `registry_host` (under any of the names auth files use for it) from `file`; returns whether
/// there was one.
pub fn remove(file: &Path, registry_host: &str) -> Result<bool, String> {
    let mut auth = read(file)?;
    let Some(auths) = auth["auths"].as_object_mut() else {
        return Ok(false);
    };
    let before = auths.len();
    for key in registry::keys(registry_host) {
        auths.remove(&key);
    }
    let removed = auths.len() < before;
    if removed {
        write(file, &auth)?;
    }
    Ok(removed)
}

/// Removes every login from `file`; returns how many there were.
pub fn remove_all(file: &Path) -> Result<usize, String> {
    let mut auth = read(file)?;
    let count = auth["auths"].as_object().map_or(0, serde_json::Map::len);
    if count > 0 {
        auth["auths"] = json!({});
        write(file, &auth)?;
    }
    Ok(count)
}

/// Another auth file that has a login for `registry_host`, for telling the user where a login they cannot remove
/// here comes from.
pub fn elsewhere(settings: &Settings, except: &Path, registry_host: &str) -> Option<PathBuf> {
    settings
        .auth_files
        .iter()
        .filter(|f| f.as_path() != except)
        .find(|f| {
            read(f).is_ok_and(|auth| {
                registry::keys(registry_host)
                    .iter()
                    .any(|k| auth["auths"][k].is_object() || auth["credHelpers"][k].is_string())
            })
        })
        .cloned()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A registry that wants a login: Basic on `/v2/` itself, or a Bearer challenge pointing at its token server.
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Serves until the test process ends and returns `127.0.0.1:<port>`. `secret` is `user:password` in base64.
    pub(crate) fn registry(secret: &str, bearer: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (secret, realm) = (secret.to_string(), format!("http://{address}/token"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                let mut authorization = String::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        authorization = value.trim().to_string();
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let good = authorization == format!("Basic {secret}");
                let (status, extra, body) = match (path, bearer) {
                    ("/v2/", false) if good => ("200 OK", String::new(), "{}"),
                    ("/v2/", false) => (
                        "401 Unauthorized",
                        "WWW-Authenticate: Basic realm=\"test\"\r\n".into(),
                        "",
                    ),
                    ("/v2/", true) => (
                        "401 Unauthorized",
                        format!("WWW-Authenticate: Bearer realm=\"{realm}\",service=\"test-registry\"\r\n"),
                        "",
                    ),
                    (p, true) if p.starts_with("/token?") && p.contains("service=test-registry") && good => {
                        ("200 OK", String::new(), r#"{"token":"t"}"#)
                    }
                    (p, true) if p.starts_with("/token?") => ("401 Unauthorized", String::new(), ""),
                    _ => ("500 Internal Server Error", String::new(), ""),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
                    body.len()
                );
            }
        });
        address
    }
}

#[cfg(test)]
mod tests {
    use super::testing::registry;
    use super::*;
    use crate::qemu::testing::scratch;

    fn secret(user: &str, password: &str) -> String {
        hermit_proto::b64::encode(format!("{user}:{password}").as_bytes())
    }

    fn plain_http(address: &str) -> Settings {
        Settings {
            insecure: vec![address.to_string()],
            ..Settings::default()
        }
    }

    #[test]
    fn the_login_file_is_podmans() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| (*v).into())
        };
        let settings = Settings::from_env(env(&[("HOME", "/home/me")]));
        assert_eq!(
            login_file(&settings),
            Some(PathBuf::from("/home/me/.config/containers/auth.json"))
        );
        let settings = Settings::from_env(env(&[("HOME", "/home/me"), ("XDG_RUNTIME_DIR", "/run/user/1")]));
        assert_eq!(
            login_file(&settings),
            Some(PathBuf::from("/run/user/1/containers/auth.json"))
        );
        let settings = Settings::from_env(env(&[("HOME", "/h"), ("REGISTRY_AUTH_FILE", "/a.json")]));
        assert_eq!(login_file(&settings), Some(PathBuf::from("/a.json")));
        // Docker's own file is read but never written.
        let settings = Settings::from_env(env(&[("DOCKER_CONFIG", "/d")]));
        assert_eq!(login_file(&settings), None);
    }

    #[test]
    fn registries_are_named_as_auth_files_name_them() {
        assert_eq!(registry_name("ghcr.io").unwrap(), "ghcr.io");
        assert_eq!(
            registry_name("https://Registry.Example.com:5000/").unwrap(),
            "registry.example.com:5000"
        );
        for hub in [
            "",
            "index.docker.io",
            "https://index.docker.io/v1/",
            "registry-1.docker.io",
        ] {
            assert_eq!(registry_name(hub).unwrap(), "docker.io", "{hub:?}");
        }
        assert!(registry_name("ghcr.io/org/app").unwrap_err().contains("not a registry"));
    }

    #[test]
    fn challenges_are_parsed() {
        let pairs = bearer_challenge(r#"Bearer realm="https://auth.example/token",service="reg", scope="a:b:pull""#);
        assert_eq!(
            pairs.unwrap(),
            [
                ("realm".to_string(), "https://auth.example/token".to_string()),
                ("service".to_string(), "reg".to_string()),
                ("scope".to_string(), "a:b:pull".to_string()),
            ]
        );
        assert_eq!(
            bearer_challenge("Bearer realm=plain").unwrap(),
            [("realm".into(), "plain".into())]
        );
        assert!(bearer_challenge(r#"Basic realm="x""#).is_none());
        assert!(bearer_challenge(r#"Bearer realm="unterminated"#).is_none());
    }

    #[test]
    fn verify_asks_basic_and_token_registries() {
        for bearer in [false, true] {
            let address = registry(&secret("alice", "s3cret"), bearer);
            let host = address.as_str();
            let settings = plain_http(host);
            assert_eq!(verify(host, "alice", "s3cret", &settings), Ok(()), "bearer {bearer}");
            assert_eq!(
                verify(host, "alice", "wrong", &settings),
                Err(LoginError::Rejected(host.to_string())),
                "bearer {bearer}"
            );
        }
        // Nothing listening: the registry cannot be asked.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let err = verify(&closed, "a", "b", &plain_http(&closed)).unwrap_err();
        assert!(matches!(err, LoginError::Registry(_)), "{err}");
        assert!(
            LoginError::Rejected("r".into())
                .to_string()
                .contains("rejected the user name or password")
        );
    }

    #[test]
    fn logins_are_stored_read_and_removed() {
        let dir = scratch("login-store");
        let file = dir.join("containers/auth.json");
        assert_eq!(user(&file, "ghcr.io"), None);
        assert!(!remove(&file, "ghcr.io").unwrap());

        store(&file, "ghcr.io", "alice", "pa:ss").unwrap();
        store(&file, "docker.io", "bob", "x").unwrap();
        assert_eq!(user(&file, "ghcr.io").as_deref(), Some("alice"));
        assert_eq!(user(&file, "docker.io").as_deref(), Some("bob"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(file.parent().unwrap()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        // pull reads what login wrote.
        let settings = Settings {
            auth_files: vec![file.clone()],
            ..Settings::default()
        };
        let none = |_: &str, _: &str| None;
        assert_eq!(
            registry::credentials("ghcr.io", &settings, none),
            oci_client::secrets::RegistryAuth::Basic("alice".into(), "pa:ss".into())
        );

        // Other keys in the file are kept.
        let mut auth: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        auth["credHelpers"] = json!({ "quay.io": "secretservice" });
        std::fs::write(&file, serde_json::to_vec(&auth).unwrap()).unwrap();
        store(&file, "ghcr.io", "carol", "y").unwrap();
        let kept: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(kept["credHelpers"]["quay.io"], "secretservice");
        let err = store(&file, "quay.io", "a", "b").unwrap_err();
        assert!(err.contains("docker-credential-secretservice"), "{err}");

        assert!(remove(&file, "ghcr.io").unwrap());
        assert_eq!(user(&file, "ghcr.io"), None);
        assert_eq!(remove_all(&file).unwrap(), 1);
        assert_eq!(remove_all(&file).unwrap(), 0);

        std::fs::write(&file, "not json").unwrap();
        assert!(store(&file, "ghcr.io", "a", "b").is_err());
        std::fs::write(&file, "[]").unwrap();
        assert!(
            store(&file, "ghcr.io", "a", "b")
                .unwrap_err()
                .contains("not an auth file")
        );
    }

    #[test]
    fn a_login_from_another_tool_is_found() {
        let dir = scratch("login-elsewhere");
        let ours = dir.join("auth.json");
        let docker = dir.join("config.json");
        std::fs::write(&docker, r#"{"auths":{"https://index.docker.io/v1/":{"auth":"eDp5"}}}"#).unwrap();
        let settings = Settings {
            auth_files: vec![ours.clone(), docker.clone()],
            ..Settings::default()
        };
        assert_eq!(elsewhere(&settings, &ours, "docker.io"), Some(docker));
        assert_eq!(elsewhere(&settings, &ours, "ghcr.io"), None);
    }
}
