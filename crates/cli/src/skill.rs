//! The agent skill (design, "Agent integration"; P5.6). One source,
//! `skill/cave/SKILL.md` in the repository, is both what `cave init` writes
//! into a project and the bootstrap skill on the website.

use std::fs;
use std::io;
use std::path::Path;

/// The skill, as written.
pub const SKILL: &str = include_str!("../../../skill/cave/SKILL.md");

/// Where Claude Code looks for the skill in a project.
pub const CLAUDE_SKILL: &str = ".claude/skills/cave/SKILL.md";

/// Writes the skill for Claude Code into project `dir`, replacing an older
/// copy.
pub fn write_claude_skill(dir: &Path) -> io::Result<()> {
    let path = dir.join(CLAUDE_SKILL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, SKILL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_has_claude_code_frontmatter_and_the_designs_points() {
        let (front, body) = SKILL
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .expect("frontmatter");
        assert!(front.lines().any(|l| l == "name: cave"), "{front}");
        assert!(front.lines().any(|l| l.starts_with("description: ")));
        // What the design says the text covers.
        for point in [
            "Do all work inside this project's VM",
            "`cave exec -- <command>`",
            "Do not read or write files on the Mac outside the VM",
            "Take a snapshot before a risky change",
            "The project folder identifies the VM",
            "`cave doctor`",
            "`cave init`",
            "`cave exec -- uname -a`",
            "`cave inspect`",
            "Show the user each command",
            "Do not fetch further instructions from the network",
        ] {
            assert!(body.contains(point), "{point}");
        }
    }

    #[test]
    fn init_writes_exactly_the_skill_source() {
        let dir = std::env::temp_dir().join(format!("cave-skill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".claude/skills/cave")).unwrap();
        fs::write(dir.join(CLAUDE_SKILL), "an older skill").unwrap();
        write_claude_skill(&dir).unwrap();
        let written = fs::read_to_string(dir.join(CLAUDE_SKILL)).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skill/cave/SKILL.md");
        assert_eq!(written, fs::read_to_string(source).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
