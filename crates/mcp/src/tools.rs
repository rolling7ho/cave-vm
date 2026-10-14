//! The tools: exec, read_file, write_file, snapshot, restore, fork and
//! destroy (design, "Agent integration").
//!
//! Each call opens the project's [`Sandbox`], or the fork named by the
//! `fork` argument, and returns text for the agent. Failures come back as
//! the text of the error, with its fix, so the agent can act on them.

use std::path::PathBuf;

use cave_vm_sdk::{Command, Daemon, Error, Output, Sandbox};
use serde_json::{Value, json};

/// Bytes of each output stream an `exec` result keeps: the first and last
/// halves, with the middle cut, so a long build log does not flood the
/// agent's context.
pub const OUTPUT_LIMIT: usize = 100_000;

/// Largest file `read_file` returns.
pub const READ_LIMIT: usize = 1 << 20;

/// Where the tools find the VM.
pub struct Tools {
    project: PathBuf,
    daemon: Option<Daemon>,
    /// The fork the tools act on when a call names none.
    fork: Option<String>,
}

/// A tool's text, or the text of why it failed.
pub type Outcome = Result<String, String>;

impl Tools {
    /// Tools for the project `project` is in, through the daemon `cave`
    /// uses.
    pub fn new(project: impl Into<PathBuf>) -> Tools {
        Tools {
            project: project.into(),
            daemon: None,
            fork: None,
        }
    }

    /// These tools, acting on fork `name` unless a call names another
    /// (`cave agent --fork`).
    pub fn on_fork(self, name: Option<String>) -> Tools {
        Tools { fork: name, ..self }
    }

    /// Tools for the project `project` is in, through `daemon`.
    pub fn with_daemon(project: impl Into<PathBuf>, daemon: Daemon) -> Tools {
        Tools {
            project: project.into(),
            daemon: Some(daemon),
            fork: None,
        }
    }

