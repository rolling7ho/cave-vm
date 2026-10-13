//! Reading `cave.toml` (`docs/config.md`), and writing the image pin into it
//! without touching anything else (D22, D27).

use std::fmt;
use std::path::Path;
use std::time::Duration;

use cave_vm_proto::daemon::{NetMode, Storage};
use toml_edit::{Document, DocumentMut, Item, Key, Table, TableLike, Value};

/// The file name in a project folder.
pub const FILE: &str = "cave.toml";

const KIB: u64 = 1024;

/// A project's settings, with every default filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `base`, or pinned as `base@sha256:<digest>`.
    pub image: String,
    pub cpus: u32,
    pub memory_bytes: u64,
    /// `None` is `"off"`.
    pub idle_suspend: Option<Duration>,
    pub ttl: Option<Duration>,
    pub storage: Storage,
    pub disk_bytes: u64,
    /// `Off` or `Nat`; `lan` is refused in the file.
    pub net: NetMode,
    pub snapshots_auto: bool,
    pub snapshots_keep: u32,
    pub updates_check: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            image: "base".into(),
            cpus: 2,
            memory_bytes: 2 * KIB * KIB * KIB,
            idle_suspend: Some(Duration::from_secs(30 * 60)),
            ttl: None,
            storage: Storage::Sealed,
            disk_bytes: 20 * KIB * KIB * KIB,
            net: NetMode::Nat,
            snapshots_auto: true,
            snapshots_keep: 10,
            updates_check: true,
        }
    }
}

impl Config {
    /// The image name without its pin.
    pub fn image_name(&self) -> &str {
        self.image
            .split_once('@')
            .map_or(&self.image, |(name, _)| name)
    }

    /// The pinned digest (hex, without `sha256:`), if any.
    pub fn image_digest(&self) -> Option<&str> {
        self.image.split_once("@sha256:").map(|(_, digest)| digest)
    }
}

/// What is wrong with a `cave.toml`, in the shape the CLI prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// 1-based, when the problem has a place in the file.
    pub line: Option<usize>,
    pub message: String,
    pub fix: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "cave.toml line {line}: {}", self.message),
            None => write!(f, "cave.toml: {}", self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads `cave.toml` in `project`.
pub fn load(project: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(project.join(FILE)).map_err(|e| ConfigError {
        line: None,
        message: format!("cannot read {}: {e}", project.join(FILE).display()),
        fix: "Run `cave init` to create it".into(),
    })?;
    parse(&text)
}

/// Parses the text of a `cave.toml`.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let doc = Document::parse(text).map_err(|e| ConfigError {
        line: e.span().map(|s| line_of(text, s.start)),
        message: e.message().trim().to_string(),
        fix: "Fix the TOML syntax; see docs/config.md for the format".into(),
    })?;
    let reader = Reader { text };
    let mut config = Config::default();
    let root = doc.as_table();
    for (name, item) in root.iter() {
        let key = key_of(root, name);
        let Some(table) = item.as_table_like() else {
            return Err(reader.error(
                key,
                format!("{name} must be a table"),
                format!("put it under a [{name}] heading"),
            ));
        };
        match name {
            "vm" => reader.vm(table, &mut config)?,
            "storage" => reader.storage(table, &mut config)?,
            "network" => reader.network(table, &mut config)?,
            "snapshots" => reader.snapshots(table, &mut config)?,
            "updates" => reader.updates(table, &mut config)?,
            _ => {
                return Err(reader.error(
                    key,
                    format!("unknown section [{name}]"),
                    "the sections are [vm], [storage], [network], [snapshots] and [updates]".into(),
                ));
            }
        }
    }
    Ok(config)
}

/// Writes `image` (such as `base@sha256:...`) as `[vm] image`, keeping the
/// rest of the file, comments and layout as they were.
pub fn pin_image(text: &str, image: &str) -> Result<String, ConfigError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| ConfigError {
            line: e.span().map(|s| line_of(text, s.start)),
            message: e.message().trim().to_string(),
            fix: "Fix the TOML syntax; see docs/config.md for the format".into(),
        })?;
    let root = doc.as_table_mut();
    if !root.contains_key("vm") {
        root.insert("vm", Item::Table(Table::new()));
    }
    let Some(vm) = root["vm"].as_table_like_mut() else {
        return Err(ConfigError {
            line: None,
            message: "vm must be a table".into(),
            fix: "put it under a [vm] heading".into(),
        });
    };
    let mut value = Value::from(image);
    match vm.get_mut("image").and_then(Item::as_value_mut) {
        // Replace the value in place, so the key keeps its spacing and the
        // value its comment.
        Some(old) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        None => {
            vm.insert("image", Item::Value(value));
        }
    }
    Ok(doc.to_string())
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

