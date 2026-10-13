//! System calls std does not wrap (decision 0007, D24).
//!
//! The one module in `agent` allowed to use `unsafe`. Each call is wrapped
//! in a safe function, and every `unsafe` block has a `// SAFETY:` comment.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

use cave_vm_proto::agent::Signal;

fn check(r: libc::c_int) -> io::Result<()> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn signal_number(signal: Signal) -> libc::c_int {
    match signal {
        Signal::Int => libc::SIGINT,
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
        Signal::Hup => libc::SIGHUP,
    }
}

fn pid_t(pid: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Sends `signal` to process `pid`.
pub fn kill(pid: u32, signal: Signal) -> io::Result<()> {
    let pid = pid_t(pid)?;
    // SAFETY: kill takes plain integers and has no memory effects.
    check(unsafe { libc::kill(pid, signal_number(signal)) })
}

/// Sends `signal` to every process in the group led by `pid`.
pub fn kill_group(pid: u32, signal: Signal) -> io::Result<()> {
    let pid = pid_t(pid)?;
    // SAFETY: as for kill; a negative pid names the process group.
    check(unsafe { libc::kill(-pid, signal_number(signal)) })
}

/// The real user ID of the agent.
pub fn uid() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Opens a pseudo-terminal of the given size: (controller, terminal).
pub fn open_pty(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut controller: libc::c_int = -1;
    let mut terminal: libc::c_int = -1;
    let mut size = winsize(cols, rows);
    // SAFETY: the two ints and the winsize are valid and writable for the
    // call; a null name and termios are allowed and mean "don't return the
    // name" and "default settings".
    check(unsafe {
        libc::openpty(
            &mut controller,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    })?;
    // SAFETY: openpty succeeded, so both are open descriptors that nothing
    // else owns.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(controller),
            OwnedFd::from_raw_fd(terminal),
        )
    })
}

/// Sets the window size of a pseudo-terminal; the kernel sends SIGWINCH to
/// its foreground process group.
pub fn set_window_size(pty: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
    let size = winsize(cols, rows);
    // SAFETY: TIOCSWINSZ reads one winsize, which `size` is and which
    // outlives the call; the descriptor is open for as long as the borrow.
    check(unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ as _, &size) })
}

/// Makes `command` start in a new session with its stdin, which must be a
/// pseudo-terminal, as the controlling terminal, as a login shell expects.
pub fn new_session_on_stdin(command: &mut Command) {
    // SAFETY: the closure runs in the child between fork and exec, and only
    // calls setsid and ioctl, which are async-signal-safe, and allocates
    // nothing.
    unsafe {
        command.pre_exec(|| {
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))
        });
    }
}

/// Makes `command` run as `uid` and `gid` with exactly `groups` as its
/// supplementary groups. Needs root. std's own `uid` would drop the
/// supplementary groups, and its other hooks run after it gives up root,
/// so all three calls happen here.
pub fn run_as(command: &mut Command, uid: u32, gid: u32, groups: Vec<u32>) {
    let groups: Vec<libc::gid_t> = groups.into_iter().map(|g| g as libc::gid_t).collect();
    // SAFETY: the closure runs in the child between fork and exec. It only
    // calls setgroups, setgid and setuid, which are async-signal-safe, and
    // allocates nothing: `groups` was built before the fork and is only
    // read.
    unsafe {
        command.pre_exec(move || {
            check(libc::setgroups(groups.len() as _, groups.as_ptr()))?;
            check(libc::setgid(gid as libc::gid_t))?;
            check(libc::setuid(uid as libc::uid_t))
        });
    }
}

