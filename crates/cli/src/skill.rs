//! The agent skill files (design, "Agent integration"; P5.6, P5.7). One
//! source, `skill/cave/SKILL.md` in the repository, is both what `cave init`
//! writes into a project and the bootstrap skill on the website. `cave init`
//! writes it as a Claude Code skill and as `.cave/AGENTS.md`, with a line
//! in the project's `AGENTS.md` pointing there (D37), and
//! sets Claude Code up to use `cave mcp` with host Bash and reading `.env`
//! denied: the skill is guidance and the config is the guardrail.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::agent;
use crate::error::{CliError, Result};

/// The skill, as written.
pub const SKILL: &str = include_str!("../../../skill/cave/SKILL.md");

/// Where Claude Code looks for the skill in a project.
pub const CLAUDE_SKILL: &str = ".claude/skills/cave/SKILL.md";

/// The file of agent instructions that the pointer goes into.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// Where the skill's text goes for agents that read `AGENTS.md` (D37).
pub const CAVE_AGENTS_FILE: &str = ".cave/AGENTS.md";

/// The line in the project's `AGENTS.md`, between the markers, that sends
/// agents to [`CAVE_AGENTS_FILE`].
const POINTER: &str = "This project runs in a cave VM. Before doing anything, read .cave/AGENTS.md: it says how to work inside the VM.\n";

/// The marker comments around the cave section of `AGENTS.md`.
const SECTION_START: &str = "<!-- cave:start -->";
const SECTION_END: &str = "<!-- cave:end -->";

/// Claude Code's project settings, where host Bash and reading `.env` are
/// denied.
pub const CLAUDE_SETTINGS: &str = ".claude/settings.json";

/// Claude Code's project MCP servers, where `cave mcp` is registered.
pub const MCP_CONFIG: &str = ".mcp.json";

/// The skill without its frontmatter, for `.cave/AGENTS.md`.
pub fn body() -> &'static str {
    SKILL
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(SKILL, |(_, body)| body)
        .trim_start_matches('\n')
}

/// Writes every skill file into project `dir`.
pub fn write_all(dir: &Path) -> Result<()> {
    write_claude_skill(dir)?;
    write_agents_section(dir)?;
    update_json(dir, CLAUDE_SETTINGS, deny_host_tools)?;
    update_json(dir, MCP_CONFIG, register_mcp)?;
    Ok(())
}

/// Writes the skill for Claude Code into project `dir`, replacing an older
/// copy.
pub fn write_claude_skill(dir: &Path) -> io::Result<()> {
    let path = dir.join(CLAUDE_SKILL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, SKILL)
}

/// Writes the skill's text to `.cave/AGENTS.md`, and the line pointing
/// there into `AGENTS.md` between the marker comments, keeping everything
/// else. A section already there, such as the whole text an older cave
/// wrote, is replaced; otherwise the line goes at the end, and a project
/// with no `AGENTS.md` gets one with just the line, since agents read only
/// that file on their own (D37).
pub fn write_agents_section(dir: &Path) -> io::Result<()> {
    write_cave_agents(dir)?;
    let path = dir.join(AGENTS_FILE);
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let section = format!("{SECTION_START}\n{POINTER}{SECTION_END}");
    let start = existing.find(SECTION_START);
    let end = start.and_then(|s| existing[s..].find(SECTION_END).map(|e| s + e));
    let text = match (start, end) {
        (Some(start), Some(end)) => format!(
            "{}{section}{}",
            &existing[..start],
            &existing[end + SECTION_END.len()..]
        ),
        _ if existing.trim().is_empty() => format!("{section}\n"),
        _ => format!("{}\n\n{section}\n", existing.trim_end()),
    };
    fs::write(path, text)
}

/// Writes the skill's text to `.cave/AGENTS.md` in project `dir`.
pub fn write_cave_agents(dir: &Path) -> io::Result<()> {
    let path = dir.join(CAVE_AGENTS_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body())
}

/// Adds [`agent::DENIED_TOOLS`] to Claude Code's denied tools, so the
/// agent has no shell on the Mac and cannot read `.env`. Claude Code asks
/// the user once to approve the `cave` server from `.mcp.json`; a
/// project's own settings do not approve it.
fn deny_host_tools(settings: &mut Map<String, Value>) -> std::result::Result<(), String> {
    let permissions = object(settings, "permissions")?;
    agent::DENIED_TOOLS
        .iter()
        .try_for_each(|tool| push_unique(permissions, "deny", tool))
}

/// Registers `cave mcp` as the `cave` server.
fn register_mcp(config: &mut Map<String, Value>) -> std::result::Result<(), String> {
    let servers = object(config, "mcpServers")?;
    servers.insert(
        "cave".into(),
        json!({"type": "stdio", "command": "cave", "args": ["mcp"]}),
    );
    Ok(())
}

/// The object under `key`, created if missing.
fn object<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> std::result::Result<&'a mut Map<String, Value>, String> {
    map.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("\"{key}\" is not an object"))
}

/// Adds `item` to the list under `key` unless it is there.
fn push_unique(
    map: &mut Map<String, Value>,
    key: &str,
    item: &str,
) -> std::result::Result<(), String> {
    let list = map
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| format!("\"{key}\" is not a list"))?;
    if !list.iter().any(|v| v.as_str() == Some(item)) {
        list.push(item.into());
    }
    Ok(())
}