fn key_of<'a>(table: &'a dyn TableLike, name: &str) -> Option<&'a Key> {
    table.get_key_value(name).map(|(k, _)| k)
}

struct Reader<'a> {
    text: &'a str,
}

impl Reader<'_> {
    fn error(&self, key: Option<&Key>, message: String, fix: String) -> ConfigError {
        ConfigError {
            line: key.and_then(Key::span).map(|s| line_of(self.text, s.start)),
            message,
            fix,
        }
    }

    /// Calls `f` for each key of `table`, refusing keys not in `known`.
    fn keys<'t>(
        &self,
        section: &str,
        table: &'t dyn TableLike,
        known: &[&str],
        mut f: impl FnMut(&str, Option<&'t Key>, &'t Item) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        for (name, item) in table.iter() {
            let key = key_of(table, name);
            if !known.contains(&name) {
                return Err(self.error(
                    key,
                    format!("unknown key {name} in [{section}]"),
                    format!("the keys in [{section}] are {}", list(known)),
                ));
            }
            f(name, key, item)?;
        }
        Ok(())
    }

    fn string<'i>(
        &self,
        name: &str,
        key: Option<&Key>,
        item: &'i Item,
        example: &str,
    ) -> Result<&'i str, ConfigError> {
        item.as_str().ok_or_else(|| {
            self.error(
                key,
                format!("{name} must be a string"),
                format!("write {name} = \"{example}\""),
            )
        })
    }

    fn integer(
        &self,
        name: &str,
        key: Option<&Key>,
        item: &Item,
        min: i64,
        example: &str,
    ) -> Result<u32, ConfigError> {
        item.as_integer()
            .filter(|&n| n >= min)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                self.error(
                    key,
                    format!("{name} must be a whole number of at least {min}"),
                    format!("write {name} = {example}"),
                )
            })
    }

    fn boolean(&self, name: &str, key: Option<&Key>, item: &Item) -> Result<bool, ConfigError> {
        item.as_bool().ok_or_else(|| {
            self.error(
                key,
                format!("{name} must be true or false"),
                format!("write {name} = true or {name} = false"),
            )
        })
    }

    fn size(
        &self,
        name: &str,
        key: Option<&Key>,
        item: &Item,
        example: &str,
    ) -> Result<u64, ConfigError> {
        let fix = || format!("write {name} = \"{example}\" (units: K, M, G, T)");
        if let Some(n) = item.as_integer() {
            return Err(self.error(key, format!("{name} = {n} needs a unit"), fix()));
        }
        let text = self.string(name, key, item, example)?;
        parse_size(text)
            .ok_or_else(|| self.error(key, format!("{name} = \"{text}\" is not a size"), fix()))
    }

    fn duration(
        &self,
        name: &str,
        key: Option<&Key>,
        item: &Item,
        example: &str,
    ) -> Result<Option<Duration>, ConfigError> {
        let fix =
            || format!("write {name} = \"{example}\" (units: s, m, h, d) or {name} = \"off\"");
        if let Some(n) = item.as_integer() {
            return Err(self.error(key, format!("{name} = {n} needs a unit"), fix()));
        }
        let text = self.string(name, key, item, example)?;
        if text == "off" {
            return Ok(None);
        }
        parse_duration(text)
            .map(Some)
            .ok_or_else(|| self.error(key, format!("{name} = \"{text}\" is not a duration"), fix()))
    }

    fn vm(&self, table: &dyn TableLike, config: &mut Config) -> Result<(), ConfigError> {
        self.keys(
            "vm",
            table,
            &["image", "cpus", "memory", "idle_suspend", "ttl"],
            |name, key, item| {
                match name {
                    "image" => {
                        let image = self.string(name, key, item, "base")?;
                        if !valid_image(image) {
                            return Err(self.error(
                                key,
                                format!("image = \"{image}\" is not an image name"),
                                "write image = \"base\", or a name from `cave images`".into(),
                            ));
                        }
                        config.image = image.to_string();
                    }
                    "cpus" => config.cpus = self.integer(name, key, item, 1, "2")?,
                    "memory" => config.memory_bytes = self.size(name, key, item, "2G")?,
                    "idle_suspend" => {
                        config.idle_suspend = self.duration(name, key, item, "30m")?
                    }
                    "ttl" => config.ttl = self.duration(name, key, item, "8h")?,
                    _ => unreachable!("checked by keys"),
                }
                Ok(())
            },
        )
    }

    fn storage(&self, table: &dyn TableLike, config: &mut Config) -> Result<(), ConfigError> {
        self.keys("storage", table, &["mode", "disk"], |name, key, item| {
            match name {
                "mode" => {
                    config.storage = match self.string(name, key, item, "sealed")? {
                        "sealed" => Storage::Sealed,
                        "synced" => Storage::Synced,
                        other => {
                            return Err(self.error(
                                key,
                                format!("storage mode \"{other}\" is not a mode"),
                                "write mode = \"sealed\" or mode = \"synced\"".into(),
                            ));
                        }
                    }
                }
                "disk" => config.disk_bytes = self.size(name, key, item, "20G")?,
                _ => unreachable!("checked by keys"),
            }
            Ok(())
        })
    }

    fn network(&self, table: &dyn TableLike, config: &mut Config) -> Result<(), ConfigError> {
        self.keys("network", table, &["mode"], |name, key, item| {
            config.net = match self.string(name, key, item, "nat")? {
                "nat" => NetMode::Nat,
                "off" => NetMode::Off,
                "lan" => {
                    return Err(self.error(
                        key,
                        "network mode \"lan\" cannot be set in cave.toml".into(),
                        "pass --net lan to cave run when you need LAN access".into(),
                    ));
                }
                other => {
                    return Err(self.error(
                        key,
                        format!("network mode \"{other}\" is not a mode"),
                        "write mode = \"nat\" or mode = \"off\"".into(),
                    ));
                }
            };
            Ok(())
        })
    }

    fn snapshots(&self, table: &dyn TableLike, config: &mut Config) -> Result<(), ConfigError> {
        self.keys("snapshots", table, &["auto", "keep"], |name, key, item| {
            match name {
                "auto" => config.snapshots_auto = self.boolean(name, key, item)?,
                "keep" => config.snapshots_keep = self.integer(name, key, item, 0, "10")?,
                _ => unreachable!("checked by keys"),
            }
            Ok(())
        })
    }

    fn updates(&self, table: &dyn TableLike, config: &mut Config) -> Result<(), ConfigError> {
        self.keys("updates", table, &["check"], |name, key, item| {
            config.updates_check = self.boolean(name, key, item)?;
            Ok(())
        })
    }
}

