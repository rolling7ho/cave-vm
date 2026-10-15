//! `cave cp`: copies files between the Mac and the VM (docs/cli.md,
//! "cave cp"). A path starting with `:` is in the VM, relative to the
//! project directory there unless absolute, as with `scp`.
//!
//! Files go through the agent's `file_read` and `file_write`, checked by
//! SHA-256 on both ends. Directories go through `tar` on both sides, the
//! VM's run with `exec`, as the protocol says.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use cave_vm_proto::MAX_CHUNK;
use cave_vm_proto::agent::Exec;
use cave_vm_proto::daemon::{Reply, Request, VmId};

use crate::client::Connection;
use crate::commands::{ensure_running, vm_id};
use crate::error::{CliError, EXIT_USAGE, Result};
use crate::exec::exit_code;
use crate::init;

/// The project directory inside the VM.
const VM_PROJECT_DIR: &str = "/workspace";

/// One side of a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Mac(PathBuf),
    /// An absolute path in the VM.
    Vm(String),
}

fn usage(message: impl Into<String>, fix: &str) -> CliError {
    CliError {
        code: EXIT_USAGE,
        ..CliError::new(message).fix(fix)
    }
}

fn place(arg: &str) -> Place {
    match arg.strip_prefix(':') {
        Some(path) if path.starts_with('/') => Place::Vm(path.to_string()),
        Some("") => Place::Vm(VM_PROJECT_DIR.to_string()),
        Some(path) => Place::Vm(format!("{VM_PROJECT_DIR}/{path}")),
        None => Place::Mac(PathBuf::from(arg)),
    }
}

/// The last component of a VM path, ignoring a trailing `/`.
fn vm_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// The folder of a VM path, ignoring a trailing `/`.
fn vm_parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &trimmed[..i],
    }
}

fn vm_join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// `cave cp`.
pub fn run(paths: Vec<String>) -> Result<()> {
    let (dest, srcs) = match paths.split_last() {
        Some((dest, srcs)) if !srcs.is_empty() => (place(dest), srcs.iter().map(|s| place(s))),
        _ => {
            return Err(usage(
                "cave cp needs a source and a destination",
                "cave cp ./fixtures :test/fixtures",
            ));
        }
    };
    let srcs: Vec<Place> = srcs.collect();
    let to_vm = matches!(dest, Place::Vm(_));
    if srcs.iter().any(|s| matches!(s, Place::Vm(_)) == to_vm) {
        return Err(if to_vm {
            usage(
                "cave cp copies between the Mac and the VM, not within the VM",
                "Use `cave exec cp -r <src> <dest>`",
            )
        } else {
            usage(
                "one side of cave cp must be in the VM",
                "Prefix the VM path with `:`, for example `:src/app.ts`",
            )
        });
    }
    let project = init::project_or_set_up()?;
    let vm = vm_id(&project, None);
    ensure_running(&vm, None, Vec::new(), false)?;
    let several = srcs.len() > 1;
    match dest {
        Place::Vm(dest) => {
            let into = dest.ends_with('/') || several || is_vm_dir(&vm, &dest)?;
            if several && !into {
                return Err(not_a_folder(&format!(":{dest}")));
            }
            for src in srcs {
                let Place::Mac(src) = src else { unreachable!() };
                to_vm_one(&vm, &src, &dest, into)?;
            }
        }
        Place::Mac(dest) => {
            let into = dest.to_string_lossy().ends_with('/') || several || dest.is_dir();
            if several && !into {
                return Err(not_a_folder(&dest.display().to_string()));
            }
            for src in srcs {
                let Place::Vm(src) = src else { unreachable!() };
                from_vm_one(&vm, &src, &dest, into)?;
            }
        }
    }
    Ok(())
}

fn not_a_folder(dest: &str) -> CliError {
    CliError::new(format!("{dest} is not a folder"))
        .fix("Copy several sources into an existing folder, or end the destination with /")
}

