//! Image commands over the store, as Docker and Podman spell them: `images`, `rmi`, `inspect`, `image prune`, `tag`,
//! `save`, `image history`, `image exists` and `system df`.
//!
//! They only read and write the store, a folder the user owns: no sandbox and no network.

use crate::image::{self, ImageError};
use crate::qemu::GuestArch;
use crate::registry;
use crate::store::{Entry, Store};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

/// One stored image: an index entry with its manifest and config read and checked.
#[derive(Debug, Clone)]
pub struct Stored {
    pub entry: Entry,
    /// The config's digest, which Docker and Podman call the image ID.
    pub id: String,
    pub config: Value,
    /// Each layer's digest and size, bottom first.
    pub layers: Vec<(String, u64)>,
    /// Manifest, config and layers together, as stored.
    pub size: u64,
}

impl Stored {
    /// `docker.io/library/alpine` and `latest`, or `<none>` for what the name does not say.
    pub fn repository_and_tag(&self) -> (String, String) {
        split_name(&self.entry.name)
    }

    /// Whether the entry is named by an image ID, as `load` names an image that came without a name.
    pub fn is_unnamed(&self) -> bool {
        self.entry.name.starts_with("sha256:")
    }

    fn platform(&self) -> String {
        format!("{}/{}", self.entry.os, self.entry.arch)
    }

    fn created(&self) -> Option<&str> {
        self.config["created"].as_str()
    }
}

/// The repository and tag a stored name is shown as.
pub fn split_name(name: &str) -> (String, String) {
    let none = || "<none>".to_string();
    if name.starts_with("sha256:") {
        return (none(), none());
    }
    match registry::normalize(name) {
        Ok(r) => (
            format!("{}/{}", r.registry(), r.repository()),
            r.tag().map_or_else(none, String::from),
        ),
        Err(_) => (name.to_string(), none()),
    }
}

/// An image ID as Docker shows it: the digest's first 12 hex digits, or all of them with `--no-trunc`.
pub fn short_id(id: &str, no_trunc: bool) -> String {
    if no_trunc {
        return id.to_string();
    }
    let hex = id.trim_start_matches("sha256:");
    hex[..hex.len().min(12)].to_string()
}

fn parse(what: &str, bytes: &[u8]) -> Result<Value, ImageError> {
    serde_json::from_slice(bytes).map_err(|e| ImageError::Invalid(format!("{what}: {e}")))
}

/// Reads an entry's manifest and config from the store.
pub fn read(store: &Store, entry: &Entry) -> Result<Stored, ImageError> {
    let manifest = parse("manifest", &store.blob(&entry.digest)?)?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| ImageError::Invalid(format!("manifest {} has no config", entry.digest)))?;
    let config_bytes = store.blob(config_digest)?;
    let layers: Vec<(String, u64)> = manifest["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| Some((l["digest"].as_str()?.to_string(), l["size"].as_u64().unwrap_or(0))))
        .collect();
    let size = entry.size + config_bytes.len() as u64 + layers.iter().map(|(_, s)| s).sum::<u64>();
    Ok(Stored {
        entry: entry.clone(),
        id: config_digest.to_string(),
        config: parse("image config", &config_bytes)?,
        layers,
        size,
    })
}

/// Every image in the store.
pub fn all(store: &Store) -> Result<Vec<Stored>, ImageError> {
    store.entries()?.iter().map(|e| read(store, e)).collect()
}

/// How a name given on the command line picked its images.
#[derive(Debug, PartialEq, Eq)]
pub enum Matched {
    /// By a stored name (every platform stored under it).
    Name(String),
    /// By an image ID or a unique prefix of one.
    Id(String),
}

/// What a name on the command line does not pick.
#[derive(Debug, PartialEq, Eq)]
pub enum LookupError {
    NotFound(String),
    Ambiguous(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookupError::NotFound(name) => write!(f, "no such image: {name}"),
            LookupError::Ambiguous(prefix) => write!(f, "{prefix} matches more than one image ID; give more of it"),
        }
    }
}

impl std::error::Error for LookupError {}

/// The images `wanted` names: a stored name exactly, the reference as Docker spells it out (`alpine` is
/// `docker.io/library/alpine:latest`), or an image ID or a unique prefix of one (with or without `sha256:`).
pub fn find<'a>(images: &'a [Stored], wanted: &str) -> Result<(Vec<&'a Stored>, Matched), LookupError> {
    let named = |name: &str| -> Vec<&Stored> { images.iter().filter(|i| i.entry.name == name).collect() };
    let exact = named(wanted);
    if !exact.is_empty() {
        return Ok((exact, Matched::Name(wanted.to_string())));
    }
    if let Ok(reference) = registry::normalize(wanted) {
        let whole = reference.whole();
        let found = named(&whole);
        if !found.is_empty() {
            return Ok((found, Matched::Name(whole)));
        }
    }
    let hex = wanted.strip_prefix("sha256:").unwrap_or(wanted);
    if !hex.is_empty() && hex.len() <= 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let found: Vec<&Stored> = images
            .iter()
            .filter(|i| {
                i.id.trim_start_matches("sha256:")
                    .starts_with(&hex.to_ascii_lowercase())
            })
            .collect();
        let mut ids: Vec<&str> = found.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        match ids.as_slice() {
            [] => {}
            [id] => return Ok((found, Matched::Id((*id).to_string()))),
            _ => return Err(LookupError::Ambiguous(wanted.to_string())),
        }
    }
    Err(LookupError::NotFound(wanted.to_string()))
}

