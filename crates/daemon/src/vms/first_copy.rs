//! The first-boot copy (D13, D33): on a project VM's first boot, the files
//! already in the project folder go into `/workspace` once, everything
//! except `.cave/` and the top-level `.env`. The project source sets a
//! marker in `.cave/` when it makes the disk, and it is cleared once the
//! copy is done, so a copy that failed is tried again on the next start.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use cave_vm_proto::agent::{Exec, Message, Stream};
use cave_vm_proto::daemon::VmId;
use cave_vm_proto::{ErrorCode, ErrorMessage, MAX_CHUNK, write_frame};

use super::{Vms, error, vm_state_dir};
use crate::project::{COPY_PENDING_FILE, STATE_DIR};
use crate::relay;

/// Top-level names in the project folder that are never copied.
const LEFT_OUT: [&str; 2] = [STATE_DIR, ".env"];

/// Unpacks the archive from stdin into `/workspace`, owned by the user
/// the agent runs commands as. macOS's tar may add headers GNU tar does
/// not know; they carry nothing the VM needs.
const UNPACK: [&str; 7] = [
    "tar",
    "-x",
    "-f",
    "-",
    "-C",
    "/workspace",
    "--warning=no-unknown-keyword",
];

fn failed(e: impl std::fmt::Display) -> ErrorMessage {
    error(
        ErrorCode::Io,
        format!("cannot copy the project's files into the VM: {e}"),
    )
    .with_fix("Run the command again to retry the copy, or copy the files in with `cave cp`")
}

/// The top-level names to copy, sorted.
fn entries(project: &Path) -> io::Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(project)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<_>>()?;
    names.retain(|n| !LEFT_OUT.contains(&n.as_str()));
    names.sort();
    Ok(names)
}

impl Vms {
    /// Does the copy if `id` is marked for it, then clears the mark.
    /// Returns whether it copied.
    pub fn copy_if_pending(&self, id: &VmId) -> Result<bool, ErrorMessage> {
        let marker = vm_state_dir(id).join(COPY_PENDING_FILE);
        if !marker.exists() {
            return Ok(false);
        }
        let project = Path::new(&id.project);
        let names = entries(project).map_err(failed)?;
        if !names.is_empty() {
            self.copy_in(id, project, &names)?;
        }
        fs::remove_file(&marker).map_err(failed)?;
        Ok(true)
    }

    /// Streams a tar archive of `names` in `project` into the VM.
    fn copy_in(&self, id: &VmId, project: &Path, names: &[String]) -> Result<(), ErrorMessage> {
        let mut tar = Command::new("/usr/bin/tar")
            .args([
                "-c",
                "-f",
                "-",
                "--no-mac-metadata",
                "--no-xattrs",
                "--no-acls",
            ])
            .arg("--no-fflags")
            .arg("--")
            .args(names)
            .current_dir(project)
            .env("COPYFILE_DISABLE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(failed)?;
        let (mut agent, _in_use) = self.agent_connection(id)?;
        let exec = Message::Exec(Exec {
            argv: UNPACK.map(String::from).to_vec(),
            stdin: true,
            ..Exec::default()
        });
        relay::open(&mut agent, &exec)?;
        let mut out = tar.stdout.take().expect("piped stdout");
        let mut input = agent.try_clone().map_err(failed)?;
        let writer = thread::spawn(move || -> io::Result<()> {
            let mut chunk = vec![0; MAX_CHUNK];
            loop {
                let n = out.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                let message = Message::Input {
                    data: chunk[..n].to_vec().into(),
                };
                write_frame(&mut input, &message).map_err(io::Error::other)?;
            }
            write_frame(&mut input, &Message::InputEnd).map_err(io::Error::other)
        });
        let mut stderr = Vec::new();
        let code = loop {
            match relay::read_agent(&mut agent)? {
                Message::Output {
                    stream: Stream::Stderr,
                    data,
                } => stderr.extend(data.0),
                Message::Output { .. } => {}
                Message::Exit { code, .. } => break code,
                Message::Error(e) => return Err(e),
                other => return Err(failed(format!("the agent sent {other:?}"))),
            }
        };
        // Unblocks the writer if the VM stopped reading early.
        let _ = agent.shutdown(std::net::Shutdown::Both);
        let packed = tar.wait_with_output().map_err(failed)?;
        let _ = writer.join();
        if !packed.status.success() {
            return Err(failed(String::from_utf8_lossy(&packed.stderr).trim()));
        }
        if code != Some(0) {
            return Err(failed(String::from_utf8_lossy(&stderr).trim()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_but_the_state_folder_and_env_is_copied() {
        let dir = std::env::temp_dir().join(format!("cave-first-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for name in [".cave", "src", ".git"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        for name in [".env", ".env.example", "README.md"] {
            fs::write(dir.join(name), "x").unwrap();
        }
        assert_eq!(
            entries(&dir).unwrap(),
            [".env.example", ".git", "README.md", "src"]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