/// Applies `change` to the JSON object in `dir`/`file`, creating the file
/// if needed. A file that already has what `change` adds is left alone.
fn update_json(
    dir: &Path,
    file: &str,
    change: fn(&mut Map<String, Value>) -> std::result::Result<(), String>,
) -> Result<()> {
    let path = dir.join(file);
    let cannot = |why: String| {
        CliError::new(format!("cannot update {file}: {why}")).fix(format!(
            "Fix or remove {}, then run `cave init` again",
            path.display()
        ))
    };
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(cannot(e.to_string())),
    };
    let mut value = match &existing {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).map_err(|e| cannot(format!("it is not valid JSON ({e})")))?
        }
        _ => Value::Object(Map::new()),
    };
    let before = value.clone();
    let map = value
        .as_object_mut()
        .ok_or_else(|| cannot("it is not a JSON object".into()))?;
    change(map).map_err(cannot)?;
    if existing.is_some() && value == before {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&value).expect("JSON");
    fs::write(&path, text + "\n")?;
    Ok(())
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
            "`uname -m` must print `arm64`",
            "26 or later",
            "`brew install rolling7ho/cave/cave-vm`",
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
    fn the_bootstrap_page_copies_exactly_the_skill() {
        let page = include_str!("../../../docs/bootstrap.md");
        let block = page
            .split_once("````markdown\n")
            .and_then(|(_, rest)| rest.split_once("````"))
            .expect("the copy block")
            .0;
        assert_eq!(block, SKILL);
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

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cave-skill-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn agents_md_points_at_the_skill_between_markers_and_keeps_the_rest() {
        let dir = temp("agents");
        let section = format!(
            "{SECTION_START}\nThis project runs in a cave VM. Before doing anything, read .cave/AGENTS.md: it says how to work inside the VM.\n{SECTION_END}"
        );
        assert!(body().starts_with("# cave: work inside the project's VM\n"));

        // A new file holds just the pointer, and the text is in .cave/.
        write_agents_section(&dir).unwrap();
        let path = dir.join(AGENTS_FILE);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{section}\n"));
        assert_eq!(
            fs::read_to_string(dir.join(CAVE_AGENTS_FILE)).unwrap(),
            body()
        );

        // Existing content is kept and the section goes at the end.
        fs::write(&path, "# My project\n\nUse tabs.\n").unwrap();
        write_agents_section(&dir).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("# My project\n\nUse tabs.\n\n{section}\n"));

        // An old section, such as the whole text, is replaced in place by
        // the pointer, and text after it is kept.
        fs::write(
            &path,
            format!(
                "# Mine\n\n{SECTION_START}\n{}{SECTION_END}\n\n## After\n",
                body()
            ),
        )
        .unwrap();
        write_agents_section(&dir).unwrap();
        write_agents_section(&dir).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("# Mine\n\n{section}\n\n## After\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_gets_cave_mcp_with_host_bash_and_env_denied() {
        let dir = temp("claude");
        write_all(&dir).unwrap();
        assert_eq!(
            read_json(&dir.join(CLAUDE_SETTINGS)),
            json!({"permissions": {"deny": ["Bash", "Read(./.env)"]}})
        );
        assert_eq!(
            read_json(&dir.join(MCP_CONFIG)),
            json!({"mcpServers": {"cave": {"type": "stdio", "command": "cave", "args": ["mcp"]}}})
        );
        assert!(dir.join(CLAUDE_SKILL).is_file());
        assert!(dir.join(AGENTS_FILE).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_settings_are_kept_and_not_rewritten_when_complete() {
        let dir = temp("merge");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(CLAUDE_SETTINGS),
            r#"{"model": "x", "permissions": {"allow": ["Read"], "deny": ["WebFetch"]}}"#,
        )
        .unwrap();
        fs::write(
            dir.join(MCP_CONFIG),
            r#"{"mcpServers": {"other": {"command": "o"}}}"#,
        )
        .unwrap();
        write_all(&dir).unwrap();
        assert_eq!(
            read_json(&dir.join(CLAUDE_SETTINGS)),
            json!({
                "model": "x",
                "permissions": {"allow": ["Read"], "deny": ["WebFetch", "Bash", "Read(./.env)"]}
            })
        );
        let servers = read_json(&dir.join(MCP_CONFIG))["mcpServers"].clone();
        assert_eq!(servers["other"], json!({"command": "o"}));
        assert_eq!(servers["cave"]["args"], json!(["mcp"]));

        // Complete already: the file is left byte for byte.
        let complete = "{\"permissions\":{\"deny\":[\"Bash\",\"Read(./.env)\"]}}";
        fs::write(dir.join(CLAUDE_SETTINGS), complete).unwrap();
        write_all(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(CLAUDE_SETTINGS)).unwrap(),
            complete
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_settings_file_says_how_to_fix_it_and_is_left_alone() {
        let dir = temp("broken");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(CLAUDE_SETTINGS), "{ not json").unwrap();
        let e = write_all(&dir).unwrap_err().to_string();
        assert!(
            e.contains("cannot update .claude/settings.json: it is not valid JSON"),
            "{e}"
        );
        assert!(e.contains("then run `cave init` again"), "{e}");
        assert_eq!(
            fs::read_to_string(dir.join(CLAUDE_SETTINGS)).unwrap(),
            "{ not json"
        );

        fs::write(dir.join(CLAUDE_SETTINGS), r#"{"permissions": []}"#).unwrap();
        let e = write_all(&dir).unwrap_err().to_string();
        assert!(e.contains("\"permissions\" is not an object"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }
}
