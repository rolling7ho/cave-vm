//! `cave mirror`: the terminal of the agent `cave agent` is running for a
//! VM, shown live and read-only (D39). `cave agent` records what the agent
//! shows to `agent.term` in the VM's folder; this replays it, so the
//! screen builds up as it looks now, then follows it. `q` or Ctrl-C stops.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::thread;
use std::time::Duration;

use crate::agent::record_path;
use crate::commands::folder_name;
use crate::error::{CliError, Result};
use crate::project;
use crate::shell::{RawMode, stty};

/// How often the recording is checked for more.
const POLL: Duration = Duration::from_millis(50);

/// Leaves the agent's alternate screen, resets colours and shows the
/// cursor, so the terminal is usable again after mirroring a full-screen
/// agent.
const RESET: &str = "\x1b[?1049l\x1b[0m\x1b[?25h\r\n";

/// `cave mirror [--fork <name>]`. Runs until `q` or Ctrl-C.
pub fn run(fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let path = record_path(&project, fork.as_deref());
    let name = fork.clone().unwrap_or_else(|| folder_name(&project));
    let mut file = File::open(&path).map_err(|_| {
        CliError::new(format!("no agent has run in a terminal for {name} yet"))
            .fix("Start one with cave agent claude, then run cave mirror in another terminal")
    })?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliError::new("cave mirror needs a terminal")
            .fix("Use cave view for a plain feed of what happens in the VM"));
    }
    eprintln!("Showing the agent's terminal for {name}, read-only. Press q or Ctrl-C to stop");
    thread::sleep(Duration::from_millis(800));
    let raw = RawMode::enter()?;
    let saved = raw.saved.clone();
    // Keys only stop the mirror; nothing reaches the agent.
    thread::spawn(move || {
        let mut key = [0u8; 1];
        let mut stdin = io::stdin().lock();
        while let Ok(1) = stdin.read(&mut key) {
            if matches!(key[0], b'q' | b'Q' | 3) {
                let _ = stty(&[&saved]);
                print!("{RESET}");
                let _ = io::stdout().flush();
                std::process::exit(0);
            }
        }
    });
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            let mut out = io::stdout().lock();
            out.write_all(&buf[..n])?;
            out.flush()?;
            continue;
        }
        // A new run empties the file: start over with a clear screen.
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if file.stream_position()? > len {
            file.seek(SeekFrom::Start(0))?;
            print!("\x1b[H\x1b[2J");
        }
        thread::sleep(POLL);
    }
}