fn to_vm_one(vm: &VmId, src: &Path, dest: &str, into: bool) -> Result<()> {
    let meta = fs::metadata(src).map_err(|e| {
        CliError::new(format!("cannot read {}: {e}", src.display()))
            .fix("Check the path on your Mac")
    })?;
    let name = src
        .canonicalize()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| CliError::new(format!("{} has no name to copy", src.display())))?;
    if meta.is_dir() {
        // Into `dest`, or as `dest` itself when it does not exist yet.
        let (target, strip) = (dest, !into);
        vm_ok(vm, &["mkdir", "-p", target], None, &mut io::sink())?;
        let parent = src
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut tar = local_tar_create(&parent, &name)?;
        let stdout = tar.stdout.take().expect("piped");
        let mut argv = vec!["tar", "-x", "-f", "-", "-C", target];
        if strip {
            argv.push("--strip-components=1");
        }
        let result = vm_ok(vm, &argv, Some(Box::new(stdout)), &mut io::sink());
        let status = tar.wait()?;
        result?;
        if !status.success() {
            return Err(
                CliError::new(format!("tar could not read {}", src.display()))
                    .fix("Check that you can read every file in it, then try again"),
            );
        }
        return Ok(());
    }
    let target = if into {
        vm_join(dest, &name)
    } else {
        dest.to_string()
    };
    write_file(vm, src, &target, meta.permissions().mode() & 0o777)
}

/// Copies the contents of folder `src` in the VM into `dest` on the Mac,
/// creating it, for `cave export`.
pub fn from_vm_folder(vm: &VmId, src: &str, dest: &Path) -> Result<()> {
    if !is_vm_dir(vm, src)? {
        return Err(CliError::new(format!("{src} is not a folder in the VM"))
            .fix("Name a folder in the VM, such as /workspace, or copy one file with `cave cp`"));
    }
    from_vm_one(vm, src, dest, false)
}

fn from_vm_one(vm: &VmId, src: &str, dest: &Path, into: bool) -> Result<()> {
    let name = vm_name(src);
    if is_vm_dir(vm, src)? {
        let (target, strip) = (dest, !into);
        fs::create_dir_all(target)?;
        let mut tar = local_tar_extract(target, strip)?;
        let mut stdin = tar.stdin.take().expect("piped");
        let result = vm_ok(
            vm,
            &["tar", "-c", "-f", "-", "-C", vm_parent(src), name],
            None,
            &mut stdin,
        );
        drop(stdin);
        let status = tar.wait()?;
        result?;
        if !status.success() {
            return Err(
                CliError::new(format!("tar could not write into {}", target.display()))
                    .fix("Check that you can write there and the disk has space, then try again"),
            );
        }
        return Ok(());
    }
    let target = if into {
        dest.join(name)
    } else {
        dest.to_path_buf()
    };
    read_file(vm, src, &target)
}

fn local_tar_create(parent: &Path, name: &str) -> Result<Child> {
    Ok(Command::new("/usr/bin/tar")
        // No AppleDouble `._` files or macOS attributes in the VM.
        .env("COPYFILE_DISABLE", "1")
        .args(["--no-mac-metadata", "--no-xattrs", "-c", "-f", "-", "-C"])
        .arg(parent)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?)
}

fn local_tar_extract(target: &Path, strip: bool) -> Result<Child> {
    let mut tar = Command::new("/usr/bin/tar");
    tar.args(["-x", "-f", "-", "-C"]).arg(target);
    if strip {
        tar.args(["--strip-components", "1"]);
    }
    Ok(tar.stdin(Stdio::piped()).spawn()?)
}

/// Whether `path` is a folder in the VM.
fn is_vm_dir(vm: &VmId, path: &str) -> Result<bool> {
    let (code, _) = exec_in_vm(vm, &["test", "-d", path], None, &mut io::sink())?;
    Ok(code == 0)
}

/// Runs `argv` in the VM and fails with its stderr unless it exits 0.
fn vm_ok(
    vm: &VmId,
    argv: &[&str],
    input: Option<Box<dyn Read + Send>>,
    out: &mut dyn Write,
) -> Result<()> {
    let (code, stderr) = exec_in_vm(vm, argv, input, out)?;
    if code != 0 {
        let why = stderr.trim();
        return Err(CliError::new(if why.is_empty() {
            format!("{} failed in the VM with exit code {code}", argv[0])
        } else {
            format!("{} failed in the VM: {why}", argv[0])
        })
        .fix("Check the path and its permissions in the VM, for example with `cave exec -- ls -la /workspace`"));
    }
    Ok(())
}

