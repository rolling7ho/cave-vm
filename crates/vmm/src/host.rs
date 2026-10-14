//! Host system calls std does not wrap (decision 0007, D24).
//!
//! One of the few modules in `vmm` allowed to use `unsafe`. Each call is
//! wrapped in a safe function, and every `unsafe` block has a `// SAFETY:`
//! comment.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
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
    clone_result(r)
}

/// Like [`clonefile`], from a file already open. The clone is of the file
/// the handle refers to, even if its path has since been deleted or
/// replaced.
pub fn clone_open_file(src: &File, dst: &Path) -> io::Result<()> {
    let dst = c_path(dst)?;
    // SAFETY: `src` is an open descriptor for the duration of the call, and
    // `dst` is a NUL-terminated string that outlives it and is only read.
    let r = unsafe { libc::fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, dst.as_ptr(), 0) };
    clone_result(r)
}

fn clone_result(r: libc::c_int) -> io::Result<()> {
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

/// Frees the blocks behind `len` bytes of `file` at `offset`, which then
/// read as zeroes; the file keeps its size. Both must be multiples of the
/// volume's block size (`F_PUNCHHOLE` refuses anything else with
/// `EINVAL`).
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let too_big = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "range is past the largest file offset",
        )
    };
    let args = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: libc::off_t::try_from(offset).map_err(|_| too_big())?,
        fp_length: libc::off_t::try_from(len).map_err(|_| too_big())?,
    };
    // SAFETY: the descriptor is open for the life of `file`, and `args` is
    // a valid fpunchhole_t that outlives the call, which only reads it.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &args) };
    if r == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The VMM's standard input as a Unix socket: the control link the daemon
/// starts it with (`docs/protocol.md`, "VMM control"). Fails if standard
/// input is not a socket.
pub fn stdin_socket() -> io::Result<UnixStream> {
    // SAFETY: dup only reads its argument; it returns a new descriptor or
    // -1.
    let fd = unsafe { libc::dup(0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor from dup that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut stat: libc::stat = unsafe_zeroed_stat();
    // SAFETY: `fd` is open and `stat` is a valid, writable stat.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "standard input is not a socket; cave-vmm is started by the cave daemon",
        ));
    }
    Ok(UnixStream::from(fd))
}

fn unsafe_zeroed_stat() -> libc::stat {
    // SAFETY: stat is plain data, so all zeroes is a valid value.
    unsafe { std::mem::zeroed() }
}

/// Room for one descriptor's control message.
const FD_SPACE: usize = 32;

/// Sends all of `data` on `socket`, with `fd` attached to its first byte
/// (`SCM_RIGHTS`).
pub fn send_with_fd(socket: &UnixStream, data: &[u8], fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as _) } as usize;
    assert!(space <= FD_SPACE, "control buffer too small");
    let mut control = [0u64; FD_SPACE / 8];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: msghdr is plain data, so all zeroes is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at `control`, which is aligned (u64) and holds
    // `space` bytes, so CMSG_FIRSTHDR returns a header inside it, and
    // CMSG_DATA has room for one c_int after it.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), fd.as_raw_fd());
    }
    // SAFETY: every pointer in `msg` is valid for the call: `iov` covers
    // `data`, and the control buffer is filled in above.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if sent == -1 {
        return Err(io::Error::last_os_error());
    }
    let sent = sent as usize;
    if sent < data.len() {
        // The descriptor went with the first part.
        io::Write::write_all(&mut &*socket, &data[sent..])?;
    }
    Ok(())
}

