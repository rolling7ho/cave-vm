//! PSCI, the firmware interface Linux uses to manage CPUs and power, as
//! trapped through `HVC`. Only what a single-vCPU spike needs is handled;
//! secondary CPUs are reported as not supported.

/// Function IDs from the PSCI specification (SMC32 and SMC64 forms).
pub mod function {
    pub const PSCI_VERSION: u32 = 0x8400_0000;
    pub const CPU_SUSPEND_32: u32 = 0x8400_0001;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON_32: u32 = 0x8400_0003;
    pub const AFFINITY_INFO_32: u32 = 0x8400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;
    pub const PSCI_FEATURES: u32 = 0x8400_000a;
    pub const CPU_SUSPEND_64: u32 = 0xc400_0001;
    pub const CPU_ON_64: u32 = 0xc400_0003;
    pub const AFFINITY_INFO_64: u32 = 0xc400_0004;
}

/// PSCI return codes.
pub mod ret {
    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMETERS: i64 = -2;
    pub const ALREADY_ON: i64 = -4;
}

/// PSCI 1.0, as `major << 16 | minor`.
pub const VERSION_1_0: i64 = 0x1_0000;
/// `MIGRATE_INFO_TYPE`: no trusted OS that needs migrating.
const MIGRATE_NOT_REQUIRED: i64 = 2;

/// What the VMM must do after a PSCI call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Write this value to `x0` and resume the guest.
    Return(i64),
    /// Stop the calling vCPU.
    CpuOff,
    /// Power the VM off.
    SystemOff,
    /// Reset the VM.
    SystemReset,
}

/// Handles a PSCI call. `args` are `x0` to `x3`. `cpus` is the number of
/// vCPUs, of which only vCPU 0 runs in the spike.
pub fn handle(args: [u64; 4], cpus: u32) -> Action {
    use function::*;
    let func = args[0] as u32;
    match func {
        PSCI_VERSION => Action::Return(VERSION_1_0),
        MIGRATE_INFO_TYPE => Action::Return(MIGRATE_NOT_REQUIRED),
        PSCI_FEATURES => Action::Return(match args[1] as u32 {
            PSCI_VERSION | CPU_OFF | CPU_ON_32 | CPU_ON_64 | AFFINITY_INFO_32
            | AFFINITY_INFO_64 | MIGRATE_INFO_TYPE | SYSTEM_OFF | SYSTEM_RESET | PSCI_FEATURES => {
                ret::SUCCESS
            }
            _ => ret::NOT_SUPPORTED,
        }),
        CPU_OFF => Action::CpuOff,
        CPU_ON_32 | CPU_ON_64 => Action::Return(match args[1] & 0xff_ffff {
            0 => ret::ALREADY_ON,
            target if target < u64::from(cpus) => ret::NOT_SUPPORTED,
            _ => ret::INVALID_PARAMETERS,
        }),
        AFFINITY_INFO_32 | AFFINITY_INFO_64 => Action::Return(match args[1] & 0xff_ffff {
            0 => 0,                        // ON
            t if t < u64::from(cpus) => 1, // OFF
            _ => ret::INVALID_PARAMETERS,
        }),
        SYSTEM_OFF => Action::SystemOff,
        SYSTEM_RESET => Action::SystemReset,
        _ => Action::Return(ret::NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::function::*;
    use super::*;

    fn call(func: u32, arg: u64) -> Action {
        handle([u64::from(func), arg, 0, 0], 2)
    }

    #[test]
    fn version_is_1_0() {
        assert_eq!(call(PSCI_VERSION, 0), Action::Return(0x1_0000));
    }

    #[test]
    fn features_reports_handled_calls() {
        assert_eq!(
            call(PSCI_FEATURES, u64::from(SYSTEM_OFF)),
            Action::Return(ret::SUCCESS)
        );
        assert_eq!(
            call(PSCI_FEATURES, u64::from(CPU_SUSPEND_64)),
            Action::Return(ret::NOT_SUPPORTED)
        );
    }

    #[test]
    fn power_calls() {
        assert_eq!(call(SYSTEM_OFF, 0), Action::SystemOff);
        assert_eq!(call(SYSTEM_RESET, 0), Action::SystemReset);
        assert_eq!(call(CPU_OFF, 0), Action::CpuOff);
    }

    #[test]
    fn cpu_on_for_boot_cpu_and_others() {
        assert_eq!(call(CPU_ON_64, 0), Action::Return(ret::ALREADY_ON));
        assert_eq!(call(CPU_ON_64, 1), Action::Return(ret::NOT_SUPPORTED));
        assert_eq!(call(CPU_ON_64, 9), Action::Return(ret::INVALID_PARAMETERS));
    }

    #[test]
    fn unknown_function_is_not_supported() {
        assert_eq!(call(0x8400_00ff, 0), Action::Return(ret::NOT_SUPPORTED));
    }
}
