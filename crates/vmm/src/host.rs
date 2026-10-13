//! Host system calls std does not wrap (decision 0007, D24).
//!
//! One of the few modules in `vmm` allowed to use `unsafe`. Each call is
//! wrapped in a safe function, and every `unsafe` block has a `// SAFETY:`
//! comment.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`: clone a symlink itself rather
/// than its target. Not exported by the `libc` crate.
const CLONE_NOFOLLOW: u32 = 0x0001;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Makes `dst` an APFS clone of `src`: a new file that shares `src`'s
/// blocks until either is written. Fails with `ErrorKind::AlreadyExists` if
/// `dst` exists and `ErrorKind::Unsupported` if the volume cannot clone
/// (not APFS, or the two paths are on different volumes).
pub fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (c_path(src)?, c_path(dst)?);
    // SAFETY: both pointers are to NUL-terminated strings that live until
    // the call returns, and clonefile only reads them.
    let r = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => {
            Err(io::Error::new(io::ErrorKind::Unsupported, err))
        }
        _ => Err(err),
    }
}

/// Bytes available to an unprivileged user on the volume holding `path`.
pub fn available_space(path: &Path) -> io::Result<u64> {
    let path = c_path(path)?;
    // SAFETY: statfs is plain data, so all zeroes is a valid value.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and outlives the call, and `stat` is
    // a valid, writable statfs that the call fills in.
    let r = unsafe { libc::statfs(path.as_ptr(), &mut stat) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail.saturating_mul(u64::from(stat.f_bsize)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cave-host-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clone_has_the_same_contents_and_is_independent() {
        let dir = temp("clone");
        let (src, dst) = (dir.join("src"), dir.join("dst"));
        fs::write(&src, b"base image").unwrap();
        clonefile(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"base image");
        fs::write(&dst, b"changed").unwrap();
        assert_eq!(fs::read(&src).unwrap(), b"base image");
        let err = clonefile(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let missing = clonefile(&dir.join("nope"), &dir.join("x")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn available_space_is_reported() {
        let space = available_space(&std::env::temp_dir()).unwrap();
        assert!(space > 0);
        assert!(available_space(Path::new("/no/such/dir")).is_err());
    }
}
