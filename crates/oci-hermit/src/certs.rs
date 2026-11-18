//! Per-registry CA certificates and client certificates, from the `certs.d` folders Podman and Docker use (#86).
//!
//! A registry's folder (`<certs.d>/<host:port>/`) holds `*.crt` CA certificates, trusted for that registry on top of
//! the system's, and at most one `*.cert` client certificate with its `*.key`. Key files are read, never printed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One `certs.d` folder that is searched: how it is named, and the folder if the variables it needs are set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertLocation {
    pub name: &'static str,
    pub path: Option<PathBuf>,
}

/// The `certs.d` folders, in the order they are searched, from environment variables read through `var`.
pub fn locations(var: impl Fn(&str) -> Option<OsString>) -> Vec<CertLocation> {
    let path = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = path("HOME").or_else(|| path("USERPROFILE"));
    let podman_user = match path("XDG_CONFIG_HOME") {
        Some(dir) => CertLocation {
            name: "$XDG_CONFIG_HOME/containers/certs.d",
            path: Some(dir.join("containers/certs.d")),
        },
        None => CertLocation {
            name: "~/.config/containers/certs.d",
            path: home.as_ref().map(|h| h.join(".config/containers/certs.d")),
        },
    };
    let mut found = vec![podman_user];
    if cfg!(unix) {
        found.push(CertLocation {
            name: "/etc/containers/certs.d",
            path: Some(PathBuf::from("/etc/containers/certs.d")),
        });
    }
    found.push(CertLocation {
        name: "~/.docker/certs.d",
        path: home.as_ref().map(|h| h.join(".docker/certs.d")),
    });
    if cfg!(windows) {
        found.push(CertLocation {
            name: "%ProgramData%\\docker\\certs.d",
            path: path("ProgramData").map(|d| d.join("docker").join("certs.d")),
        });
    } else {
        found.push(CertLocation {
            name: "/etc/docker/certs.d",
            path: Some(PathBuf::from("/etc/docker/certs.d")),
        });
    }
    found
}

/// The folder names a registry's certificates may be under: `host:port`, and on Windows, where a name cannot hold
/// a colon, `hostport` as Docker spells it there.
pub fn folder_names(registry: &str) -> Vec<String> {
    let mut names = vec![registry.to_string()];
    if cfg!(windows) && registry.contains(':') {
        names.push(registry.replace(':', ""));
    }
    names
}

/// What one registry's folder holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCerts {
    pub dir: PathBuf,
    /// CA certificates (`*.crt`), in name order.
    pub cas: Vec<PathBuf>,
    /// The client certificate (`*.cert`) and its key (`*.key`).
    pub client: Option<(PathBuf, PathBuf)>,
}

impl RegistryCerts {
    /// `2 CAs, a client certificate`, for `machine info`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match self.cas.len() {
            0 => {}
            1 => parts.push("1 CA".to_string()),
            n => parts.push(format!("{n} CAs")),
        }
        if self.client.is_some() {
            parts.push("a client certificate".into());
        }
        if parts.is_empty() {
            "empty".into()
        } else {
            parts.join(", ")
        }
    }

    /// Warnings about the folder: a private key other users can read.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        #[cfg(unix)]
        if let Some((_, key)) = &self.client {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(key)
                && meta.permissions().mode() & 0o077 != 0
            {
                warnings.push(format!(
                    "WARNING: {} can be read by other users; restrict it with `chmod 600`",
                    key.display()
                ));
            }
        }
        warnings
    }

    /// Adds the CAs and the client certificate to `builder`. Errors name the file, never its contents.
    pub fn apply(&self, mut builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder, String> {
        let read = |path: &Path| std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()));
        for ca in &self.cas {
            let certs = reqwest::Certificate::from_pem_bundle(&read(ca)?)
                .ok()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| format!("{} is not a PEM certificate", ca.display()))?;
            builder = builder.tls_certs_merge(certs);
        }
        if let Some((cert, key)) = &self.client {
            let mut pem = read(cert)?;
            pem.push(b'\n');
            pem.extend(read(key)?);
            let identity = reqwest::Identity::from_pem(&pem).map_err(|_| {
                format!(
                    "{} and {} are not a PEM certificate and its private key",
                    cert.display(),
                    key.display()
                )
            })?;
            builder = builder.identity(identity);
        }
        Ok(builder)
    }
}

