//! `cave init`, and the setup `run`, `shell` and `exec` do on first use.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use cave_vm_proto::daemon::Storage;

use crate::error::{CliError, Result};
use crate::project::{self, CONFIG_FILE, STATE_DIR};
use crate::skill;
use crate::ssh;

/// Image names `cave init --image` accepts, and what each stands for.
const IMAGES: [(&str, &str); 3] = [
    ("base", "debian"),
    ("debian", "debian"),
    ("ubuntu", "ubuntu"),
];

fn config(storage: Storage, image: &str) -> String {
    let mode = match storage {
        Storage::Sealed => "sealed",
        Storage::Synced => "synced",
    };
    format!(
        r#"[vm]
image   = "{image}"{pad}# pinned to a digest on first run
cpus    = 2
memory  = "2G"

[storage]
mode    = "{mode}"{pad2}# or "{other}"

[network]
mode    = "nat"         # off | nat
"#,
        pad = " ".repeat(14usize.saturating_sub(image.len() + 2).max(1)),
        pad2 = " ".repeat(14usize.saturating_sub(mode.len() + 2).max(1)),
        other = if storage == Storage::Sealed {
            "synced"
        } else {
            "sealed"
        },
    )
}

/// The checks before setting up `dir`: not the home folder, not inside
/// another project.
fn check_folder(dir: &Path) -> Result<()> {
    let home = project::home();
    if home.as_deref() == Some(dir) {
        return Err(
            CliError::new("cave cannot set up a project in your home folder itself")
                .fix("Run `cave init` in a project folder instead"),
        );
    }
    if let Some(parent) = dir.parent()
        && let Some(outer) = project::find(parent, home.as_deref())
    {
        return Err(CliError::new(format!(
            "this folder is inside the project at {}",
            outer.display()
        ))
        .fix("Run cave from that project, or move this folder out of it"));
    }
    Ok(())
}

/// Adds `.cave/` to `.gitignore`. Returns whether it changed anything.
fn ignore_state_dir(dir: &Path) -> io::Result<bool> {
    let path = dir.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let ignored = existing
        .lines()
        .any(|l| matches!(l.trim(), ".cave" | ".cave/" | "/.cave" | "/.cave/"));
    if ignored {
        return Ok(false);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(b".cave/\n")?;
    Ok(true)
}

fn ask_storage() -> Result<Storage> {
    eprint!(
        "Keep a copy of the code on your Mac? It lets you use your Mac editor and Spotlight. [y/N] "
    );
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        Storage::Synced
    } else {
        Storage::Sealed
    })
}

/// The question next to [`ask_storage`] (D55).
const SKIP_PROMPTS_QUESTION: &str = "Let Claude Code skip its permission prompts in this project? Its own tools on the Mac are turned off, so it can act only inside the VM, which is its sandbox. [y/N] ";

/// Whether Claude Code skips its prompts in project `dir`: yes if an
/// earlier yes is in its personal settings, otherwise asked when stdin is
/// a terminal. Asked in an existing project too, so one set up before the
/// question existed can still say yes.
fn skip_prompts_answer(dir: &Path) -> Result<bool> {
    if skill::prompts_skipped(dir) {
        return Ok(true);
    }
    Ok(io::stdin().is_terminal() && ask_skip_prompts()?)
}

