//! Finding the project: `cave.toml` in a folder or a parent, up to the
//! home folder, the way `cave` finds it (D16).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const CONFIG_FILE: &str = "cave.toml";

/// The project folder containing `start`, if any. The search stops at
/// `home`, which is never a project itself.
pub(crate) fn find(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if Some(d) == home {
            return None;
        }
        if d.join(CONFIG_FILE).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// The user's home folder.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The project `path` is in, as an absolute path.
pub(crate) fn require(path: &Path) -> Result<PathBuf> {
    let dir = path.canonicalize().map_err(|e| {
        Error::new(format!("cannot open {}: {e}", path.display()))
            .fix("Pass the path of a folder that exists")
    })?;
    let home = home().and_then(|h| h.canonicalize().ok());
    find(&dir, home.as_deref()).ok_or_else(|| {
        Error::new(format!(
            "not a cave project (no cave.toml in {} or any parent folder)",
            dir.display()
        ))
        .fix(format!("Run `cave init` in {}", dir.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_the_nearest_project_and_stops_at_home() {
        let root = std::env::temp_dir().join(format!("cave-sdk-find-{}", std::process::id()));
        let home = root.join("home");
        let project = home.join("code/app");
        let deep = project.join("src/lib");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find(&deep, Some(&home)), None);
        fs::write(project.join(CONFIG_FILE), "").unwrap();
        assert_eq!(find(&deep, Some(&home)), Some(project.clone()));
        fs::remove_file(project.join(CONFIG_FILE)).unwrap();
        fs::write(home.join(CONFIG_FILE), "").unwrap();
        assert_eq!(find(&deep, Some(&home)), None);
        let _ = fs::remove_dir_all(&root);
    }
}