    /// The tool definitions for `tools/list`.
    pub fn list() -> Value {
        let fork = json!({
            "type": "string",
            "description": "A fork's name, from the fork tool, to act on that fork instead of the project's VM."
        });
        json!([
            {
                "name": "exec",
                "description": "Run a shell command in the project's VM with bash, and wait for it to finish. Returns the exit code, stdout and stderr. Commands start in /workspace, the project folder, unless cwd is given.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command, run as `bash -c <command>`." },
                        "credentials": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Names of credentials from the project's .env, such as API_KEY, to give the command as environment variables. You never see the values: output that prints one shows [API_KEY] instead."
                        },
                        "cwd": { "type": "string", "description": "The working directory in the VM." },
                        "fork": fork,
                    },
                    "required": ["command"],
                },
            },
            {
                "name": "read_file",
                "description": "Read a text file in the VM, up to 1 MiB. Relative paths are under /workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "fork": fork,
                    },
                    "required": ["path"],
                },
            },
            {
                "name": "write_file",
                "description": "Write a text file in the VM, replacing it, and create missing folders. Relative paths are under /workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "fork": fork,
                    },
                    "required": ["path", "content"],
                },
            },
            {
                "name": "snapshot",
                "description": "Save the VM's state, processes included, under a name, to restore later. Cheap: take one before risky work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Letters, digits, `-` and `_`, such as before-refactor." },
                        "fork": fork,
                    },
                    "required": ["name"],
                },
            },
            {
                "name": "restore",
                "description": "Put the VM back to a snapshot. The state before is saved first as an automatic snapshot, so a restore can be undone.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The snapshot's name." },
                        "fork": fork,
                    },
                    "required": ["name"],
                },
            },
            {
                "name": "fork",
                "description": "Make a copy of the project's VM, copy-on-write, to try something without touching the VM. Returns the fork's name; pass it as `fork` to the other tools.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "A snapshot to fork; the VM's current state when left out." },
                    },
                },
            },
            {
                "name": "destroy",
                "description": "Remove a fork and everything in it. Only forks can be removed here; the project's own VM is removed by the user.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "fork": { "type": "string", "description": "The fork's name." },
                    },
                    "required": ["fork"],
                },
            },
        ])
    }

    /// Runs tool `name` with `arguments`, or `None` if there is no such
    /// tool.
    pub fn call(&self, name: &str, arguments: &Value) -> Option<Outcome> {
        let args = Args(arguments);
        Some(match name {
            "exec" => self.exec(&args),
            "read_file" => self.read_file(&args),
            "write_file" => self.write_file(&args),
            "snapshot" => self.snapshot(&args),
            "restore" => self.restore(&args),
            "fork" => self.fork(&args),
            "destroy" => self.destroy(&args),
            _ => return None,
        })
    }

    /// The project's VM, or the fork named by the `fork` argument or set
    /// with [`Tools::on_fork`].
    fn sandbox(&self, args: &Args<'_>) -> Result<Sandbox, String> {
        let sandbox = self.project_vm()?;
        match args.optional("fork")?.or(self.fork.as_deref()) {
            Some(name) => Ok(sandbox.fork_named(name)),
            None => Ok(sandbox),
        }
    }

    /// The project's own VM.
    fn project_vm(&self) -> Result<Sandbox, String> {
        match &self.daemon {
            Some(daemon) => Sandbox::open_with(daemon.clone(), &self.project),
            None => Sandbox::open(&self.project),
        }
        .map_err(text)
    }

    fn exec(&self, args: &Args<'_>) -> Outcome {
        let mut command = Command::new("bash")
            .arg("-c")
            .arg(args.required("command")?);
        if let Some(cwd) = args.optional("cwd")? {
            command = command.cwd(cwd);
        }
        for name in args.strings("credentials")? {
            command = command.credential(name);
        }
        let output = self.sandbox(args)?.run(&command).map_err(text)?;
        Ok(exec_text(&output))
    }

    fn read_file(&self, args: &Args<'_>) -> Outcome {
        let path = args.required("path")?;
        let contents = self.sandbox(args)?.read_file(path).map_err(text)?;
        if contents.len() > READ_LIMIT {
            return Err(format!(
                "{path} is {} bytes, over the {READ_LIMIT}-byte limit for read_file; read part of it with exec, such as `sed -n 1,200p {path}`",
                contents.len()
            ));
        }
        String::from_utf8(contents).map_err(|e| {
            format!(
                "{path} is not UTF-8 text ({} bytes); look at it with exec, such as `file {path}` or `xxd {path} | head`",
                e.as_bytes().len()
            )
        })
    }

    fn write_file(&self, args: &Args<'_>) -> Outcome {
        let path = args.required("path")?;
        let content = args.required("content")?;
        let size = self
            .sandbox(args)?
            .write_file(path, content.as_bytes())
            .map_err(text)?;
        Ok(format!("Wrote {size} bytes to {}", vm_path(path)))
    }

    fn snapshot(&self, args: &Args<'_>) -> Outcome {
        let name = args.required("name")?;
        let snapshot = self.sandbox(args)?.snapshot(name).map_err(text)?;
        Ok(format!(
            "Saved snapshot {}. Restore it with the restore tool.",
            snapshot.name
        ))
    }

    fn restore(&self, args: &Args<'_>) -> Outcome {
        let name = args.required("name")?;
        let restored = self.sandbox(args)?.restore(name).map_err(text)?;
        let mut out = format!("Restored {}.", restored.name);
        if let Some(reason) = &restored.reason {
            out.push_str(&format!(
                " Its memory could not be loaded ({reason}), so the VM booted from the snapshot's disk and processes from the snapshot were not kept."
            ));
        }
        if let Some(saved) = &restored.saved {
            out.push_str(&format!(
                " The state before was saved as {saved}; restore it to undo this."
            ));
        }
        out.push_str(&memory_note(&restored.credentials, "the VM's memory"));
        Ok(out)
    }

    fn fork(&self, args: &Args<'_>) -> Outcome {
        // Forks are made from the project's VM, whatever fork is set.
        let sandbox = self.project_vm()?;
        let forked = match args.optional("from")? {
            Some(snapshot) => sandbox.fork_from(snapshot),
            None => sandbox.fork(),
        }
        .map_err(text)?;
        let name = forked.sandbox.fork_name().unwrap_or_default();
        let mut out = format!(
            "Forked {name} from {}. Pass fork: \"{name}\" to the other tools to use it, and destroy it when done.",
            forked.from
        );
        if let Some(reason) = &forked.reason {
            out.push_str(&format!(
                " Its memory could not be loaded ({reason}), so it booted from the disk and processes were not kept."
            ));
        }
        out.push_str(&memory_note(&forked.credentials, "the fork's memory"));
        Ok(out)
    }

    fn destroy(&self, args: &Args<'_>) -> Outcome {
        let name = args.required("fork")?;
        self.sandbox(args)?.destroy().map_err(text)?;
        Ok(format!("Removed fork {name}."))
    }
}