fn ask_skip_prompts() -> Result<bool> {
    eprint!("{SKIP_PROMPTS_QUESTION}");
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Adds `entry` to `.gitignore` unless a line already names it. Returns
/// whether it changed anything.
fn ignore(dir: &Path, entry: &str) -> io::Result<bool> {
    let path = dir.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing
        .lines()
        .any(|l| l.trim().trim_start_matches('/') == entry)
    {
        return Ok(false);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(format!("{entry}\n").as_bytes())?;
    Ok(true)
}

/// `cave init`.
pub fn run(sealed: bool, synced: bool, image: &str) -> Result<()> {
    let dir = project::current_dir()?;
    check_folder(&dir)?;
    let Some((_, distro)) = IMAGES.iter().find(|(name, _)| *name == image) else {
        return Err(CliError::new(format!("unknown image \"{image}\""))
            .fix("Run `cave images` to see the image names"));
    };
    let config_path = dir.join(CONFIG_FILE);
    let skip_prompts;
    if config_path.exists() {
        println!("cave.toml already exists; left it unchanged");
        skip_prompts = skip_prompts_answer(&dir)?;
    } else {
        let storage = if synced {
            Storage::Synced
        } else if sealed {
            Storage::Sealed
        } else if io::stdin().is_terminal() {
            ask_storage()?
        } else {
            println!("No terminal to ask, so the project is sealed: code lives only in the VM");
            Storage::Sealed
        };
        skip_prompts = skip_prompts_answer(&dir)?;
        write_project(&dir, storage, image)?;
        let mode = if storage == Storage::Sealed {
            "sealed"
        } else {
            "synced"
        };
        println!("Created cave.toml ({mode}, {distro}, 2 CPUs, 2 GB)");
    }
    fs::create_dir_all(dir.join(STATE_DIR))?;
    if ignore_state_dir(&dir)? {
        println!("Added .cave/ to .gitignore");
    }
    skill::write_all(&dir)?;
    println!("Wrote .cave/AGENTS.md, a line pointing to it in AGENTS.md, and .claude/skills/cave");
    println!(
        "Set up Claude Code to use cave mcp, with host Bash, Mac edits and reading .env denied (.mcp.json, .claude/settings.json)"
    );
    if skip_prompts {
        skill::skip_prompts(&dir)?;
        println!(
            "Claude Code skips its prompts here, with every tool that reaches the Mac denied (.claude/settings.local.json)"
        );
        if ignore(&dir, skill::CLAUDE_LOCAL_SETTINGS)? {
            println!("Added {} to .gitignore", skill::CLAUDE_LOCAL_SETTINGS);
        }
    }
    let (host, included) = ssh::set_up(&dir)?;
    if included {
        println!("Added an Include line to ~/.ssh/config for cave's SSH hosts");
    }
    println!("Edit in the VM: connect VS Code Remote-SSH or Zed to {host}");
    println!(
        "Next: cave agent claude or cave agent codex; any other agent or command runs in the VM with cave shell or cave exec"
    );
    Ok(())
}

fn write_project(dir: &Path, storage: Storage, image: &str) -> Result<()> {
    fs::write(dir.join(CONFIG_FILE), config(storage, image))?;
    Ok(())
}

/// The project for `run`, `shell` and `exec`: the one this folder is in,
/// or this folder, set up with the defaults (docs/cli.md, "Setting up on
/// first use").
pub fn project_or_set_up() -> Result<std::path::PathBuf> {
    let dir = project::current_dir()?;
    if let Some(found) = project::find(&dir, project::home().as_deref()) {
        // `.cave/` goes with `cave rm`; the text the AGENTS.md line points
        // to comes back with the VM (D37).
        if !found.join(skill::CAVE_AGENTS_FILE).exists() {
            let _ = skill::write_cave_agents(&found);
        }
        return Ok(found);
    }
    check_folder(&dir)?;
    write_project(&dir, Storage::Sealed, "base")?;
    fs::create_dir_all(dir.join(STATE_DIR))?;
    ignore_state_dir(&dir)?;
    skill::write_all(&dir)?;
    ssh::set_up(&dir)?;
    eprintln!(
        "Set up this folder as a cave project with the defaults (sealed, debian); see cave.toml"
    );
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_adds_an_entry_once() {
        let dir = std::env::temp_dir().join(format!("cave-ignore-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".gitignore"), ".cave/").unwrap();
        assert!(ignore(&dir, ".claude/settings.local.json").unwrap());
        assert!(!ignore(&dir, ".claude/settings.local.json").unwrap());
        fs::write(dir.join(".gitignore"), "/.claude/settings.local.json\n").unwrap();
        assert!(!ignore(&dir, ".claude/settings.local.json").unwrap());
        fs::write(dir.join(".gitignore"), ".cave/").unwrap();
        ignore(&dir, ".claude/settings.local.json").unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(".gitignore")).unwrap(),
            ".cave/\n.claude/settings.local.json\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_written_config_is_the_documented_minimal_file() {
        assert_eq!(
            config(Storage::Sealed, "base"),
            r#"[vm]
image   = "base"        # pinned to a digest on first run
cpus    = 2
memory  = "2G"

[storage]
mode    = "sealed"      # or "synced"

[network]
mode    = "nat"         # off | nat
"#
        );
    }
}
