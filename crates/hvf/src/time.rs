//! The host clock the guest's virtual counter is derived from.

use std::sync::OnceLock;
use std::time::Duration;

use crate::ffi;

/// Host monotonic time in ticks (`mach_absolute_time`). A guest reads
/// `CNTVCT_EL0` as this minus its vCPU's vtimer offset.
pub fn host_ticks() -> u64 {
    // SAFETY: no arguments, no side effects.
    unsafe { ffi::mach_absolute_time() }
}

/// Converts a number of host ticks to a duration.
pub fn ticks_to_duration(ticks: u64) -> Duration {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut info = ffi::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `info` is a valid, writable struct for the call.
        let ret = unsafe { ffi::mach_timebase_info(&mut info) };
        if ret != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer, info.denom)
        }
    });
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}
