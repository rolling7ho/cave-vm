//! Errors returned by Hypervisor.framework.

use std::fmt;

/// Raw return code of a Hypervisor.framework call (`hv_return_t`).
pub type RawReturn = i32;

// Values from `hv_error.h`: `err_local | err_sub(0xba5) | code`.
const HV_SUCCESS: u32 = 0;
const HV_ERROR: u32 = 0xfae9_4001;
const HV_BUSY: u32 = 0xfae9_4002;
const HV_BAD_ARGUMENT: u32 = 0xfae9_4003;
const HV_NO_RESOURCES: u32 = 0xfae9_4005;
const HV_NO_DEVICE: u32 = 0xfae9_4006;
const HV_DENIED: u32 = 0xfae9_4007;
const HV_FAULT: u32 = 0xfae9_4008;
const HV_UNSUPPORTED: u32 = 0xfae9_400f;

/// An error from Hypervisor.framework, or from this wrapper's own checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// `HV_ERROR`: unspecified failure.
    Failed,
    /// `HV_BUSY`: the resource is in use.
    Busy,
    /// `HV_BAD_ARGUMENT`: an argument was rejected.
    BadArgument,
    /// `HV_NO_RESOURCES`: out of memory or another host resource.
    NoResources,
    /// `HV_NO_DEVICE`: the host has no hypervisor support.
    NoDevice,
    /// `HV_DENIED`: the process lacks the hypervisor entitlement.
    Denied,
    /// `HV_FAULT`: a bad address was passed.
    Fault,
    /// `HV_UNSUPPORTED`: the operation is not supported on this host.
    Unsupported,
    /// A return code this wrapper does not know.
    Unknown(RawReturn),
    /// This process already has a VM. Hypervisor.framework allows one per
    /// process.
    VmExists,
}

/// Result of a wrapper call.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Converts a raw return code. `HV_SUCCESS` maps to `Ok(())`.
    pub fn check(code: RawReturn) -> Result<()> {
        Err(match code as u32 {
            HV_SUCCESS => return Ok(()),
            HV_ERROR => Error::Failed,
            HV_BUSY => Error::Busy,
            HV_BAD_ARGUMENT => Error::BadArgument,
            HV_NO_RESOURCES => Error::NoResources,
            HV_NO_DEVICE => Error::NoDevice,
            HV_DENIED => Error::Denied,
            HV_FAULT => Error::Fault,
            HV_UNSUPPORTED => Error::Unsupported,
            _ => Error::Unknown(code),
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Failed => f.write_str("Hypervisor.framework call failed"),
            Error::Busy => f.write_str("Hypervisor.framework resource is busy"),
            Error::BadArgument => f.write_str("Hypervisor.framework rejected an argument"),
            Error::NoResources => f.write_str("Hypervisor.framework is out of resources"),
            Error::NoDevice => f.write_str(
                "this Mac has no hypervisor support; cave needs Apple Silicon and macOS 26 or later",
            ),
            Error::Denied => f.write_str(
                "the hypervisor refused this process; sign the binary with the \
                 com.apple.security.hypervisor entitlement",
            ),
            Error::Fault => f.write_str("Hypervisor.framework was given a bad address"),
            Error::Unsupported => f.write_str("this Hypervisor.framework call is not supported here"),
            Error::Unknown(code) => {
                write!(f, "Hypervisor.framework returned unknown code {:#x}", *code as u32)
            }
            Error::VmExists => f.write_str("this process already has a VM"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_ok() {
        assert_eq!(Error::check(0), Ok(()));
    }

    #[test]
    fn known_codes_map_to_variants() {
        let cases = [
            (0xfae9_4001u32, Error::Failed),
            (0xfae9_4002, Error::Busy),
            (0xfae9_4003, Error::BadArgument),
            (0xfae9_4005, Error::NoResources),
            (0xfae9_4006, Error::NoDevice),
            (0xfae9_4007, Error::Denied),
            (0xfae9_4008, Error::Fault),
            (0xfae9_400f, Error::Unsupported),
        ];
        for (code, want) in cases {
            assert_eq!(Error::check(code as i32), Err(want), "{code:#x}");
        }
    }

    #[test]
    fn unknown_code_is_kept() {
        assert_eq!(Error::check(7), Err(Error::Unknown(7)));
    }

    #[test]
    fn denied_says_how_to_fix() {
        assert!(
            Error::Denied
                .to_string()
                .contains("com.apple.security.hypervisor")
        );
    }
}
