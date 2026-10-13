//! The user commands run as: `cave`, with passwordless sudo and the
//! `docker` group (`docs/cli.md`, "Guest layout"), not root.

use std::path::PathBuf;
use std::process::Command;

use crate::sys;

/// The guest user's name.
pub const GUEST_USER: &str = "cave";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: String,
    /// Supplementary groups, such as `docker`.
    pub groups: Vec<u32>,
}

impl User {
    /// Finds `name` in the text of `/etc/passwd` and `/etc/group`.
    pub fn lookup(name: &str, passwd: &str, group: &str) -> Option<User> {
        let fields: Vec<&str> = passwd
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
            .find(|f| f.len() == 7 && f[0] == name)?;
        let uid = fields[2].parse().ok()?;
        let gid = fields[3].parse().ok()?;
        let mut groups: Vec<u32> = group
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                (f.len() == 4 && f[3].split(',').any(|member| member == name))
                    .then(|| f[2].parse().ok())
                    .flatten()
            })
            .collect();
        if !groups.contains(&gid) {
            groups.insert(0, gid);
        }
        Some(User {
            name: name.to_string(),
            uid,
            gid,
            home: PathBuf::from(fields[5]),
            shell: if fields[6].is_empty() {
                "/bin/sh".into()
            } else {
                fields[6].to_string()
            },
            groups,
        })
    }

    /// Finds `name` in this system's `/etc/passwd` and `/etc/group`.
    pub fn from_system(name: &str) -> Option<User> {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
        User::lookup(name, &passwd, &group)
    }

    /// Makes `command` run as this user, with its groups and the
    /// environment a login would give (`HOME`, `USER`, `LOGNAME`,
    /// `SHELL`). Variables the request sets afterwards still win.
    pub fn apply(&self, command: &mut Command) {
        command
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .env("SHELL", &self.shell);
        sys::run_as(command, self.uid, self.gid, self.groups.clone());
    }
}

/// The user for commands when the agent runs as root: [`GUEST_USER`], if
/// the image has it. Otherwise commands run as the agent itself.
pub fn guest_user() -> Option<User> {
    if sys::uid() != 0 {
        return None;
    }
    let user = User::from_system(GUEST_USER);
    if user.is_none() {
        eprintln!("cave-agent: no user {GUEST_USER} in /etc/passwd; commands run as root");
    }
    user
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          cave:x:1000:1000:cave:/home/cave:/bin/bash\n\
                          nologin:x:1001:1001::/home/n:\n";
    const GROUP: &str = "root:x:0:\n\
                         sudo:x:27:cave\n\
                         docker:x:999:other,cave\n\
                         cave:x:1000:\n\
                         staff:x:50:caveman\n";

    #[test]
    fn finds_the_user_and_its_groups() {
        assert_eq!(
            User::lookup("cave", PASSWD, GROUP),
            Some(User {
                name: "cave".into(),
                uid: 1000,
                gid: 1000,
                home: "/home/cave".into(),
                shell: "/bin/bash".into(),
                groups: vec![1000, 27, 999],
            })
        );
    }

    #[test]
    fn an_empty_shell_is_sh_and_a_missing_user_is_none() {
        let user = User::lookup("nologin", PASSWD, GROUP).unwrap();
        assert_eq!(user.shell, "/bin/sh");
        assert_eq!(user.groups, [1001]);
        assert_eq!(User::lookup("nobody", PASSWD, GROUP), None);
        assert_eq!(User::lookup("cave", "cave:x:abc:1:::\n", ""), None);
    }
}