/// Waits until child `pid` exits without reaping it, so its pid stays
/// reserved (and safe to signal) until the caller reaps it.
pub fn wait_exit_keep(pid: u32) -> io::Result<()> {
    let id = libc::id_t::from(pid);
    loop {
        // SAFETY: siginfo_t is plain data, so all zeroes is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable siginfo_t that waitid fills
        // in; the other arguments are plain integers.
        let r = unsafe { libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) };
        match check(r) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

#[cfg(target_os = "linux")]
pub use vsock::{Vsock, VsockListener};

#[cfg(target_os = "linux")]
mod vsock {
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::check;

    /// A connected vsock socket.
    pub struct Vsock(File);

    impl Vsock {
        pub fn try_clone(&self) -> io::Result<Vsock> {
            Ok(Vsock(self.0.try_clone()?))
        }

        pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
            let how = match how {
                Shutdown::Read => libc::SHUT_RD,
                Shutdown::Write => libc::SHUT_WR,
                Shutdown::Both => libc::SHUT_RDWR,
            };
            // SAFETY: shutdown takes an open descriptor, which `self` owns, and
            // an integer.
            check(unsafe { libc::shutdown(self.0.as_raw_fd(), how) })
        }
    }

    impl Read for Vsock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Vsock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A listening vsock socket.
    pub struct VsockListener(OwnedFd);

    impl VsockListener {
        /// Listens on `port` for connections from the host.
        pub fn bind(port: u32) -> io::Result<VsockListener> {
            // SAFETY: socket takes integers and returns a new descriptor or -1.
            let fd =
                unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            check(fd)?;
            // SAFETY: socket succeeded, so `fd` is open and owned by nobody else.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: sockaddr_vm is plain data, so all zeroes is valid.
            let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_port = port;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
            // SAFETY: `addr` is a valid sockaddr_vm of `len` bytes that bind only
            // reads.
            check(unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&raw const addr).cast::<libc::sockaddr>(),
                    len,
                )
            })?;
            // SAFETY: listen takes an open descriptor and an integer.
            check(unsafe { libc::listen(fd.as_raw_fd(), 64) })?;
            Ok(VsockListener(fd))
        }

        /// Waits for the next connection.
        pub fn accept(&self) -> io::Result<Vsock> {
            loop {
                // SAFETY: null address arguments are allowed and mean the peer
                // address is not returned.
                let fd = unsafe {
                    libc::accept4(
                        self.0.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                };
                match check(fd) {
                    // SAFETY: accept4 succeeded, so `fd` is a new open descriptor
                    // owned by nobody else.
                    Ok(()) => return Ok(Vsock(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// Flushes every file system to disk, before a snapshot.
pub fn sync() {
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() }
}

/// Sets the system clock to `ns` nanoseconds since the Unix epoch, after a
/// restore. Needs root, which the agent has in the guest.
pub fn set_clock(ns: u64) -> io::Result<()> {
    let ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as _,
    };
    // SAFETY: `ts` is a valid timespec that outlives the call, which only
    // reads it.
    check(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) })
}

/// The name of signal `sig` without the `SIG` prefix, such as `KILL`.
pub fn signal_name(sig: i32) -> String {
    let names = [
        (libc::SIGHUP, "HUP"),
        (libc::SIGINT, "INT"),
        (libc::SIGQUIT, "QUIT"),
        (libc::SIGILL, "ILL"),
        (libc::SIGTRAP, "TRAP"),
        (libc::SIGABRT, "ABRT"),
        (libc::SIGBUS, "BUS"),
        (libc::SIGFPE, "FPE"),
        (libc::SIGKILL, "KILL"),
        (libc::SIGUSR1, "USR1"),
        (libc::SIGSEGV, "SEGV"),
        (libc::SIGUSR2, "USR2"),
        (libc::SIGPIPE, "PIPE"),
        (libc::SIGALRM, "ALRM"),
        (libc::SIGTERM, "TERM"),
        (libc::SIGXCPU, "XCPU"),
        (libc::SIGXFSZ, "XFSZ"),
        (libc::SIGSYS, "SYS"),
    ];
    names
        .iter()
        .find(|(n, _)| *n == sig)
        .map_or_else(|| format!("SIG{sig}"), |(_, name)| (*name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(libc::SIGKILL), "KILL");
        assert_eq!(signal_name(libc::SIGSEGV), "SEGV");
        assert_eq!(signal_name(1000), "SIG1000");
    }
}
