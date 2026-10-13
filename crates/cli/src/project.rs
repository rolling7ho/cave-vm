//! Finding the project: `cave.toml` in this folder or a parent, up to the
//! home folder, the way git finds a repository (D16).

use std::path::{Path, PathBuf};

use crate::error::{CliError, Result};

pub const CONFIG_FILE: &str = "cave.toml";
pub const STATE_DIR: &str = ".cave";

/// The project folder containing `start`, if any. The search stops at
/// `home`, which is never a project itself.
pub fn find(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
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
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The current folder, as an absolute path.
pub fn current_dir() -> Result<PathBuf> {
    std::env::current_dir()
        .map_err(|e| CliError::new(format!("cannot read the current folder: {e}")))
}

/// The error for commands that need a project outside one.
pub fn not_a_project() -> CliError {
    CliError::new("not a cave project (no cave.toml here or in any parent folder)")
        .fix("Run `cave init` to set up this folder")
}

/// The project for a command that needs one.
pub fn require() -> Result<PathBuf> {
    let cwd = current_dir()?;
    find(&cwd, home().as_deref()).ok_or_else(not_a_project)
}

/// A path as the daemon API wants it.
pub fn wire_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_the_nearest_project_and_stops_at_home() {
        let root = std::env::temp_dir().join(format!("cave-find-{}", std::process::id()));
        let home = root.join("home");
        let project = home.join("code/app");
        let deep = project.join("src/lib");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find(&deep, Some(&home)), None);
        fs::write(project.join(CONFIG_FILE), "").unwrap();
        assert_eq!(find(&deep, Some(&home)), Some(project.clone()));
        assert_eq!(find(&project, Some(&home)), Some(project.clone()));
        // A cave.toml in the home folder itself does not count.
        fs::remove_file(project.join(CONFIG_FILE)).unwrap();
        fs::write(home.join(CONFIG_FILE), "").unwrap();
        assert_eq!(find(&deep, Some(&home)), None);
        let _ = fs::remove_dir_all(&root);
    }
}
