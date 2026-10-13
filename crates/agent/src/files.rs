//! `file_read` and `file_write`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cave_vm_proto::agent::Message;
use cave_vm_proto::{Data, ErrorCode, MAX_CHUNK, read_frame};
use sha2::{Digest, Sha256};

use crate::exec::resolve;
use crate::session::{Config, kind};
use crate::stream::{Out, Stream};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn io_error(out: &Out, path: &Path, e: &io::Error) -> io::Result<()> {
    let code = match e.kind() {
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::Io,
    };
    let what = if code == ErrorCode::NotFound {
        "no such file in the VM".to_string()
    } else {
        e.to_string()
    };
    out.error(code, format!("{what}: {}", path.display()))
}

pub fn read(path: &str, out: &Out, config: &Config) -> io::Result<()> {
    let path = resolve(path, config);
    let mut file = match File::open(&path).and_then(|f| f.metadata().map(|m| (f, m))) {
        Ok((_, meta)) if meta.is_dir() => {
            return out.error(
                ErrorCode::BadRequest,
                format!(
                    "{} is a directory; copy directories with tar through exec",
                    path.display()
                ),
            );
        }
        Ok((file, meta)) => {
            out.send(&Message::FileInfo {
                size: meta.len(),
                mode: meta.permissions().mode() & 0o7777,
            })?;
            file
        }
        Err(e) => return io_error(out, &path, &e),
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; MAX_CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return io_error(out, &path, &e),
        };
        hasher.update(&buf[..n]);
        out.send(&Message::FileData {
            data: Data(buf[..n].to_vec()),
        })?;
    }
    out.send(&Message::FileEnd {
        sha256: hex(&hasher.finalize()),
    })
}

/// A temporary file, removed unless renamed into place.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn write(
    path: &str,
    mode: Option<u32>,
    create_dirs: bool,
    mut reader: Box<dyn Stream>,
    out: &Out,
    config: &Config,
) -> io::Result<()> {
    let path = resolve(path, config);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return out.error(
            ErrorCode::BadRequest,
            format!("not a file path: {}", path.display()),
        );
    };
    if create_dirs && let Err(e) = fs::create_dir_all(dir) {
        return io_error(out, dir, &e);
    }
    let temp = Temp(dir.join(format!(
        ".cave-tmp-{}-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
        name.to_string_lossy()
    )));
    let mut file = match File::create(&temp.0) {
        Ok(file) => file,
        Err(e) => return io_error(out, dir, &e),
    };
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let sent = loop {
        match read_frame::<_, Message>(&mut reader) {
            Ok(Some(Message::FileData { data })) => {
                hasher.update(&data.0);
                size += data.0.len() as u64;
                if let Err(e) = file.write_all(&data.0) {
                    drop(temp);
                    return io_error(out, &path, &e);
                }
            }
            Ok(Some(Message::FileEnd { sha256 })) => break sha256,
            Ok(Some(other)) => {
                drop(temp);
                return out.error(
                    ErrorCode::BadRequest,
                    format!("expected file_data or file_end, got {}", kind(&other)),
                );
            }
            // The client went away; the temporary file is removed.
            Ok(None) => return Ok(()),
            Err(e) => {
                drop(temp);
                return out.send(&Message::Error(e.to_error_message()));
            }
        }
    };
    let got = hex(&hasher.finalize());
    if !sent.eq_ignore_ascii_case(&got) {
        // Remove the temporary file before replying, so the client never
        // sees it once it has the answer.
        drop(temp);
        return out.error(
            ErrorCode::Io,
            format!(
                "{} arrived damaged (SHA-256 {got}, expected {sent}); the original is untouched",
                path.display()
            ),
        );
    }
    let mode = mode
        .or_else(|| {
            fs::metadata(&path)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777)
        })
        .unwrap_or(0o644);
    let finish = || -> io::Result<()> {
        file.sync_all()?;
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(mode))?;
        fs::rename(&temp.0, &path)
    };
    if let Err(e) = finish() {
        drop(temp);
        return io_error(out, &path, &e);
    }
    out.send(&Message::FileWritten { size })
}
