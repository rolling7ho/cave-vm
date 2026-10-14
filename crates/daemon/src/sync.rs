//! Synced mode's rules (design, "Storage"; docs/cli.md, "cave sync" and
//! "cave pull"; P5.10): which files `cave sync` writes to the Mac mirror and
//! which `cave pull` brings into the VM, decided from the SHA-256 of each
//! file on the Mac, in the VM and at the last sync.
//!
//! A file changed on one side is the change that side made since the last
//! sync; a file missing on a side that had it then was deleted there.
//!
//! - `pull`: a file changed only on the Mac is applied to the VM, one
//!   changed only in the VM is left alone, and one changed on both sides
//!   to different contents stops the pull before anything is applied.
//! - `sync`: the VM is authoritative, so a file changed only in the VM is
//!   written to the mirror. A file changed on the Mac is never written
//!   over: it waits for `cave pull`, and one changed on both sides is
//!   listed.
//!
//! Dependency and build folders are excluded: `node_modules`, `target` and
//! the paths `.gitignore` names, plus `.git`, `.cave` and `.env`, which
//! never sync.

use std::collections::{BTreeMap, BTreeSet};

/// Each file's SHA-256 as lowercase hex, by path relative to the project.
pub type Hashes = BTreeMap<String, String>;

/// Folders and files that never sync, wherever they are.
const ALWAYS_EXCLUDED: [&str; 4] = ["node_modules", "target", ".git", ".cave"];

/// Files that never sync at the project's top level. `.env` holds the
/// credentials and stays on the Mac (D33).
const EXCLUDED_AT_TOP: [&str; 1] = [".env"];

/// What a pull does, if it goes ahead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pull {
    /// Files to copy from the Mac into the VM.
    pub write: Vec<String>,
    /// Files deleted on the Mac, to delete in the VM.
    pub delete: Vec<String>,
}

/// What a sync does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sync {
    /// Files to copy from the VM to the Mac.
    pub write: Vec<String>,
    /// Files deleted in the VM, to delete on the Mac.
    pub delete: Vec<String>,
    /// Files changed on both sides, left alone.
    pub conflicts: Vec<String>,
}

fn changed(path: &str, side: &Hashes, last: &Hashes) -> bool {
    side.get(path) != last.get(path)
}

fn paths<'a>(last: &'a Hashes, mac: &'a Hashes, vm: &'a Hashes) -> BTreeSet<&'a String> {
    last.keys().chain(mac.keys()).chain(vm.keys()).collect()
}

/// The pull, or the files changed on both sides, which stop it.
pub fn plan_pull(last: &Hashes, mac: &Hashes, vm: &Hashes) -> Result<Pull, Vec<String>> {
    let mut pull = Pull::default();
    let mut conflicts = Vec::new();
    for path in paths(last, mac, vm) {
        let on_mac = changed(path, mac, last);
        let in_vm = changed(path, vm, last);
        match (on_mac, in_vm) {
            (true, false) => match mac.get(path) {
                Some(_) => pull.write.push(path.clone()),
                None => pull.delete.push(path.clone()),
            },
            (true, true) if mac.get(path) != vm.get(path) => conflicts.push(path.clone()),
            _ => {}
        }
    }
    if conflicts.is_empty() {
        Ok(pull)
    } else {
        Err(conflicts)
    }
}

/// The sync.
pub fn plan_sync(last: &Hashes, mac: &Hashes, vm: &Hashes) -> Sync {
    let mut sync = Sync::default();
    for path in paths(last, mac, vm) {
        let on_mac = changed(path, mac, last);
        let in_vm = changed(path, vm, last);
        match (on_mac, in_vm) {
            (false, true) => match vm.get(path) {
                Some(_) => sync.write.push(path.clone()),
                None => sync.delete.push(path.clone()),
            },
            (true, true) if mac.get(path) != vm.get(path) => sync.conflicts.push(path.clone()),
            _ => {}
        }
    }
    sync
}

/// The hashes to keep as the last sync once both sides hold `mac` and
/// `vm`: each file whose two sides agree, at that content. A file whose
/// sides differ keeps its old entry, so its pending change is still seen.
pub fn agreed(last: &Hashes, mac: &Hashes, vm: &Hashes) -> Hashes {
    let mut next = Hashes::new();
    for path in paths(last, mac, vm) {
        match (mac.get(path), vm.get(path)) {
            (Some(m), Some(v)) if m == v => {
                next.insert(path.clone(), m.clone());
            }
            (None, None) => {}
            _ => {
                if let Some(old) = last.get(path) {
                    next.insert(path.clone(), old.clone());
                }
            }
        }
    }
    next
}