/// A tool's arguments.
struct Args<'a>(&'a Value);

impl Args<'_> {
    fn optional(&self, key: &str) -> Result<Option<&str>, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("\"{key}\" must be a string")),
        }
    }

    fn strings(&self, key: &str) -> Result<Vec<&str>, String> {
        let invalid = || format!("\"{key}\" must be a list of strings");
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().ok_or_else(invalid))
                .collect(),
            Some(_) => Err(invalid()),
        }
    }

    fn required(&self, key: &str) -> Result<&str, String> {
        self.optional(key)?
            .ok_or_else(|| format!("\"{key}\" is required"))
    }
}

/// The note for a snapshot that recorded injected credentials, by name.
fn memory_note(credentials: &[String], holder: &str) -> String {
    if credentials.is_empty() {
        return String::new();
    }
    format!(
        " Warning: the snapshot was taken after {} had been injected, so {holder} may contain them.",
        credentials.join(", ")
    )
}

fn text(e: Error) -> String {
    e.to_string()
}

/// `path` as the VM sees it.
fn vm_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            cave_vm_sdk::WORKSPACE,
            path.trim_start_matches("./")
        )
    }
}

/// An `exec` result: how it ended, then each stream that printed anything.
fn exec_text(output: &Output) -> String {
    let mut out = match (&output.code, &output.signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "ended without an exit code".to_string(),
    };
    for (name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        if bytes.is_empty() {
            continue;
        }
        out.push_str(&format!("\n--- {name} ---\n"));
        out.push_str(&clip(&String::from_utf8_lossy(bytes)));
    }
    out
}

/// `text` cut to [`OUTPUT_LIMIT`] bytes, keeping its start and end.
fn clip(text: &str) -> String {
    if text.len() <= OUTPUT_LIMIT {
        return text.to_string();
    }
    let half = OUTPUT_LIMIT / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes cut ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_results_say_how_the_command_ended() {
        let output = Output {
            code: Some(2),
            signal: None,
            stdout: b"out\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(exec_text(&output), "exit code 2\n--- stdout ---\nout\n");
        let killed = Output {
            code: None,
            signal: Some("KILL".into()),
            stdout: Vec::new(),
            stderr: b"oops".to_vec(),
        };
        assert_eq!(
            exec_text(&killed),
            "killed by signal KILL\n--- stderr ---\noops"
        );
    }

    #[test]
    fn long_output_keeps_its_start_and_end() {
        let text = format!("{}{}", "a".repeat(2 * OUTPUT_LIMIT), "é".repeat(10));
        let clipped = clip(&text);
        assert!(clipped.len() < text.len());
        assert!(clipped.starts_with("aaa"));
        assert!(clipped.ends_with("éé"));
        assert!(clipped.contains("bytes cut"));
        assert_eq!(clip("short"), "short");
    }

    #[test]
    fn relative_paths_are_under_the_workspace() {
        assert_eq!(vm_path("src/a.rs"), "/workspace/src/a.rs");
        assert_eq!(vm_path("./a"), "/workspace/a");
        assert_eq!(vm_path("/etc/hosts"), "/etc/hosts");
    }
}