/// Of several platforms of one image, the host's if there is one, else the first.
pub fn preferred<'a>(found: &[&'a Stored]) -> &'a Stored {
    let host = GuestArch::host().map(image::oci_arch);
    found
        .iter()
        .find(|i| Some(i.entry.arch.as_str()) == host)
        .unwrap_or(&found[0])
}

/// Seconds since the Unix epoch for an RFC 3339 time such as `2025-10-20T08:15:00.123Z` or `…+02:00`.
pub fn parse_time(text: &str) -> Option<i64> {
    let num = |s: &str| s.parse::<i64>().ok();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-');
    let (year, month, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    if rest.len() < 8 {
        return None;
    }
    let (clock, mut zone) = rest.split_at(8);
    let mut c = clock.splitn(3, ':');
    let (hour, minute, second) = (num(c.next()?)?, num(c.next()?)?, num(c.next()?)?);
    if let Some(fraction) = zone.strip_prefix('.') {
        zone = fraction.trim_start_matches(|ch: char| ch.is_ascii_digit());
    }
    let offset = match zone {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone.get(1..)?.split_once(':')?;
            sign * (num(h)? * 3600 + num(m)? * 60)
        }
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Seconds since the Unix epoch as RFC 3339 in UTC, as Docker writes times: `2025-01-01T00:00:00Z`.
pub fn format_time(seconds: i64) -> String {
    // The civil date from days (Howard Hinnant's algorithm), the inverse of `parse_time`'s.
    let (days, secs) = (seconds.div_euclid(86_400) + 719_468, seconds.rem_euclid(86_400));
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// How long ago, as Docker words it: `3 days ago`, `About an hour ago`.
pub fn ago(seconds: i64) -> String {
    format!("{} ago", duration(seconds))
}

/// A length of time as Docker words it: `3 days`, `About an hour`, `Less than a second`.
pub fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let hours = (s + 1800) / 3600;
    match s {
        0 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        2..=59 => format!("{s} seconds"),
        60..=119 => "About a minute".to_string(),
        120..=3599 => format!("{} minutes", s / 60),
        _ if hours == 1 => "About an hour".to_string(),
        _ if hours < 48 => format!("{hours} hours"),
        _ if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        _ if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        _ if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        _ => format!("{} years", s / 3600 / 24 / 365),
    }
}

fn created_ago(created: Option<&str>, now: i64) -> String {
    created.and_then(parse_time).map_or("N/A".into(), |t| ago(now - t))
}

/// The current time in seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Columns padded to their widest cell plus three spaces, as Docker's tables are.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    let all = std::iter::once(headers.iter().map(|h| (*h).to_string()).collect::<Vec<_>>()).chain(rows.iter().cloned());
    for row in all {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < row.len() {
                line.push_str(&" ".repeat(widths[i] - cell.chars().count() + 3));
            }
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// Options for `images`.
#[derive(Debug, Default)]
pub struct ListOptions<'a> {
    pub reference: Option<&'a str>,
    pub quiet: bool,
    pub digests: bool,
    pub no_trunc: bool,
}

/// `images`: newest first, one row per name and platform.
pub fn list(store: &Store, options: &ListOptions, now: i64, out: &mut impl Write) -> Result<(), ImageError> {
    let mut images = all(store)?;
    if let Some(wanted) = options.reference {
        let by_tag = wanted
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains(':') || last.contains('@'));
        let reference = registry::normalize(wanted).ok();
        images.retain(|i| {
            let (repository, _) = i.repository_and_tag();
            i.entry.name == wanted
                || reference.as_ref().is_some_and(|r| {
                    if by_tag {
                        i.entry.name == r.whole()
                    } else {
                        repository == format!("{}/{}", r.registry(), r.repository())
                    }
                })
        });
    }
    images.sort_by_key(|i| std::cmp::Reverse(i.created().and_then(parse_time).unwrap_or(0)));
    if options.quiet {
        let mut seen = Vec::new();
        for i in &images {
            let id = short_id(&i.id, options.no_trunc);
            if !seen.contains(&id) {
                let _ = writeln!(out, "{id}");
                seen.push(id);
            }
        }
        return Ok(());
    }
    let mut headers = vec!["REPOSITORY", "TAG"];
    if options.digests {
        headers.push("DIGEST");
    }
    headers.extend(["IMAGE ID", "PLATFORM", "CREATED", "SIZE"]);
    let rows: Vec<Vec<String>> = images
        .iter()
        .map(|i| {
            let (repository, tag) = i.repository_and_tag();
            let mut row = vec![repository, tag];
            if options.digests {
                row.push(i.entry.digest.clone());
            }
            row.extend([
                short_id(&i.id, options.no_trunc),
                i.platform(),
                created_ago(i.created(), now),
                registry::size(i.size),
            ]);
            row
        })
        .collect();
    let _ = write!(out, "{}", table(&headers, &rows));
    Ok(())
}

/// `rmi`: removes each name (every platform under it) or image ID, then the blobs nothing uses any more. An ID with
/// several names needs `force`, as in Docker and Podman. Returns whether every name was removed.
pub fn remove(
    store: &Store,
    wanted: &[String],
    force: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<bool, ImageError> {
    let images = all(store)?;
    let before: Vec<String> = images.iter().map(|i| i.id.clone()).collect();
    let mut ok = true;
    for name in wanted {
        let (found, matched) = match find(&images, name) {
            Ok(found) => found,
            Err(e) => {
                let _ = writeln!(err, "oci-hermit: {e}");
                ok = false;
                continue;
            }
        };
        match matched {
            Matched::Name(name) => {
                store.untag(|e| e.name == name)?;
                let _ = writeln!(out, "Untagged: {name}");
            }
            Matched::Id(id) => {
                let mut names: Vec<&str> = found
                    .iter()
                    .filter(|i| !i.is_unnamed())
                    .map(|i| i.entry.name.as_str())
                    .collect();
                names.sort_unstable();
                names.dedup();
                if names.len() > 1 && !force {
                    let _ = writeln!(
                        err,
                        "oci-hermit: image {} has {} names ({}); remove them by name, or use -f to remove them all",
                        short_id(&id, false),
                        names.len(),
                        names.join(", ")
                    );
                    ok = false;
                    continue;
                }
                let digests: Vec<&str> = found.iter().map(|i| i.entry.digest.as_str()).collect();
                store.untag(|e| digests.contains(&e.digest.as_str()))?;
                for name in names {
                    let _ = writeln!(out, "Untagged: {name}");
                }
            }
        }
    }
    let (deleted, _) = store.collect_garbage()?;
    let mut ids: Vec<&String> = before.iter().filter(|id| deleted.contains(id)).collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let _ = writeln!(out, "Deleted: {id}");
    }
    Ok(ok)
}

/// `image prune`: removes the images loaded without a name (or every image with `all`), and any blob nothing uses.
pub fn prune(store: &Store, all_images: bool, out: &mut impl Write) -> Result<(), ImageError> {
    let images = all(store)?;
    let removed = store.untag(|e| all_images || e.name.starts_with("sha256:"))?;
    let (deleted, freed) = store.collect_garbage()?;
    let ids: Vec<&String> =
        images
            .iter()
            .map(|i| &i.id)
            .filter(|id| deleted.contains(id))
            .fold(Vec::new(), |mut v, id| {
                if !v.contains(&id) {
                    v.push(id);
                }
                v
            });
    if !removed.is_empty() || !ids.is_empty() {
        let _ = writeln!(out, "Deleted Images:");
        let mut names: Vec<&str> = removed
            .iter()
            .filter(|e| !e.name.starts_with("sha256:"))
            .map(|e| e.name.as_str())
            .collect();
        names.dedup();
        for name in names {
            let _ = writeln!(out, "untagged: {name}");
        }
        for id in ids {
            let _ = writeln!(out, "deleted: {id}");
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "Total reclaimed space: {}", registry::size(freed));
    Ok(())
}

/// `tag`: names `source`'s images (every platform) `target` as well.
pub fn tag(store: &Store, source: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let images = all(store)?;
    let (found, _) = find(&images, source)?;
    let target = registry::normalize(target)?;
    if target.digest().is_some() {
        return Err(format!("{target}: a tag cannot name a digest").into());
    }
    let target = target.whole();
    let mut done: Vec<String> = Vec::new();
    for i in found {
        let platform = i.platform();
        if done.contains(&platform) {
            continue;
        }
        let e = &i.entry;
        store.tag(&target, &e.os, &e.arch, &e.media_type, &e.digest, e.size)?;
        done.push(platform);
    }
    Ok(())
}

/// `image exists`.
pub fn exists(store: &Store, name: &str) -> Result<bool, ImageError> {
    Ok(find(&all(store)?, name).is_ok())
}

/// `inspect`: a JSON array with one object per name, close to `podman image inspect`. Returns the objects and the
/// names that matched nothing.
pub fn inspect(store: &Store, wanted: &[String]) -> Result<(Vec<Inspected>, Vec<String>), ImageError> {
    let images = all(store)?;
    let mut objects = Vec::new();
    let mut missing = Vec::new();
    for name in wanted {
        let Ok((found, _)) = find(&images, name) else {
            missing.push(name.clone());
            continue;
        };
        let i = preferred(&found);
        let same = images
            .iter()
            .filter(|o| o.entry.digest == i.entry.digest && !o.is_unnamed());
        let mut tags = Vec::new();
        let mut digests = Vec::new();
        for o in same {
            let (repository, tag) = o.repository_and_tag();
            if tag != "<none>" {
                tags.push(o.entry.name.clone());
            }
            let digest = format!("{repository}@{}", o.entry.digest);
            if !digests.contains(&digest) {
                digests.push(digest);
            }
        }
        let c = &i.config;
        objects.push(Inspected {
            id: i.id.clone(),
            digest: i.entry.digest.clone(),
            repo_tags: tags,
            repo_digests: digests,
            created: c["created"].clone(),
            author: c["author"].clone(),
            architecture: i.entry.arch.clone(),
            variant: c["variant"].clone(),
            os: i.entry.os.clone(),
            size: i.size,
            labels: c["config"]["Labels"].clone(),
            config: c["config"].clone(),
            root_fs: c["rootfs"].clone(),
            history: c["history"].clone(),
            manifest_type: i.entry.media_type.clone(),
        });
    }
    Ok((objects, missing))
}

/// One image as `inspect` shows it, in the order `podman image inspect` uses.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Inspected {
    pub id: String,
    pub digest: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    pub created: Value,
    pub author: Value,
    pub architecture: String,
    pub variant: Value,
    pub os: String,
    pub size: u64,
    pub labels: Value,
    pub config: Value,
    #[serde(rename = "RootFS")]
    pub root_fs: Value,
    pub history: Value,
    pub manifest_type: String,
}

/// `image history`: newest step first; the layer each step made, or 0 B for steps that made none.
pub fn history(
    store: &Store,
    name: &str,
    no_trunc: bool,
    quiet: bool,
    now: i64,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let images = all(store)?;
    let (found, _) = find(&images, name)?;
    let i = preferred(&found);
    let mut layers = i.layers.iter();
    let steps: Vec<Value> = match i.config["history"].as_array() {
        Some(steps) if !steps.is_empty() => steps.clone(),
        _ => i.layers.iter().map(|_| json!({})).collect(),
    };
    let mut rows: Vec<Vec<String>> = steps
        .iter()
        .map(|step| {
            let size = if step["empty_layer"].as_bool() == Some(true) {
                0
            } else {
                layers.next().map_or(0, |(_, s)| *s)
            };
            let mut by = step["created_by"].as_str().unwrap_or_default().to_string();
            if !no_trunc && by.chars().count() > 45 {
                by = by.chars().take(44).collect::<String>() + "…";
            }
            vec![
                "<missing>".to_string(),
                created_ago(step["created"].as_str(), now),
                by,
                registry::size(size),
                step["comment"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    rows.reverse();
    if let Some(top) = rows.first_mut() {
        top[0] = short_id(&i.id, no_trunc);
    }
    if quiet {
        for row in &rows {
            let _ = writeln!(out, "{}", row[0]);
        }
    } else {
        let _ = write!(
            out,
            "{}",
            table(&["IMAGE", "CREATED", "CREATED BY", "SIZE", "COMMENT"], &rows)
        );
    }
    Ok(())
}

/// `system df`: what the store holds. There are no containers yet, so every image counts as reclaimable.
pub fn disk_usage(store: Option<&Store>, out: &mut impl Write) -> Result<(), ImageError> {
    let (count, bytes) = match store {
        Some(store) => {
            let mut ids: Vec<String> = all(store)?.into_iter().map(|i| i.id).collect();
            ids.sort();
            ids.dedup();
            let bytes = crate::store::usage(store.path().parent().unwrap_or(store.path())).map_or(0, |u| u.bytes);
            (ids.len(), bytes)
        }
        None => (0, 0),
    };
    let reclaimable = if bytes == 0 {
        registry::size(0)
    } else {
        format!("{} (100%)", registry::size(bytes))
    };
    let rows = vec![
        vec![
            "Images".into(),
            count.to_string(),
            "0".into(),
            registry::size(bytes),
            reclaimable,
        ],
        vec![
            "Containers".into(),
            "0".into(),
            "0".into(),
            registry::size(0),
            registry::size(0),
        ],
    ];
    let _ = write!(
        out,
        "{}",
        table(&["TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"], &rows)
    );
    Ok(())
}

/// Reads a blob while hashing it, so `save` never writes a blob that does not match its digest.
struct Verified<R> {
    inner: R,
    hasher: sha2::Sha256,
}

impl<R: Read> Read for Verified<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// `save`: writes the images to `output` as Docker 25+ does, an OCI image layout with a `manifest.json` beside it,
/// so `docker load`, `podman load`, skopeo (`oci-archive:`) and `oci-hermit load` all read it. Every platform of a
/// name goes in the OCI index; `manifest.json` lists the host's platform of each (or the first), as it holds one.
pub fn save(
    store: &Store,
    wanted: &[String],
    platform: Option<&str>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let images = all(store)?;
    let mut chosen: Vec<&Stored> = Vec::new();
    for name in wanted {
        let (found, _) = find(&images, name)?;
        let found: Vec<&Stored> = found
            .into_iter()
            .filter(|i| platform.is_none_or(|p| i.platform() == p))
            .collect();
        if found.is_empty() {
            return Err(format!("{name} has no {} variant", platform.unwrap_or("matching")).into());
        }
        for i in found {
            if !chosen.iter().any(|c| c.entry == i.entry) {
                chosen.push(i);
            }
        }
    }

    let mut index = Vec::new();
    let mut docker: BTreeMap<String, (Vec<String>, &Stored)> = BTreeMap::new();
    let mut blobs: Vec<(String, u64)> = Vec::new();
    for i in &chosen {
        let mut annotations = serde_json::Map::new();
        if !i.is_unnamed() {
            annotations.insert("io.containerd.image.name".into(), json!(i.entry.name));
            let (_, tag) = i.repository_and_tag();
            if tag != "<none>" {
                annotations.insert(image::REF_NAME.into(), json!(tag));
            }
        }
        index.push(json!({
            "mediaType": i.entry.media_type,
            "digest": i.entry.digest,
            "size": i.entry.size,
            "platform": { "os": i.entry.os, "architecture": i.entry.arch },
            "annotations": annotations,
        }));
        for blob in store.blobs_of(&i.entry)? {
            if !blobs.contains(&blob) {
                blobs.push(blob);
            }
        }
    }
    // manifest.json: one platform per name, grouped by image ID as Docker writes it.
    let host = GuestArch::host().map(image::oci_arch);
    let mut per_name: BTreeMap<&str, &Stored> = BTreeMap::new();
    for i in &chosen {
        let slot = per_name.entry(i.entry.name.as_str()).or_insert(i);
        if Some(i.entry.arch.as_str()) == host {
            *slot = i;
        }
    }
    for (name, i) in per_name {
        let (names, _) = docker.entry(i.id.clone()).or_insert((Vec::new(), i));
        let (_, tag) = split_name(name);
        if tag != "<none>" {
            names.push(name.to_string());
        }
    }
    let blob_path = |digest: &str| format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
    let manifest_json: Vec<Value> = docker
        .values()
        .map(|(names, i)| {
            json!({
                "Config": blob_path(&i.id),
                "RepoTags": names,
                "Layers": i.layers.iter().map(|(d, _)| blob_path(d)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": index,
    });

    let mut temp = output.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = std::path::PathBuf::from(temp);
    let written = write_archive(store, &temp, &blobs, &index, &manifest_json, &blob_path);
    match written.and_then(|()| std::fs::rename(&temp, output).map_err(Into::into)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

fn write_archive(
    store: &Store,
    path: &Path,
    blobs: &[(String, u64)],
    index: &Value,
    manifest_json: &[Value],
    blob_path: &dyn Fn(&str) -> String,
) -> Result<(), Box<dyn std::error::Error>> {
    use sha2::Digest;
    let mut tar = tar::Builder::new(std::io::BufWriter::new(std::fs::File::create(path)?));
    let mut add = |name: &str, bytes: &[u8]| -> std::io::Result<()> {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        tar.append_data(&mut header, name, bytes)
    };
    add("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    add("index.json", &serde_json::to_vec(index)?)?;
    add("manifest.json", &serde_json::to_vec(manifest_json)?)?;
    for (digest, _) in blobs {
        let file = store.path().join(blob_path(digest));
        let size = std::fs::metadata(&file)?.len();
        let mut reader = Verified {
            inner: std::fs::File::open(&file)?,
            hasher: sha2::Sha256::new(),
        };
        let mut header = tar::Header::new_ustar();
        header.set_size(size);
        header.set_mode(0o644);
        tar.append_data(&mut header, blob_path(digest), &mut reader)?;
        let actual = reader
            .hasher
            .finalize()
            .iter()
            .fold(String::from("sha256:"), |mut hex, byte| {
                hex.push_str(&format!("{byte:02x}"));
                hex
            });
        if actual != *digest {
            return Err(ImageError::Mismatch(digest.clone()).into());
        }
    }
    tar.into_inner()?.flush()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::image::testing::{layer, manifest};
    use crate::qemu::testing::scratch;
    use crate::store::Store;
    use std::path::{Path, PathBuf};

    pub(crate) const CONFIG: &str = r#"{"architecture":"ARCH","os":"linux","created":"2025-01-01T00:00:00Z","config":{"Cmd":["sh"],"Labels":{"team":"qa"}},"rootfs":{"type":"layers","diff_ids":[]},"history":[{"created":"2025-01-01T00:00:00Z","created_by":"ADD rootfs.tar / # a long command line that goes on well past forty-five characters","comment":"buildkit"},{"created":"2025-01-01T00:00:00Z","created_by":"CMD [\"sh\"]","empty_layer":true}]}"#;

    /// Builds an image for `arch` with `config` beside the store, copies its blobs in, and names it `name`.
    pub(crate) fn add(store: &Store, name: &str, arch: &str, config: &str) -> String {
        let owner = store
            .path()
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy();
        let source = scratch(&format!("{owner}-src-{}-{arch}", name.replace(['/', ':'], "-")));
        let (digest, bytes) = manifest(&source, arch, &layer(), config);
        copy(store, &source);
        let media = "application/vnd.oci.image.manifest.v1+json";
        store
            .tag(name, "linux", arch, media, &digest, bytes.len() as u64)
            .unwrap();
        digest
    }

    fn copy(store: &Store, source: &Path) {
        for file in std::fs::read_dir(source.join("blobs/sha256")).unwrap() {
            let path = file.unwrap().path();
            let digest = format!("sha256:{}", path.file_name().unwrap().to_string_lossy());
            store.put_blob(&digest, &std::fs::read(&path).unwrap()).unwrap();
        }
    }

    /// A store with `example.com/app:1` for amd64 and arm64, `example.com/other:2` naming the same amd64 image, and
    /// an image loaded without a name.
    pub(crate) fn fixture(name: &str) -> (PathBuf, Store) {
        let data = scratch(name);
        let store = Store::open(&data).unwrap();
        add(&store, "example.com/app:1", "amd64", CONFIG);
        add(&store, "example.com/app:1", "arm64", CONFIG);
        add(&store, "example.com/other:2", "amd64", CONFIG);
        let unnamed = CONFIG.replace("qa", "unnamed");
        let source = scratch(&format!("{name}-unnamed"));
        let (digest, bytes) = manifest(&source, "amd64", b"other layer", &unnamed);
        copy(&store, &source);
        let id = crate::image::digest_of(unnamed.replace("ARCH", "amd64").as_bytes());
        let media = "application/vnd.oci.image.manifest.v1+json";
        store
            .tag(&id, "linux", "amd64", media, &digest, bytes.len() as u64)
            .unwrap();
        (data, store)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{CONFIG, add, fixture};
    use super::*;
    use crate::qemu::testing::scratch;

    const NOW: i64 = 1_735_689_600 + 3 * 86_400; // three days after 2025-01-01

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    fn synthetic(name: &str, id: &str, arch: &str) -> Stored {
        Stored {
            entry: Entry {
                name: name.into(),
                os: "linux".into(),
                arch: arch.into(),
                media_type: String::new(),
                digest: format!("sha256:m{id}"),
                size: 0,
            },
            id: format!("sha256:{id}"),
            config: Value::Null,
            layers: vec![],
            size: 0,
        }
    }

    #[test]
    fn times_are_read_and_worded_as_docker_does() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2025-01-01T00:00:00.123456789Z"), Some(1_735_689_600));
        assert_eq!(parse_time("2025-01-01T02:00:00+02:00"), Some(1_735_689_600));
        assert_eq!(parse_time("2024-12-31T19:00:00-05:00"), Some(1_735_689_600));
        assert_eq!(parse_time("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(format_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_time(1_709_164_800 + 3661), "2024-02-29T01:01:01Z");
        assert_eq!(format_time(-1), "1969-12-31T23:59:59Z");
        for t in [0, 951_782_400, 1_735_689_599, 4_102_444_800, 1_763_600_000] {
            assert_eq!(parse_time(&format_time(t)), Some(t), "{t}");
        }
        for bad in [
            "",
            "2025-01-01",
            "2025-13-01T00:00:00Z",
            "2025-01-01T00:00Z",
            "x-y-zT00:00:00Z",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        let cases = [
            (0, "Less than a second ago"),
            (1, "1 second ago"),
            (30, "30 seconds ago"),
            (90, "About a minute ago"),
            (600, "10 minutes ago"),
            (3600, "About an hour ago"),
            (5 * 3600, "5 hours ago"),
            (3 * 86_400, "3 days ago"),
            (20 * 86_400, "2 weeks ago"),
            (90 * 86_400, "3 months ago"),
            (800 * 86_400, "2 years ago"),
            (-5, "Less than a second ago"),
        ];
        for (seconds, words) in cases {
            assert_eq!(ago(seconds), words);
        }
        assert_eq!(created_ago(None, NOW), "N/A");
    }

    #[test]
    fn names_ids_and_tables_are_shown_as_docker_shows_them() {
        assert_eq!(
            split_name("docker.io/library/alpine:3.20"),
            ("docker.io/library/alpine".into(), "3.20".into())
        );
        let pinned = format!("ghcr.io/org/app@sha256:{}", "a".repeat(64));
        assert_eq!(split_name(&pinned), ("ghcr.io/org/app".into(), "<none>".into()));
        assert_eq!(split_name("sha256:abc"), ("<none>".into(), "<none>".into()));
        assert_eq!(split_name("Not A Name"), ("Not A Name".into(), "<none>".into()));
        assert_eq!(short_id("sha256:0123456789abcdef", false), "0123456789ab");
        assert_eq!(short_id("sha256:0123456789abcdef", true), "sha256:0123456789abcdef");
        assert_eq!(
            table(&["A", "LONGER"], &[vec!["wide cell".into(), "x".into()]]),
            "A           LONGER\nwide cell   x\n"
        );
    }

    #[test]
    fn find_takes_names_references_and_id_prefixes() {
        let images = vec![
            synthetic("docker.io/library/app:latest", "aaaa1111", "amd64"),
            synthetic("docker.io/library/app:latest", "aaaa2222", "arm64"),
            synthetic("sha256:bbbb", "bbbb", "amd64"),
        ];
        let (found, matched) = find(&images, "app").unwrap();
        assert_eq!(
            (found.len(), matched),
            (2, Matched::Name("docker.io/library/app:latest".into()))
        );
        let (found, matched) = find(&images, "docker.io/library/app:latest").unwrap();
        assert_eq!(
            (found.len(), matched),
            (2, Matched::Name("docker.io/library/app:latest".into()))
        );
        let (found, matched) = find(&images, "AAAA2").unwrap();
        assert_eq!((found.len(), matched), (1, Matched::Id("sha256:aaaa2222".into())));
        assert_eq!(
            find(&images, "sha256:bbbb").unwrap().1,
            Matched::Name("sha256:bbbb".into())
        );
        assert_eq!(
            find(&images, "aaaa").unwrap_err(),
            LookupError::Ambiguous("aaaa".into())
        );
        assert_eq!(find(&images, "cccc").unwrap_err(), LookupError::NotFound("cccc".into()));
        assert_eq!(find(&images, "nope").unwrap_err(), LookupError::NotFound("nope".into()));
        assert!(
            LookupError::Ambiguous("aa".into())
                .to_string()
                .contains("more than one image ID")
        );
        let host = GuestArch::host().map(image::oci_arch).unwrap();
        let all: Vec<&Stored> = images.iter().take(2).collect();
        assert_eq!(preferred(&all).entry.arch, host);
    }

    #[test]
    fn images_lists_newest_first_and_filters() {
        let (_, store) = fixture("images-list");
        let newer = CONFIG.replace("2025-01-01T00:00:00Z", "2025-01-03T00:00:00Z");
        add(&store, "example.com/new:1", "amd64", &newer);
        let mut out = Vec::new();
        list(&store, &ListOptions::default(), NOW, &mut out).unwrap();
        let out = text(out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("REPOSITORY"), "{out}");
        assert!(lines[1].starts_with("example.com/new "), "{out}");
        assert!(lines[1].contains("24 hours ago"), "{out}");
        assert!(out.contains("<none>"), "{out}");
        assert!(out.contains("linux/arm64   3 days ago"), "{out}");
        assert_eq!(lines.len(), 6);

        let mut out = Vec::new();
        let options = ListOptions {
            reference: Some("example.com/app"),
            digests: true,
            no_trunc: true,
            ..ListOptions::default()
        };
        list(&store, &options, NOW, &mut out).unwrap();
        let out = text(out);
        assert!(out.lines().next().unwrap().contains("DIGEST"));
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.contains("sha256:"));

        let mut out = Vec::new();
        let by_tag = ListOptions {
            reference: Some("example.com/other:2"),
            quiet: true,
            ..ListOptions::default()
        };
        list(&store, &by_tag, NOW, &mut out).unwrap();
        assert_eq!(text(out).lines().count(), 1);

        // -q shows each ID once although two names share it.
        let mut out = Vec::new();
        let quiet = ListOptions {
            quiet: true,
            ..ListOptions::default()
        };
        list(&store, &quiet, NOW, &mut out).unwrap();
        assert_eq!(text(out).lines().count(), 4);
    }

    #[test]
    fn rmi_removes_names_then_unused_blobs() {
        let (data, store) = fixture("images-rmi");
        let images = all(&store).unwrap();
        let app_amd64 = images
            .iter()
            .find(|i| i.entry.name == "example.com/app:1" && i.entry.arch == "amd64")
            .unwrap()
            .id
            .clone();
        let blobs = || std::fs::read_dir(data.join("images/blobs/sha256")).unwrap().count();
        let before = blobs();

        // By ID, an image with two names needs -f.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ok = remove(&store, &[app_amd64[7..15].to_string()], false, &mut out, &mut err).unwrap();
        assert!(!ok);
        assert!(text(err).contains("has 2 names"));
        assert_eq!(blobs(), before);

        // By name: every platform goes, but the amd64 blobs stay for example.com/other:2.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let wanted = ["example.com/app:1".to_string(), "nope".to_string()];
        let ok = remove(&store, &wanted, false, &mut out, &mut err).unwrap();
        assert!(!ok, "nope does not exist");
        let out = text(out);
        assert!(out.contains("Untagged: example.com/app:1"), "{out}");
        assert_eq!(
            out.matches("Deleted:").count(),
            1,
            "only the arm64 image is gone: {out}"
        );
        assert!(text(err).contains("no such image: nope"));
        assert!(blobs() < before);
        assert!(exists(&store, "example.com/other:2").unwrap());
        assert!(!exists(&store, "example.com/app:1").unwrap());

        // By ID with -f: the unnamed image goes and nothing is printed as untagged.
        let unnamed = all(&store).unwrap().into_iter().find(Stored::is_unnamed).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(remove(&store, &[unnamed.id[7..19].to_string()], true, &mut out, &mut err).unwrap());
        let out = text(out);
        assert!(!out.contains("Untagged"), "{out}");
        assert!(out.contains(&format!("Deleted: {}", unnamed.id)), "{out}");
    }

    #[test]
    fn prune_removes_unnamed_images_or_all() {
        let (_, store) = fixture("images-prune");
        let mut out = Vec::new();
        prune(&store, false, &mut out).unwrap();
        let out = text(out);
        assert!(out.starts_with("Deleted Images:\ndeleted: sha256:"), "{out}");
        assert!(!out.contains("untagged"), "{out}");
        assert_eq!(all(&store).unwrap().len(), 3);

        let mut out = Vec::new();
        prune(&store, false, &mut out).unwrap();
        assert_eq!(text(out), "Total reclaimed space: 0 B\n");

        let mut out = Vec::new();
        prune(&store, true, &mut out).unwrap();
        let out = text(out);
        assert!(
            out.contains("untagged: example.com/app:1\nuntagged: example.com/other:2\n"),
            "{out}"
        );
        assert!(all(&store).unwrap().is_empty());
    }

    #[test]
    fn tag_names_every_platform() {
        let (_, store) = fixture("images-tag");
        tag(&store, "example.com/app:1", "mine/app").unwrap();
        let tagged: Vec<String> = all(&store)
            .unwrap()
            .into_iter()
            .filter(|i| i.entry.name == "docker.io/mine/app:latest")
            .map(|i| i.entry.arch)
            .collect();
        assert_eq!(tagged, ["amd64", "arm64"]);
        let pinned = format!("mine/app@sha256:{}", "a".repeat(64));
        assert!(
            tag(&store, "example.com/app:1", &pinned)
                .unwrap_err()
                .to_string()
                .contains("digest")
        );
        assert!(tag(&store, "example.com/app:1", "Bad Name").is_err());
        assert!(tag(&store, "nope", "mine/app").is_err());
    }

    #[test]
    fn inspect_shows_docker_fields_in_order() {
        let (_, store) = fixture("images-inspect");
        let wanted = ["example.com/other:2".to_string(), "nope".to_string()];
        let (objects, missing) = inspect(&store, &wanted).unwrap();
        assert_eq!(missing, ["nope"]);
        let text = serde_json::to_string(&objects).unwrap();
        assert!(text.starts_with(r#"[{"Id":"sha256:"#), "{text}");
        let o = &objects[0];
        assert_eq!(o.repo_tags, ["example.com/app:1", "example.com/other:2"]);
        assert_eq!(o.repo_digests.len(), 2);
        assert_eq!(o.labels["team"], "qa");
        assert_eq!(o.architecture, "amd64");
        assert!(text.contains(r#""RootFS":"#));
    }

    #[test]
    fn history_shows_each_step_newest_first() {
        let (_, store) = fixture("images-history");
        let mut out = Vec::new();
        history(&store, "example.com/other:2", false, false, NOW, &mut out).unwrap();
        let out = text(out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("IMAGE"), "{out}");
        assert!(lines[1].contains(r#"CMD ["sh"]"#) && lines[1].contains("0 B"), "{out}");
        assert!(lines[2].starts_with("<missing>") && lines[2].contains('…'), "{out}");
        assert!(lines[2].contains("buildkit"), "{out}");

        let mut out = Vec::new();
        history(&store, "example.com/other:2", true, true, NOW, &mut out).unwrap();
        let out = text(out);
        assert!(out.lines().next().unwrap().starts_with("sha256:"), "{out}");
        assert_eq!(out.lines().nth(1), Some("<missing>"));

        // An image with no history gets a row per layer.
        add(
            &store,
            "example.com/bare:1",
            "amd64",
            r#"{"architecture":"ARCH","os":"linux"}"#,
        );
        let mut out = Vec::new();
        history(&store, "example.com/bare:1", false, false, NOW, &mut out).unwrap();
        assert_eq!(text(out).lines().count(), 2);
        assert!(history(&store, "nope", false, false, NOW, &mut Vec::new()).is_err());
    }

    #[test]
    fn disk_usage_counts_images_and_bytes() {
        let mut out = Vec::new();
        disk_usage(None, &mut out).unwrap();
        let out = text(out);
        assert!(out.contains("Images       0       0        0 B    0 B"), "{out}");

        let (_, store) = fixture("images-df");
        let mut out = Vec::new();
        disk_usage(Some(&store), &mut out).unwrap();
        let out = text(out);
        assert!(out.contains("Images       3"), "{out}");
        assert!(out.contains("(100%)"), "{out}");
    }

    #[test]
    fn save_writes_an_archive_that_load_reads_back() {
        let (data, store) = fixture("images-save");
        let output = data.join("saved.tar");
        let wanted = ["example.com/app:1".to_string(), "example.com/app:1".to_string()];
        save(&store, &wanted, None, &output).unwrap();

        let mut names = Vec::new();
        let mut manifest_json = String::new();
        let mut archive = tar::Archive::new(std::fs::File::open(&output).unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            if path == "manifest.json" {
                entry.read_to_string(&mut manifest_json).unwrap();
            }
            names.push(path);
        }
        assert_eq!(names[..3], ["oci-layout", "index.json", "manifest.json"]);
        assert!(
            manifest_json.contains(r#""RepoTags":["example.com/app:1"]"#),
            "{manifest_json}"
        );

        let other = Store::open(&scratch("images-save-other")).unwrap();
        let loaded = crate::load::load(&output, &other, &mut Vec::new()).unwrap();
        let mut platforms: Vec<String> = loaded.iter().map(|l| l.platform.clone()).collect();
        platforms.sort();
        assert_eq!(platforms, ["linux/amd64", "linux/arm64"]);
        let before: Vec<String> = all(&store).unwrap().iter().map(|i| i.entry.digest.clone()).collect();
        for i in all(&other).unwrap() {
            assert!(before.contains(&i.entry.digest), "digests are unchanged");
        }

        // Only one platform, and an unnamed image.
        let unnamed = all(&store).unwrap().into_iter().find(Stored::is_unnamed).unwrap();
        let one = data.join("one.tar");
        save(&store, std::slice::from_ref(&unnamed.id), Some("linux/amd64"), &one).unwrap();
        let err = save(&store, std::slice::from_ref(&unnamed.id), Some("linux/arm64"), &one).unwrap_err();
        assert!(err.to_string().contains("no linux/arm64 variant"), "{err}");
        assert!(save(&store, &["nope".to_string()], None, &one).is_err());
    }

    #[test]
    fn save_refuses_a_tampered_blob_and_leaves_nothing() {
        let (data, store) = fixture("images-save-bad");
        let other = all(&store)
            .unwrap()
            .into_iter()
            .find(|i| i.entry.name == "example.com/other:2")
            .unwrap();
        let (layer, _) = &other.layers[0];
        let path = data.join("images/blobs/sha256").join(&layer[7..]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        let output = data.join("bad.tar");
        let err = save(&store, &["example.com/other:2".to_string()], None, &output).unwrap_err();
        assert!(err.to_string().contains(layer), "{err}");
        assert!(!output.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