/// Reads a registry's folder. A `.cert` without its `.key`, a `.key` without its `.cert`, or more than one client
/// certificate is an error, so a half-copied setup does not fail later as a puzzling TLS error.
pub fn read_dir(dir: &Path) -> Result<RegistryCerts, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    let has = |path: &Path, ext: &str| path.extension().is_some_and(|e| e == ext);
    let cas = files.iter().filter(|f| has(f, "crt")).cloned().collect();
    let mut clients = Vec::new();
    for cert in files.iter().filter(|f| has(f, "cert")) {
        let key = cert.with_extension("key");
        if !key.is_file() {
            return Err(format!(
                "{} has no matching {}; a client certificate needs its key",
                cert.display(),
                key.display()
            ));
        }
        clients.push((cert.clone(), key));
    }
    for key in files.iter().filter(|f| has(f, "key")) {
        if !key.with_extension("cert").is_file() {
            return Err(format!(
                "{} has no matching {}",
                key.display(),
                key.with_extension("cert").display()
            ));
        }
    }
    if clients.len() > 1 {
        return Err(format!(
            "{} holds more than one client certificate (*.cert)",
            dir.display()
        ));
    }
    Ok(RegistryCerts {
        dir: dir.to_path_buf(),
        cas,
        client: clients.pop(),
    })
}