/// The exclusions for a project: the fixed ones and its root `.gitignore`.
#[derive(Debug, Clone, Default)]
pub struct Exclusions {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    /// Segments of the pattern, split on `/`.
    pattern: Vec<String>,
    /// Matches from the project's top only (the pattern had a `/` before
    /// its end).
    anchored: bool,
    /// Matches folders only (the pattern ended in `/`).
    dir_only: bool,
    /// Re-includes what an earlier rule excluded (`!`).
    negated: bool,
}

impl Exclusions {
    /// The exclusions with the rules of `gitignore`, the text of the
    /// project's root `.gitignore`. Nested `.gitignore` files are not read.
    pub fn new(gitignore: &str) -> Exclusions {
        let rules = gitignore
            .lines()
            .filter_map(|line| {
                let line = line.trim_end();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let (negated, line) = match line.strip_prefix('!') {
                    Some(rest) => (true, rest),
                    None => (false, line.strip_prefix('\\').unwrap_or(line)),
                };
                let dir_only = line.ends_with('/');
                let line = line.trim_end_matches('/');
                let anchored = line.contains('/');
                let line = line.trim_start_matches('/');
                if line.is_empty() {
                    return None;
                }
                Some(Rule {
                    pattern: line.split('/').map(String::from).collect(),
                    anchored,
                    dir_only,
                    negated,
                })
            })
            .collect();
        Exclusions { rules }
    }

    /// Whether `path`, relative to the project with `/` between folders,
    /// is excluded. `is_dir` says whether it is a folder. A file in an
    /// excluded folder is excluded too.
    pub fn excluded(&self, path: &str, is_dir: bool) -> bool {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments
            .first()
            .is_some_and(|s| EXCLUDED_AT_TOP.contains(s))
            && segments.len() == 1
            && !is_dir
        {
            return true;
        }
        // Each leading folder, then the path itself.
        (1..=segments.len()).any(|n| {
            let dir = n < segments.len() || is_dir;
            self.excludes(&segments[..n], dir)
        })
    }

    fn excludes(&self, segments: &[&str], is_dir: bool) -> bool {
        if segments.last().is_some_and(|s| ALWAYS_EXCLUDED.contains(s)) {
            return true;
        }
        let mut excluded = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.matches(segments) {
                excluded = !rule.negated;
            }
        }
        excluded
    }
}

impl Rule {
    fn matches(&self, segments: &[&str]) -> bool {
        let pattern: Vec<&str> = self.pattern.iter().map(String::as_str).collect();
        if self.anchored {
            match_segments(&pattern, segments)
        } else {
            // A pattern without a slash matches the name at any depth.
            segments.last().is_some_and(|name| glob(pattern[0], name))
        }
    }
}

/// Matches path segments against pattern segments, where `**` matches any
/// number of segments.
fn match_segments(pattern: &[&str], segments: &[&str]) -> bool {
    match pattern.split_first() {
        None => segments.is_empty(),
        Some((&"**", rest)) => (0..=segments.len()).any(|n| match_segments(rest, &segments[n..])),
        Some((first, rest)) => segments
            .split_first()
            .is_some_and(|(segment, more)| glob(first, segment) && match_segments(rest, more)),
    }
}

/// Matches one name against a pattern with `*`, `?` and `[...]` classes.
fn glob(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_at(&p, &n)
}

fn glob_at(p: &[char], n: &[char]) -> bool {
    match p.split_first() {
        None => n.is_empty(),
        Some(('*', rest)) => (0..=n.len()).any(|i| glob_at(rest, &n[i..])),
        Some(('?', rest)) => !n.is_empty() && glob_at(rest, &n[1..]),
        Some(('[', rest)) => {
            let Some(end) = rest.iter().position(|&c| c == ']') else {
                return n.first() == Some(&'[') && glob_at(rest, &n[1..]);
            };
            let Some(&c) = n.first() else { return false };
            let (negate, class) = match rest[..end].split_first() {
                Some(('!' | '^', class)) => (true, class),
                _ => (false, &rest[..end]),
            };
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == '-' {
                    hit |= class[i] <= c && c <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == c;
                    i += 1;
                }
            }
            hit != negate && glob_at(&rest[end + 1..], &n[1..])
        }
        Some(('\\', rest)) if !rest.is_empty() => {
            n.first() == Some(&rest[0]) && glob_at(&rest[1..], &n[1..])
        }
        Some((c, rest)) => n.first() == Some(c) && glob_at(rest, &n[1..]),
    }
}

#[cfg(test)]
mod tests;
