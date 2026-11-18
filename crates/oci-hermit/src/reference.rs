//! Image references as Docker and Podman read them: `debian` is `docker.io/library/debian:latest`.

/// A parsed image reference: registry, repository, and a tag, a digest or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

/// Whether `component` is one path component of a repository: lowercase letters and digits, joined by `.`, `_`,
/// `__` or runs of `-`.
fn valid_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if bytes.is_empty() || !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if alnum(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !alnum(bytes[i]) {
            i += 1;
        }
        let separator = &component[start..i];
        if !(separator == "." || separator == "_" || separator == "__" || separator.bytes().all(|b| b == b'-')) {
            return false;
        }
    }
    true
}

fn valid_tag(tag: &str) -> bool {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-';
    let bytes = tag.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes.iter().all(|&b| ok(b))
}

fn valid_digest(digest: &str) -> bool {
    digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn valid_registry(registry: &str) -> bool {
    !registry.is_empty()
        && registry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

impl std::str::FromStr for Reference {
    type Err = String;

    fn from_str(given: &str) -> Result<Self, String> {
        let (rest, digest) = match given.split_once('@') {
            Some((rest, digest)) if valid_digest(digest) => (rest, Some(digest.to_string())),
            Some(_) => return Err("the digest is not sha256:<64 hex digits>".into()),
            None => (given, None),
        };
        let last_slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => {
                let tag = &rest[last_slash + i + 1..];
                if !valid_tag(tag) {
                    return Err(format!("{tag:?} is not a valid tag"));
                }
                (&rest[..last_slash + i], Some(tag.to_string()))
            }
            None => (rest, None),
        };
        // The first component names a registry when it looks like a host: a dot, a port, or `localhost`.
        let (registry, repository) = match name.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let registry = match registry.as_str() {
            "index.docker.io" | "registry-1.docker.io" => "docker.io".to_string(),
            _ => registry,
        };
        if !valid_registry(&registry) {
            return Err(format!("{registry:?} is not a registry host"));
        }
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if repository.split('/').any(|c| !valid_component(c)) {
            return Err(format!(
                "{repository:?} is not a repository name (lowercase letters, digits and . _ - separators)"
            ));
        }
        let tag = if tag.is_none() && digest.is_none() {
            Some("latest".to_string())
        } else {
            tag
        };
        Ok(Reference {
            registry,
            repository,
            tag,
            digest,
        })
    }
}

impl Reference {
    /// The registry as the reference names it (`docker.io`, `ghcr.io`, `localhost:5000`).
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The host the registry's API is served from: Docker Hub's is not `docker.io`.
    pub fn api_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    /// What to ask the registry's manifest endpoint for: the digest if there is one, else the tag.
    pub fn target(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }

    /// The reference spelled out in full: `docker.io/library/debian:latest`.
    pub fn whole(&self) -> String {
        let mut whole = format!("{}/{}", self.registry, self.repository);
        if let Some(tag) = &self.tag {
            whole.push(':');
            whole.push_str(tag);
        }
        if let Some(digest) = &self.digest {
            whole.push('@');
            whole.push_str(digest);
        }
        whole
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.whole())
    }
}

#[cfg(test)]
mod tests {
    use super::Reference;

    fn parse(s: &str) -> Result<Reference, String> {
        s.parse()
    }

    #[test]
    fn references_are_read_as_docker_reads_them() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let cases = [
            ("debian", "docker.io/library/debian:latest"),
            ("debian:bookworm-slim", "docker.io/library/debian:bookworm-slim"),
            ("library/debian", "docker.io/library/debian:latest"),
            ("index.docker.io/library/debian", "docker.io/library/debian:latest"),
            ("someone/app:1.0", "docker.io/someone/app:1.0"),
            ("ghcr.io/org/team/app:1.2", "ghcr.io/org/team/app:1.2"),
            ("localhost/app", "localhost/app:latest"),
            ("localhost:5000/app:v_1", "localhost:5000/app:v_1"),
            ("127.0.0.1:5000/test/app:1", "127.0.0.1:5000/test/app:1"),
            (
                "my-registry.corp/a.b/c__d/e---f",
                "my-registry.corp/a.b/c__d/e---f:latest",
            ),
        ];
        for (given, whole) in cases {
            assert_eq!(parse(given).unwrap().whole(), whole, "{given}");
        }
        let pinned = parse(&format!("ghcr.io/org/app@{digest}")).unwrap();
        assert_eq!(pinned.tag(), None);
        assert_eq!(pinned.digest(), Some(digest.as_str()));
        assert_eq!(pinned.target(), digest);
        assert_eq!(pinned.to_string(), format!("ghcr.io/org/app@{digest}"));
        let both = parse(&format!("debian:12@{digest}")).unwrap();
        assert_eq!(both.whole(), format!("docker.io/library/debian:12@{digest}"));
        assert_eq!(both.target(), digest);

        let hub = parse("debian").unwrap();
        assert_eq!(
            (hub.registry(), hub.api_host(), hub.repository(), hub.tag()),
            ("docker.io", "registry-1.docker.io", "library/debian", Some("latest"))
        );
        assert_eq!(parse("quay.io/a/b").unwrap().api_host(), "quay.io");
    }

    #[test]
    fn malformed_references_are_refused() {
        for bad in [
            "",
            "Not A Reference",
            "Debian",
            "debian:",
            "debian:-x",
            "debian@sha256:123",
            "debian@md5:abc",
            "a//b",
            "a/-b",
            "a/b_",
            "ghcr.io/",
            "ex ample.com/app",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} parsed");
        }
        assert!(parse(&format!("app:{}", "t".repeat(129))).is_err());
    }
}