/// The certificates for `registry`: from `cert_dir` (`--cert-dir`, whose files are used directly) when given, else
/// from the first `certs.d` location with a folder for the registry. `None` when there are none.
pub fn find(
    locations: &[CertLocation],
    cert_dir: Option<&Path>,
    registry: &str,
) -> Result<Option<RegistryCerts>, String> {
    if let Some(dir) = cert_dir {
        return read_dir(dir).map(Some);
    }
    for location in locations {
        let Some(base) = &location.path else { continue };
        for name in folder_names(registry) {
            let dir = base.join(name);
            if dir.is_dir() {
                return read_dir(&dir).map(Some);
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;

    fn touch(dir: &Path, names: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), "x").unwrap();
        }
    }

    #[test]
    fn locations_follow_podman_then_docker() {
        let set = |name: &str| {
            Some(OsString::from(match name {
                "HOME" => "/home/me",
                "ProgramData" => "C:\\ProgramData",
                _ => return None,
            }))
        };
        let names: Vec<_> = locations(set).iter().map(|l| l.name).collect();
        let expected: &[&str] = if cfg!(windows) {
            &[
                "~/.config/containers/certs.d",
                "~/.docker/certs.d",
                "%ProgramData%\\docker\\certs.d",
            ]
        } else {
            &[
                "~/.config/containers/certs.d",
                "/etc/containers/certs.d",
                "~/.docker/certs.d",
                "/etc/docker/certs.d",
            ]
        };
        assert_eq!(names, expected);
        assert_eq!(
            locations(set)[0].path,
            Some(PathBuf::from("/home/me/.config/containers/certs.d"))
        );
        let xdg = locations(|name| (name == "XDG_CONFIG_HOME").then(|| "/cfg".into()));
        assert_eq!(xdg[0].name, "$XDG_CONFIG_HOME/containers/certs.d");
        assert_eq!(xdg[0].path, Some(PathBuf::from("/cfg/containers/certs.d")));
        // Without a home folder, the per-user folders are not searched.
        assert_eq!(locations(|_| None)[0].path, None);
        assert_eq!(folder_names("reg.corp:5000")[0], "reg.corp:5000");
        assert_eq!(folder_names("reg.corp:5000").len(), if cfg!(windows) { 2 } else { 1 });
        assert_eq!(folder_names("reg.corp"), ["reg.corp"]);
    }

    #[test]
    fn folders_are_read_by_the_certs_d_convention() {
        let dir = scratch("certs-read");
        touch(
            &dir.join("ok"),
            &["b.crt", "a.crt", "client.cert", "client.key", "notes.txt"],
        );
        let certs = read_dir(&dir.join("ok")).unwrap();
        assert_eq!(certs.cas, [dir.join("ok/a.crt"), dir.join("ok/b.crt")]);
        assert_eq!(
            certs.client,
            Some((dir.join("ok/client.cert"), dir.join("ok/client.key")))
        );
        assert_eq!(certs.summary(), "2 CAs, a client certificate");

        touch(&dir.join("ca-only"), &["ca.crt"]);
        assert_eq!(read_dir(&dir.join("ca-only")).unwrap().summary(), "1 CA");
        touch(&dir.join("empty"), &[]);
        assert_eq!(read_dir(&dir.join("empty")).unwrap().summary(), "empty");

        touch(&dir.join("no-key"), &["client.cert"]);
        let err = read_dir(&dir.join("no-key")).unwrap_err();
        assert!(err.contains("client.key") && err.contains("needs its key"), "{err}");
        touch(&dir.join("no-cert"), &["client.key"]);
        assert!(read_dir(&dir.join("no-cert")).unwrap_err().contains("client.cert"));
        touch(&dir.join("two"), &["a.cert", "a.key", "b.cert", "b.key"]);
        assert!(read_dir(&dir.join("two")).unwrap_err().contains("more than one"));
        assert!(read_dir(&dir.join("missing")).is_err());
    }

    #[test]
    fn the_first_folder_for_the_registry_wins() {
        let dir = scratch("certs-find");
        let location = |name: &str| CertLocation {
            name: "test",
            path: Some(dir.join(name)),
        };
        let registry = "reg.example:5000";
        let folder = folder_names(registry).pop().unwrap();
        touch(&dir.join("second").join(&folder), &["second.crt"]);
        touch(&dir.join("third").join(&folder), &["third.crt"]);
        touch(&dir.join("first").join("other.example"), &["other.crt"]);
        let all = [
            CertLocation {
                name: "unset",
                path: None,
            },
            location("first"),
            location("second"),
            location("third"),
        ];
        let found = find(&all, None, registry).unwrap().unwrap();
        assert_eq!(found.cas, [dir.join("second").join(&folder).join("second.crt")]);
        assert_eq!(find(&all, None, "nowhere.example").unwrap(), None);
        // --cert-dir is used as it is, ahead of every certs.d folder.
        touch(&dir.join("given"), &["given.crt"]);
        let given = find(&all, Some(&dir.join("given")), registry).unwrap().unwrap();
        assert_eq!(given.cas, [dir.join("given/given.crt")]);
        assert!(find(&all, Some(&dir.join("absent")), registry).is_err());
    }

    #[test]
    fn bad_files_are_named_without_their_contents() {
        let dir = scratch("certs-bad");
        std::fs::write(dir.join("ca.crt"), "not a certificate").unwrap();
        let certs = read_dir(&dir).unwrap();
        let err = certs.apply(reqwest::Client::builder()).err().unwrap();
        assert!(err.contains("ca.crt") && err.contains("not a PEM certificate"), "{err}");

        let key_dir = scratch("certs-bad-key");
        std::fs::write(key_dir.join("c.cert"), "junk").unwrap();
        std::fs::write(key_dir.join("c.key"), "TOP-SECRET-KEY").unwrap();
        let err = read_dir(&key_dir)
            .unwrap()
            .apply(reqwest::Client::builder())
            .err()
            .unwrap();
        assert!(err.contains("c.key") && !err.contains("TOP-SECRET"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_others_can_read_is_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("certs-mode");
        touch(&dir, &["c.cert", "c.key"]);
        let key = dir.join("c.key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let certs = read_dir(&dir).unwrap();
        let warnings = certs.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("chmod 600"), "{warnings:?}");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(certs.warnings().is_empty());
    }
}