fn list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `base`, or `base@sha256:` and 64 hex digits.
fn valid_image(image: &str) -> bool {
    let (name, pin) = match image.split_once('@') {
        Some((name, pin)) => (name, Some(pin)),
        None => (image, None),
    };
    let name_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    let pin_ok = pin.is_none_or(|pin| {
        pin.strip_prefix("sha256:")
            .is_some_and(|d| d.len() == 64 && d.chars().all(|c| c.is_ascii_hexdigit()))
    });
    name_ok && pin_ok
}

/// `"512M"`, `"2G"`: a whole number and a unit, powers of 1024.
pub fn parse_size(text: &str) -> Option<u64> {
    let unit = text.chars().last()?;
    let shift = match unit.to_ascii_uppercase() {
        'K' => 10,
        'M' => 20,
        'G' => 30,
        'T' => 40,
        _ => return None,
    };
    let n: u64 = text[..text.len() - 1].parse().ok()?;
    if n == 0 {
        return None;
    }
    n.checked_mul(1 << shift)
}

/// `"30m"`, `"8h"`: a whole number and a unit.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let unit = text.chars().last()?;
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    let n: u64 = text[..text.len() - 1].parse().ok()?;
    if n == 0 {
        return None;
    }
    n.checked_mul(seconds).map(Duration::from_secs)
}

#[cfg(test)]
mod tests;
