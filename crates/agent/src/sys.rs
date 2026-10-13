//! System calls std does not wrap (decision 0007, D24).
//!
//! The one module in `agent` allowed to use `unsafe`. Each call is wrapped
//! in a safe function, and every `unsafe` block has a `// SAFETY:` comment.

#![allow(unsafe_code)]

use std::io;

use cave_vm_proto::agent::Signal;

fn check(r: libc::c_int) -> io::Result<()> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Sends `signal` to process `pid`.
pub fn kill(pid: u32, signal: Signal) -> io::Result<()> {
    let sig = match signal {
        Signal::Int => libc::SIGINT,
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
        Signal::Hup => libc::SIGHUP,
    };
    let pid =
        libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: kill takes plain integers and has no memory effects.
    check(unsafe { libc::kill(pid, sig) })
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

/// Flushes every file system to disk, before a snapshot.
pub fn sync() {
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() }
}

/// Sets the system clock to `ns` nanoseconds since the Unix epoch, after a
/// restore. Needs root, which the agent has in the guest.
pub fn set_clock(ns: u64) -> io::Result<()> {
    let ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
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