/// Reads up to `buf.len()` bytes from `socket`, and the descriptor attached
/// to them, if any. Descriptors arrive close-on-exec.
pub fn recv_with_fd(socket: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut control = [0u64; FD_SPACE / 8];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr is plain data, so all zeroes is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = FD_SPACE as _;
    let n = loop {
        // SAFETY: `iov` covers `buf` and the control buffer is FD_SPACE
        // writable bytes; recvmsg writes only within them.
        let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        if n != -1 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    let mut received = None;
    // SAFETY: recvmsg filled in `msg`; CMSG_FIRSTHDR and CMSG_NXTHDR walk
    // only the part of `control` it reported, and each SCM_RIGHTS message
    // carries c_int descriptors this process now owns.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let len = (*cmsg).cmsg_len as usize - (data as usize - cmsg as usize);
                for i in 0..len / size_of::<libc::c_int>() {
                    let fd = std::ptr::read_unaligned(data.cast::<libc::c_int>().add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    let fd = OwnedFd::from_raw_fd(fd);
                    // Keep the first; any others close when dropped.
                    if received.is_none() {
                        received = Some(fd);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 && received.is_none() {
        return Err(io::Error::other("a passed descriptor was lost"));
    }
    Ok((n, received))
}

/// Whether the peer closed `socket`, without reading anything or waiting.
/// Data waiting to be read means it has not.
pub fn hung_up(socket: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: recv writes at most one byte into `byte`, which outlives the
    // call; MSG_PEEK leaves it in the socket and MSG_DONTWAIT never blocks.
    let n = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}

/// This Mac's own IPv4 addresses, on every interface.
pub fn ipv4_addresses() -> io::Result<Vec<Ipv4Addr>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes a list pointer into `list` or fails.
    if unsafe { libc::getifaddrs(&mut list) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut addrs = Vec::new();
    let mut entry = list;
    // SAFETY: the list from getifaddrs is valid until freeifaddrs; each
    // ifa_addr is null or a sockaddr of the family it names, and AF_INET
    // ones are sockaddr_in.
    unsafe {
        while !entry.is_null() {
            let addr = (*entry).ifa_addr;
            if !addr.is_null() && i32::from((*addr).sa_family) == libc::AF_INET {
                let sin = addr.cast::<libc::sockaddr_in>();
                addrs.push(Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr)));
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    Ok(addrs)
}

/// Where a file's data sits on its volume: one `(offset, length)` per
/// contiguous run, in file order. Holes have none, and neither does data
/// not yet flushed. Two files sharing APFS
/// clone blocks report the same runs for them, so counting runs once
/// counts shared blocks once.
pub fn physical_extents(file: &File) -> io::Result<Vec<(u64, u64)>> {
    let fd = file.as_raw_fd();
    let seek = |offset: i64, whence: i32| -> io::Result<Option<i64>> {
        // SAFETY: lseek only moves the file offset of a descriptor `file`
        // keeps open.
        let r = unsafe { libc::lseek(fd, offset, whence) };
        if r >= 0 {
            return Ok(Some(r));
        }
        let e = io::Error::last_os_error();
        // ENXIO: no data at or after `offset`.
        if e.raw_os_error() == Some(libc::ENXIO) {
            Ok(None)
        } else {
            Err(e)
        }
    };
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut offset = 0;
    while let Some(data) = seek(offset, libc::SEEK_DATA)? {
        let hole = seek(data, libc::SEEK_HOLE)?.unwrap_or(data);
        let mut at = data;
        while at < hole {
            let mut map = libc::log2phys {
                l2p_flags: 0,
                l2p_contigbytes: hole - at,
                l2p_devoffset: at,
            };
            // SAFETY: F_LOG2PHYS_EXT reads the file offset and length from
            // `map` and writes the device offset and the contiguous length
            // back into it; `map` outlives the call.
            if unsafe { libc::fcntl(fd, libc::F_LOG2PHYS_EXT, &raw mut map) } == -1 {
                return Err(io::Error::last_os_error());
            }
            let (device, length) = (map.l2p_devoffset, map.l2p_contigbytes);
            if length <= 0 {
                break;
            }
            at = at.saturating_add(length);
            // Data written but not flushed yet has no place on the volume
            // (a negative offset), so it takes no blocks yet.
            let (Ok(device), Ok(length)) = (u64::try_from(device), u64::try_from(length)) else {
                continue;
            };
            match runs.last_mut() {
                Some((start, len)) if start.checked_add(*len) == Some(device) => *len += length,
                _ => runs.push((device, length)),
            }
        }
        offset = hole;
    }
    Ok(runs)
}

/// The IOKit and CoreFoundation calls [`on_system_sleep`] makes, from
/// `<IOKit/pwr_mgt/IOPMLib.h>` and `<CoreFoundation/CFRunLoop.h>`. The
/// `libc` crate does not cover them.
mod power {
    use std::ffi::c_void;

    pub type IoConnect = u32;
    pub type IoObject = u32;
    pub type NotificationPort = *mut c_void;
    pub type Callback =
        extern "C" fn(refcon: *mut c_void, service: IoObject, message: u32, argument: *mut c_void);

    /// `kIOMessageCanSystemSleep`: idle sleep is coming, and may be refused.
    pub const CAN_SYSTEM_SLEEP: u32 = 0xe000_0270;
    /// `kIOMessageSystemWillSleep`: sleep is coming and cannot be refused;
    /// it waits for the answer, up to 30 seconds.
    pub const SYSTEM_WILL_SLEEP: u32 = 0xe000_0280;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IORegisterForSystemPower(
            refcon: *mut c_void,
            port: *mut NotificationPort,
            callback: Callback,
            notifier: *mut IoObject,
        ) -> IoConnect;
        pub fn IONotificationPortGetRunLoopSource(port: NotificationPort) -> *mut c_void;
        pub fn IOAllowPowerChange(kernel_port: IoConnect, notification: isize) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFRunLoopDefaultMode: *const c_void;
        pub fn CFRunLoopGetCurrent() -> *mut c_void;
        pub fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
        pub fn CFRunLoopRun();
    }
}

/// What the power callback needs: the connection to answer on and what
/// to run before the Mac sleeps.
struct Sleeper {
    root_port: std::sync::atomic::AtomicU32,
    before_sleep: Box<dyn Fn() + Send + Sync>,
}

extern "C" fn power_changed(
    refcon: *mut std::ffi::c_void,
    _service: power::IoObject,
    message: u32,
    argument: *mut std::ffi::c_void,
) {
    // SAFETY: `refcon` is the leaked `Sleeper` that `on_system_sleep`
    // registered, which is never freed.
    let sleeper = unsafe { &*refcon.cast::<Sleeper>() };
    if message != power::CAN_SYSTEM_SLEEP && message != power::SYSTEM_WILL_SLEEP {
        return;
    }
    if message == power::SYSTEM_WILL_SLEEP {
        (sleeper.before_sleep)();
    }
    let port = sleeper.root_port.load(std::sync::atomic::Ordering::Acquire);
    // SAFETY: `port` is the connection IORegisterForSystemPower returned,
    // and `argument` is the notification ID IOKit passed with the message,
    // which IOAllowPowerChange takes as a long.
    unsafe { power::IOAllowPowerChange(port, argument as isize) };
}

/// Runs `before_sleep` each time the Mac is about to sleep, on a thread of
/// its own; sleep waits for it, for up to 30 seconds. Idle sleep is never
/// refused.
pub fn on_system_sleep(before_sleep: impl Fn() + Send + Sync + 'static) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    let sleeper: &'static Sleeper = Box::leak(Box::new(Sleeper {
        root_port: AtomicU32::new(0),
        before_sleep: Box::new(before_sleep),
    }));
    let (registered, failed) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("sleep-watch".into())
        .spawn(move || {
            let mut port: power::NotificationPort = std::ptr::null_mut();
            let mut notifier: power::IoObject = 0;
            let refcon = std::ptr::from_ref(sleeper).cast_mut().cast();
            // SAFETY: `refcon` stays valid forever (leaked), the callback
            // has the signature IOKit expects, and `port` and `notifier`
            // are writable locals the call fills in.
            let root = unsafe {
                power::IORegisterForSystemPower(refcon, &mut port, power_changed, &mut notifier)
            };
            if root == 0 {
                let _ = registered.send(Err(io::Error::other("IORegisterForSystemPower failed")));
                return;
            }
            sleeper.root_port.store(root, Ordering::Release);
            // SAFETY: `port` is the notification port the registration
            // made, and this thread's run loop runs its source for good;
            // the default mode is a CFString constant CoreFoundation owns.
            unsafe {
                let source = power::IONotificationPortGetRunLoopSource(port);
                power::CFRunLoopAddSource(
                    power::CFRunLoopGetCurrent(),
                    source,
                    power::kCFRunLoopDefaultMode,
                );
                let _ = registered.send(Ok(()));
                power::CFRunLoopRun();
            }
        })?;
    failed
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("the sleep watcher thread exited")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cave-host-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clones_share_physical_extents_until_written() {
        let dir = temp("extents");
        let (src, dst) = (dir.join("src"), dir.join("dst"));
        let mut data = vec![7u8; 4 << 20];
        fs::write(&src, &data).unwrap();
        File::open(&src).unwrap().sync_all().unwrap();
        clonefile(&src, &dst).unwrap();
        let extents = |p: &Path| physical_extents(&File::open(p).unwrap()).unwrap();
        let before = extents(&src);
        let total: u64 = before.iter().map(|r| r.1).sum();
        assert_eq!(total, 4 << 20, "{before:?}");
        assert_eq!(extents(&dst), before);
        // Rewriting the first megabyte of the clone gives it its own blocks
        // there and leaves the rest shared.
        data[..1 << 20].fill(9);
        let file = fs::OpenOptions::new().write(true).open(&dst).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, &data[..1 << 20], 0).unwrap();
        file.sync_all().unwrap();
        let after = extents(&dst);
        assert_ne!(after, before);
        let shared: u64 = after
            .iter()
            .filter(|run| before.iter().any(|b| b.0 <= run.0 && run.0 < b.0 + b.1))
            .map(|r| r.1)
            .sum();
        assert_eq!(shared, 3 << 20, "{before:?} {after:?}");
        // Data not flushed yet has no place, and is skipped.
        let unflushed = dir.join("unflushed");
        let mut file = File::create(&unflushed).unwrap();
        file.write_all(&[3u8; 1 << 20]).unwrap();
        let runs = physical_extents(&file).unwrap();
        assert!(runs.iter().map(|r| r.1).sum::<u64>() <= 1 << 20, "{runs:?}");
        // A sparse file has no runs for its holes.
        let sparse = dir.join("sparse");
        let file = File::create(&sparse).unwrap();
        file.set_len(1 << 30).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, &[1u8; 16384], 1 << 29).unwrap();
        file.sync_all().unwrap();
        let runs = extents(&sparse);
        assert_eq!(runs.iter().map(|r| r.1).sum::<u64>(), 16384, "{runs:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sleep_watcher_registers() {
        // The callback itself runs only when the Mac sleeps, so it is
        // checked by hand (docs/snapshots.md).
        on_system_sleep(|| {}).unwrap();
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
    fn an_open_file_clones_even_after_its_path_is_replaced() {
        let dir = temp("clone-open");
        let src = dir.join("src");
        fs::write(&src, b"first").unwrap();
        let open = File::open(&src).unwrap();
        fs::remove_file(&src).unwrap();
        fs::write(&src, b"second").unwrap();
        clone_open_file(&open, &dir.join("dst")).unwrap();
        assert_eq!(fs::read(dir.join("dst")).unwrap(), b"first");
        let err = clone_open_file(&open, &dir.join("dst")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn punched_range_reads_as_zeroes_and_frees_blocks() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp("punch");
        let path = dir.join("img");
        fs::write(&path, vec![0xffu8; 1 << 20]).unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let before = file.metadata().unwrap().blocks();
        punch_hole(&file, 64 << 10, 512 << 10).unwrap();
        let after = file.metadata().unwrap();
        assert_eq!(after.len(), 1 << 20, "size is unchanged");
        assert!(
            after.blocks() < before,
            "{} -> {} blocks",
            before,
            after.blocks()
        );
        let data = fs::read(&path).unwrap();
        assert!(data[..64 << 10].iter().all(|&b| b == 0xff));
        assert!(data[64 << 10..576 << 10].iter().all(|&b| b == 0));
        assert!(data[576 << 10..].iter().all(|&b| b == 0xff));
        let unaligned = punch_hole(&file, 512, 4096).unwrap_err();
        assert_eq!(unaligned.raw_os_error(), Some(libc::EINVAL));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_closed_peer_is_seen_without_reading() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(!hung_up(&a));
        (&b).write_all(b"x").unwrap();
        assert!(!hung_up(&a));
        drop(b);
        assert!(!hung_up(&a), "unread data comes first");
        let mut byte = [0u8; 1];
        (&a).read_exact(&mut byte).unwrap();
        assert!(hung_up(&a));
    }

    #[test]
    fn a_descriptor_crosses_with_its_data() {
        use std::io::{Read, Write};
        use std::os::fd::AsFd;
        let (a, b) = UnixStream::pair().unwrap();
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        send_with_fd(&a, b"frame", theirs.as_fd()).unwrap();
        drop(theirs);
        let mut buf = [0u8; 64];
        let (n, fd) = recv_with_fd(&b, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"frame");
        let mut passed = UnixStream::from(fd.expect("a descriptor"));
        passed.write_all(b"through").unwrap();
        let mut got = [0u8; 7];
        ours.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"through");
        // Plain data has no descriptor.
        (&a).write_all(b"plain").unwrap();
        let (n, fd) = recv_with_fd(&b, &mut buf).unwrap();
        assert_eq!((&buf[..n], fd.is_none()), (&b"plain"[..], true));
    }

    #[test]
    fn the_macs_addresses_include_loopback() {
        assert!(ipv4_addresses().unwrap().contains(&Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn available_space_is_reported() {
        let space = available_space(&std::env::temp_dir()).unwrap();
        assert!(space > 0);
        assert!(available_space(Path::new("/no/such/dir")).is_err());
    }
}
