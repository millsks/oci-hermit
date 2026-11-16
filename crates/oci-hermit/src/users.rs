//! Resolving an image's `User` (or `--user`) to the IDs a command runs as, the way Docker and runc do: from the
//! image's own `/etc/passwd` and `/etc/group`, never the host's.

use crate::rootfs::{Node, Rootfs};

/// Who a command runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups.
    pub groups: Vec<u32>,
    /// The home directory from `/etc/passwd`, or `/` when the user has no entry.
    pub home: String,
}

impl Identity {
    pub fn is_root(&self) -> bool {
        self.uid == 0 && self.gid == 0 && self.groups.is_empty()
    }
}

struct Passwd<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

struct Group<'a> {
    name: &'a str,
    gid: u32,
    members: Vec<&'a str>,
}

/// The fields of each well-formed line; comments, blank and malformed lines are skipped, as libc does.
fn entries(text: &str, fields: usize) -> impl Iterator<Item = Vec<&str>> {
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.split(':').collect::<Vec<_>>())
        .filter(move |parts| parts.len() >= fields)
}

fn passwd(text: &str) -> Vec<Passwd<'_>> {
    entries(text, 7)
        .filter_map(|p| {
            Some(Passwd {
                name: p[0],
                uid: p[2].parse().ok()?,
                gid: p[3].parse().ok()?,
                home: p[5],
            })
        })
        .collect()
}

fn groups(text: &str) -> Vec<Group<'_>> {
    entries(text, 4)
        .filter_map(|p| {
            Some(Group {
                name: p[0],
                gid: p[2].parse().ok()?,
                members: p[3].split(',').filter(|m| !m.is_empty()).collect(),
            })
        })
        .collect()
}

/// A file's contents, following symbolic links inside the image (never above its root); empty when it does not
/// exist.
fn read(rootfs: &Rootfs, path: &str) -> String {
    let mut pending: Vec<String> = path.split('/').rev().map(str::to_string).collect();
    let mut resolved: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(part) = pending.pop() {
        match part.as_str() {
            "" | "." => continue,
            ".." => {
                resolved.pop();
                continue;
            }
            _ => resolved.push(part),
        }
        if let Some(Node::Symlink(_, target)) = rootfs.get(&resolved.join("/")) {
            links += 1;
            if links > 16 {
                return String::new();
            }
            resolved.pop();
            if target.starts_with('/') {
                resolved.clear();
            }
            pending.extend(target.split('/').rev().map(str::to_string));
        }
    }
    match rootfs.get(&resolved.join("/")) {
        Some(Node::File(_, data)) => String::from_utf8_lossy(data).into_owned(),
        _ => String::new(),
    }
}

