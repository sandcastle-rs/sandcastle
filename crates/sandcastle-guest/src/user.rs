//! Resolves a Dockerfile `USER` value against the image's `/etc/passwd`
//! and `/etc/group`, following runc's `user.GetExecUser`.

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups for `setgroups`.
    pub groups: Vec<u32>,
    /// Used for `HOME` when the image env does not set it.
    pub home: String,
}

struct PasswdEntry<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

struct GroupEntry<'a> {
    name: &'a str,
    gid: u32,
    members: Vec<&'a str>,
}

pub fn resolve(spec: &str, passwd: &str, group: &str) -> Result<Ids> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let users: Vec<PasswdEntry> = passwd.lines().filter_map(parse_passwd).collect();
    let groups: Vec<GroupEntry> = group.lines().filter_map(parse_group).collect();

    let numeric_user = user_part.parse::<u32>().ok();
    let found = if user_part.is_empty() {
        users.iter().find(|u| u.uid == 0)
    } else {
        users
            .iter()
            .find(|u| u.name == user_part || Some(u.uid) == numeric_user)
    };
    let (uid, mut gid, home, name) = match (found, numeric_user) {
        (Some(u), _) => (u.uid, u.gid, u.home.to_string(), Some(u.name)),
        (None, Some(uid)) => (uid, 0, "/".to_string(), None),
        (None, None) if user_part.is_empty() => (0, 0, "/".to_string(), None),
        (None, None) => {
            bail!("unable to find user {user_part}: no matching entries in passwd file")
        }
    };

    let supplementary = match group_part {
        Some(g) => {
            let numeric = g.parse::<u32>().ok();
            gid = match groups
                .iter()
                .find(|e| e.name == g || Some(e.gid) == numeric)
            {
                Some(e) => e.gid,
                None => match numeric {
                    Some(n) => n,
                    None => bail!("unable to find group {g}: no matching entries in group file"),
                },
            };
            Vec::new()
        }
        None => {
            let mut ids: Vec<u32> = match name {
                Some(name) => groups
                    .iter()
                    .filter(|e| e.members.contains(&name))
                    .map(|e| e.gid)
                    .collect(),
                None => Vec::new(),
            };
            ids.dedup();
            ids
        }
    };
    Ok(Ids {
        uid,
        gid,
        groups: supplementary,
        home,
    })
}

fn parse_passwd(line: &str) -> Option<PasswdEntry<'_>> {
    if line.starts_with('#') {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 7 {
        return None;
    }
    Some(PasswdEntry {
        name: f[0],
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        home: f[5],
    })
}

fn parse_group(line: &str) -> Option<GroupEntry<'_>> {
    if line.starts_with('#') {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 4 {
        return None;
    }
    Some(GroupEntry {
        name: f[0],
        gid: f[2].parse().ok()?,
        members: f[3].split(',').filter(|m| !m.is_empty()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
# comment
broken line
app:x:1500:1500::/home/app:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
";
    const GROUP: &str = "\
root:x:0:
app:x:1500:
grp:x:2000:app,other
wheel:x:10:root
";

    #[test]
    fn empty_spec_is_root_with_its_home() {
        let ids = resolve("", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.home.as_str()), (0, 0, "/root"));
        assert_eq!(ids.groups, [10]);
    }

    #[test]
    fn name_gets_primary_and_member_groups() {
        let ids = resolve("app", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid), (1500, 1500));
        assert_eq!(ids.groups, [2000]);
        assert_eq!(ids.home, "/home/app");
    }

    #[test]
    fn numeric_uid_in_passwd_uses_its_entry() {
        let ids = resolve("65534", PASSWD, GROUP).unwrap();
        assert_eq!(
            (ids.uid, ids.gid, ids.home.as_str()),
            (65534, 65534, "/nonexistent")
        );
    }

    #[test]
    fn unknown_numeric_ids_are_used_as_is() {
        let ids = resolve("4321:4322", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.home.as_str()), (4321, 4322, "/"));
        assert!(ids.groups.is_empty());
        let ids = resolve("4321", "", "").unwrap();
        assert_eq!((ids.uid, ids.gid), (4321, 0));
    }

    #[test]
    fn explicit_group_by_name_replaces_supplementary_groups() {
        let ids = resolve("app:grp", PASSWD, GROUP).unwrap();
        assert_eq!((ids.uid, ids.gid), (1500, 2000));
        assert!(ids.groups.is_empty());
    }

    #[test]
    fn unknown_names_are_errors() {
        let err = resolve("ghost", PASSWD, GROUP).unwrap_err().to_string();
        assert_eq!(
            err,
            "unable to find user ghost: no matching entries in passwd file"
        );
        let err = resolve("app:ghosts", PASSWD, GROUP)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "unable to find group ghosts: no matching entries in group file"
        );
    }
}
