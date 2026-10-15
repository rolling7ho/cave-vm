//! `cave export`: copies the project directory out of the VM, or out of a
//! snapshot, into a folder on the Mac (docs/cli.md, "cave export", D19,
//! D38).

use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use cave_vm_proto::daemon::{Reply, Request, VmId};

use crate::client::Connection;
use crate::commands::{ensure_running, folder_name, vm_id};
use crate::copy;
use crate::error::{CliError, Result};
use crate::project;
use crate::snapshot::credential_warning;

/// The project directory inside the VM (docs/cli.md, "Guest layout").
const WORKSPACE: &str = "/workspace";

/// A fork made to read a snapshot, removed when dropped.
struct TempFork {
    project: VmId,
    name: String,
}

impl Drop for TempFork {
    fn drop(&mut self) {
        let _ = Connection::call(&Request::Remove {
            vm: self.project.clone(),
            name: Some(self.name.clone()),
        });
    }
}

/// Today's date on this Mac, as `YYYY-MM-DD`.
fn today() -> Result<String> {
    let out = Command::new("/bin/date").arg("+%Y-%m-%d").output()?;
    let date = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || date.is_empty() {
        return Err(CliError::new("cannot read today's date"));
    }
    Ok(date)
}

/// The files under `dir` and the bytes they hold.
fn count(dir: &Path) -> (u64, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|e| Some((e.path(), e.file_type().ok()?)))
        .map(|(path, kind)| {
            if kind.is_dir() {
                count(&path)
            } else if kind.is_file() {
                (1, fs::metadata(&path).map_or(0, |m| m.len()))
            } else {
                (1, 0)
            }
        })
        .fold((0, 0), |(f, b), (df, db)| (f + df, b + db))
}

/// `1204` as `1,204`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn size(bytes: u64) -> String {
    const MB: u64 = 1 << 20;
    if bytes < MB {
        format!("{} KB", bytes.div_ceil(1024))
    } else if bytes < 1024 * MB {
        format!("{} MB", (bytes + MB / 2) / MB)
    } else {
        format!("{:.1} GB", bytes as f64 / (1024 * MB) as f64)
    }
}

fn exported(files: u64, bytes: u64, dir: &Path) -> String {
    let files = match files {
        1 => "1 file".to_string(),
        n => format!("{} files", thousands(n)),
    };
    format!("Exported {files} ({}) to {}", size(bytes), dir.display())
}

/// A folder to export into: missing, or empty.
fn check_target(dir: &Path) -> Result<()> {
    let taken = fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) || dir.is_file();
    if taken {
        return Err(CliError::new(format!("{} already exists", dir.display()))
            .fix("Name another folder, as in cave export ../backup, or remove that one"));
    }
    Ok(())
}

/// Whether to create `dir`, which the user named and which does not
/// exist: asked in a terminal, with yes as the default; without one it is
/// created and the caller says so (D38).
fn may_create(dir: &Path) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(true);
    }
    eprint!("{} does not exist. Create it? [Y/n] ", dir.display());
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(answer_is_yes(&answer))
}

/// An empty answer counts as yes.
fn answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "" | "y" | "Y" | "yes" | "Yes")
}

/// `cave export [folder] [--from <snapshot>]`.
pub fn run(folder: Option<PathBuf>, snapshot: Option<String>) -> Result<()> {
    let project = project::require()?;
    let target = match folder {
        Some(dir) => {
            if !dir.exists() {
                if !may_create(&dir)? {
                    return Err(CliError::new("nothing was exported"));
                }
                if !io::stdin().is_terminal() {
                    eprintln!("Creating {}", dir.display());
                }
            }
            dir
        }
        None => PathBuf::from(format!("./{}-export-{}", folder_name(&project), today()?)),
    };
    check_target(&target)?;
    let main = vm_id(&project, None);
    let (vm, _fork) = match snapshot {
        None => {
            ensure_running(&main, None, Vec::new(), false)?;
            (main, None)
        }
        Some(snapshot) => {
            let name = format!("export-{}", std::process::id());
            let Reply::Forked { credentials, .. } = Connection::call(&Request::Fork {
                vm: main.clone(),
                name: Some(name.clone()),
                from: Some(snapshot.clone()),
            })?
            else {
                return Err(CliError::new("the daemon sent an unexpected reply to fork"));
            };
            if !credentials.is_empty() {
                let source = format!("snapshot {snapshot}");
                let warning = credential_warning(&source, &credentials, "the exported files");
                eprintln!("{warning}");
            }
            let fork = TempFork {
                project: main,
                name: name.clone(),
            };
            (vm_id(&project, Some(name)), Some(fork))
        }
    };
    let created = !target.exists();
    if let Err(e) = copy::from_vm_folder(&vm, WORKSPACE, &target) {
        if created {
            let _ = fs::remove_dir_all(&target);
        }
        return Err(e);
    }
    let (files, bytes) = count(&target);
    println!("{}", exported(files, bytes, &target));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_matches_the_docs() {
        assert_eq!(
            exported(1204, 38 << 20, Path::new("./my-project-export-2026-10-12")),
            "Exported 1,204 files (38 MB) to ./my-project-export-2026-10-12"
        );
        assert_eq!(
            exported(1, 100, Path::new("out")),
            "Exported 1 file (1 KB) to out"
        );
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(999), "999");
        assert_eq!(size(3 << 30), "3.0 GB");
    }

    #[test]
    fn enter_means_create() {
        assert!(answer_is_yes("\n"));
        assert!(answer_is_yes("y\n"));
        assert!(!answer_is_yes("n\n"));
        assert!(!answer_is_yes("no\n"));
    }

    #[test]
    fn a_folder_with_files_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("cave-export-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(check_target(&dir).is_ok());
        fs::create_dir_all(&dir).unwrap();
        assert!(check_target(&dir).is_ok());
        fs::write(dir.join("x"), "x").unwrap();
        let e = check_target(&dir).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