/// Resolve `spec` (`user`, `uid`, `user:group` or `uid:gid`; empty means root) against the image.
pub fn resolve(spec: &str, rootfs: &Rootfs) -> Result<Identity, String> {
    let passwd_text = read(rootfs, "etc/passwd");
    let group_text = read(rootfs, "etc/group");
    let (users, all_groups) = (passwd(&passwd_text), groups(&group_text));
    let (user_part, group_part) = match spec.split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };
    let user_part = if user_part.is_empty() { "0" } else { user_part };

    let entry = match user_part.parse::<u32>() {
        Ok(uid) => users.iter().find(|u| u.uid == uid),
        Err(_) => Some(
            users
                .iter()
                .find(|u| u.name == user_part)
                .ok_or_else(|| format!("user {user_part:?} is not in the image's /etc/passwd"))?,
        ),
    };
    let uid = match entry {
        Some(e) => e.uid,
        None => user_part.parse().unwrap_or(0),
    };
    let mut identity = Identity {
        uid,
        gid: entry.map_or(0, |e| e.gid),
        groups: Vec::new(),
        home: entry.map_or("/", |e| e.home).to_string(),
    };
    if identity.home.is_empty() {
        identity.home = "/".into();
    }
    match group_part {
        Some(group) if !group.is_empty() => {
            identity.gid = match group.parse::<u32>() {
                Ok(gid) => gid,
                Err(_) => {
                    all_groups
                        .iter()
                        .find(|g| g.name == group)
                        .ok_or_else(|| format!("group {group:?} is not in the image's /etc/group"))?
                        .gid
                }
            };
        }
        // Without an explicit group, a named user also gets the groups that list it as a member.
        _ => {
            if let Some(e) = entry {
                for g in all_groups.iter().filter(|g| g.members.contains(&e.name)) {
                    if g.gid != identity.gid && !identity.groups.contains(&g.gid) {
                        identity.groups.push(g.gid);
                    }
                }
            }
        }
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rootfs::testing::Layer;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
# a comment
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
app:x:1000:1000:App:/home/app:/bin/sh
broken:x:notanumber:1::/:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
nohome:x:1001:1001:::/bin/sh
";
    const GROUP: &str = "\
root:x:0:
wheel:x:10:app,daemon
app:x:1000:
video:x:44:app
nogroup:x:65534:
";

    fn image() -> Rootfs {
        // /etc/passwd is a link, which some images use; /etc/group is reached through a linked /etc.
        let layer = Layer::new()
            .dir("data")
            .file("data/passwd.real", PASSWD.as_bytes())
            .file("data/group", GROUP.as_bytes())
            .link("etc", tar::EntryType::Symlink, "/data")
            .link("data/passwd", tar::EntryType::Symlink, "passwd.real")
            .bytes();
        let mut rootfs = Rootfs::new();
        rootfs.apply_layer(&layer[..]).unwrap();
        rootfs
    }

    fn ids(spec: &str) -> Result<(u32, u32, Vec<u32>, String), String> {
        resolve(spec, &image()).map(|i| (i.uid, i.gid, i.groups, i.home))
    }

    #[test]
    fn resolves_names_and_numbers_like_docker() {
        assert_eq!(ids("").unwrap(), (0, 0, vec![], "/root".into()));
        assert_eq!(ids("app").unwrap(), (1000, 1000, vec![10, 44], "/home/app".into()));
        assert_eq!(ids("1000").unwrap(), (1000, 1000, vec![10, 44], "/home/app".into()));
        assert_eq!(ids("app:video").unwrap(), (1000, 44, vec![], "/home/app".into()));
        assert_eq!(ids("app:7").unwrap(), (1000, 7, vec![], "/home/app".into()));
        assert_eq!(ids(":44").unwrap(), (0, 44, vec![], "/root".into()));
        assert_eq!(ids("nobody").unwrap(), (65534, 65534, vec![], "/nonexistent".into()));
        assert_eq!(ids("nohome").unwrap().3, "/");
        // A uid with no entry runs with gid 0 and home /.
        assert_eq!(ids("4242").unwrap(), (4242, 0, vec![], "/".into()));
        assert_eq!(ids("4242:4242").unwrap(), (4242, 4242, vec![], "/".into()));
    }

    #[test]
    fn names_missing_from_the_image_are_errors() {
        assert_eq!(
            ids("ghost").unwrap_err(),
            r#"user "ghost" is not in the image's /etc/passwd"#
        );
        assert_eq!(
            ids("broken").unwrap_err(),
            r#"user "broken" is not in the image's /etc/passwd"#
        );
        assert_eq!(
            ids("app:ghosts").unwrap_err(),
            r#"group "ghosts" is not in the image's /etc/group"#
        );
    }

    #[test]
    fn an_image_without_passwd_still_runs_numeric_users() {
        let empty = Rootfs::new();
        assert!(resolve("", &empty).unwrap().is_root());
        assert_eq!(resolve("", &empty).unwrap().home, "/");
        assert_eq!(resolve("1000:1000", &empty).unwrap().uid, 1000);
        assert!(resolve("app", &empty).is_err());
        assert!(!resolve("1000", &empty).unwrap().is_root());
    }
}