/// Runs `argv` in the VM with `input` as stdin, writes its stdout to
/// `out`, and returns its exit code and stderr.
fn exec_in_vm(
    vm: &VmId,
    argv: &[&str],
    input: Option<Box<dyn Read + Send>>,
    out: &mut dyn Write,
) -> Result<(u8, String)> {
    let mut connection = Connection::open()?;
    connection.send(&Request::Exec {
        vm: vm.clone(),
        exec: Exec {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            stdin: input.is_some(),
            ..Exec::default()
        },
        credentials: Vec::new(),
    })?;
    if let Some(mut input) = input {
        let mut sender = connection.sender()?;
        thread::spawn(move || {
            let mut buf = vec![0u8; MAX_CHUNK];
            loop {
                match input.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec().into();
                        if sender.send(&Request::Input { data }).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = sender.send(&Request::InputEnd);
        });
    }
    let mut stderr = Vec::new();
    loop {
        match connection.receive()? {
            Reply::Output { stream, data } => match stream {
                cave_vm_proto::agent::Stream::Stdout => out.write_all(&data.0)?,
                cave_vm_proto::agent::Stream::Stderr => stderr.extend_from_slice(&data.0),
            },
            Reply::Exit { code, signal } => {
                let stderr = String::from_utf8_lossy(&stderr).into_owned();
                return Ok((exit_code(code, signal.as_deref()), stderr));
            }
            Reply::Error(e) => return Err(e.into()),
            Reply::Progress { .. } => {}
            other => {
                return Err(CliError::unexpected(&format!("exec: {other:?}")));
            }
        }
    }
}

/// The SHA-256 of a file on the Mac, from `shasum`, as lowercase hex.
fn sha256(path: &Path) -> Result<String> {
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next() {
        Some(hex) if out.status.success() && hex.len() == 64 => Ok(hex.to_string()),
        _ => Err(CliError::new(format!("cannot hash {}", path.display()))),
    }
}

fn write_file(vm: &VmId, src: &Path, target: &str, mode: u32) -> Result<()> {
    let digest = sha256(src)?;
    let mut file = File::open(src)?;
    let mut connection = Connection::open()?;
    connection.send(&Request::FileWrite {
        vm: vm.clone(),
        path: target.to_string(),
        mode: Some(mode),
        create_dirs: true,
    })?;
    let mut buf = vec![0u8; MAX_CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        connection.send(&Request::FileData {
            data: buf[..n].to_vec().into(),
        })?;
    }
    connection.send(&Request::FileEnd { sha256: digest })?;
    match connection.receive()? {
        Reply::FileWritten { .. } => Ok(()),
        Reply::Error(e) => Err(e.into()),
        other => Err(CliError::unexpected(&format!("file_write: {other:?}"))),
    }
}

/// A file being received, deleted unless it is renamed into place.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn read_file(vm: &VmId, src: &str, target: &Path) -> Result<()> {
    let mut connection = Connection::open()?;
    connection.send(&Request::FileRead {
        vm: vm.clone(),
        path: src.to_string(),
    })?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let partial = Partial(target.with_file_name(format!(".{name}.cave-partial")));
    let mut file: Option<File> = None;
    let mut mode = 0o644;
    loop {
        match connection.receive()? {
            Reply::FileInfo { mode: m, .. } => {
                mode = m & 0o777;
                file = Some(File::create(&partial.0)?);
            }
            Reply::FileData { data } => match file.as_mut() {
                Some(file) => file.write_all(&data.0)?,
                None => return Err(CliError::new("file data came before file_info")),
            },
            Reply::FileEnd { sha256: expected } => {
                let Some(file) = file.take() else {
                    return Err(CliError::new("file_end came before file_info"));
                };
                file.sync_all()?;
                drop(file);
                if sha256(&partial.0)? != expected {
                    return Err(CliError::new(format!(
                        "the copy of :{src} does not match the VM's file; nothing was kept"
                    ))
                    .fix("Try again"));
                }
                fs::set_permissions(&partial.0, fs::Permissions::from_mode(mode))?;
                fs::rename(&partial.0, target)?;
                return Ok(());
            }
            Reply::Error(e) => {
                let fix = "Check the path with `cave exec ls <dir>`";
                return Err(CliError::from(e).fix(fix));
            }
            Reply::Progress { .. } => {}
            other => {
                return Err(CliError::unexpected(&format!("file_read: {other:?}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colon_paths_are_in_the_vm_relative_to_the_project() {
        assert_eq!(place(":src/a"), Place::Vm("/workspace/src/a".into()));
        assert_eq!(place(":/etc/hosts"), Place::Vm("/etc/hosts".into()));
        assert_eq!(place(":"), Place::Vm("/workspace".into()));
        assert_eq!(place("./a"), Place::Mac("./a".into()));
    }

    #[test]
    fn vm_paths_split_into_folder_and_name() {
        assert_eq!(vm_name("/workspace/dist/"), "dist");
        assert_eq!(vm_parent("/workspace/dist/"), "/workspace");
        assert_eq!(vm_parent("/etc"), "/");
        assert_eq!(vm_join("/workspace/", "a"), "/workspace/a");
    }
}
