//! PSCI, the firmware interface Linux uses to manage CPUs and power, as
//! trapped through `HVC`. [`handle`] decodes a call and checks its
//! arguments; the run loop applies CPU power changes, since those need the
//! state of every vCPU.

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
    pub const ON_PENDING: i64 = -5;
}

/// `AFFINITY_INFO` results.
pub mod affinity {
    pub const ON: i64 = 0;
    pub const OFF: i64 = 1;
    pub const ON_PENDING: i64 = 2;
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
    /// Start vCPU `target` at `entry` with `context` in `x0`, then return
    /// the result to the caller.
    CpuOn {
        target: u32,
        entry: u64,
        context: u64,
    },
    /// Return the power state of vCPU `target`.
    AffinityInfo { target: u32 },
    /// Stop the calling vCPU.
    CpuOff,
    /// Power the VM off.
    SystemOff,
    /// Reset the VM.
    SystemReset,
}

/// The vCPU index an MPIDR value names, the inverse of
/// [`fdt::mpidr`](crate::fdt::mpidr), if it is one of `cpus`.
pub fn vcpu_index(mpidr: u64, cpus: u32) -> Option<u32> {
    // Only affinity levels 0 and 1 are used; anything above must be zero.
    if mpidr & 0xff_00ff_0000 != 0 {
        return None;
    }
    let index = (mpidr & 0xff) as u32 | (((mpidr >> 8) & 0xff) as u32) << 8;
    (index < cpus).then_some(index)
}

/// Decodes a PSCI call. `args` are `x0` to `x3`. `cpus` is the number of
/// vCPUs.
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
        CPU_ON_32 | CPU_ON_64 => {
            let (entry, context) = if func == CPU_ON_32 {
                (args[2] & 0xffff_ffff, args[3] & 0xffff_ffff)
            } else {
                (args[2], args[3])
            };
            match vcpu_index(args[1], cpus) {
                Some(target) => Action::CpuOn {
                    target,
                    entry,
                    context,
                },
                None => Action::Return(ret::INVALID_PARAMETERS),
            }
        }
        AFFINITY_INFO_32 | AFFINITY_INFO_64 => {
            // Only the lowest affinity level (single vCPUs) is supported.
            match (vcpu_index(args[1], cpus), args[2]) {
                (Some(target), 0) => Action::AffinityInfo { target },
                _ => Action::Return(ret::INVALID_PARAMETERS),
            }
        }
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
    fn cpu_on_names_the_target_entry_and_context() {
        assert_eq!(
            handle([u64::from(CPU_ON_64), 0x8000_0001, 0x4008_0000, 7], 2),
            Action::CpuOn {
                target: 1,
                entry: 0x4008_0000,
                context: 7
            }
        );
        assert_eq!(call(CPU_ON_64, 9), Action::Return(ret::INVALID_PARAMETERS));
    }

    #[test]
    fn cpu_on_32_truncates_its_arguments() {
        assert_eq!(
            handle(
                [
                    u64::from(CPU_ON_32),
                    1,
                    0xffff_0000_4008_0000,
                    0x1_0000_0002
                ],
                2
            ),
            Action::CpuOn {
                target: 1,
                entry: 0x4008_0000,
                context: 2
            }
        );
    }

    #[test]
    fn affinity_info_checks_target_and_level() {
        assert_eq!(
            call(AFFINITY_INFO_64, 1),
            Action::AffinityInfo { target: 1 }
        );
        assert_eq!(
            call(AFFINITY_INFO_64, 2),
            Action::Return(ret::INVALID_PARAMETERS)
        );
        assert_eq!(
            handle([u64::from(AFFINITY_INFO_64), 0, 1, 0], 2),
            Action::Return(ret::INVALID_PARAMETERS),
            "higher affinity levels are not supported"
        );
    }

    #[test]
    fn mpidr_round_trips_to_index() {
        for index in [0, 1, 7, 0x102] {
            assert_eq!(vcpu_index(crate::fdt::mpidr(index), 0x200), Some(index));
        }
        assert_eq!(vcpu_index(0x8000_0002, 2), None, "out of range");
        assert_eq!(vcpu_index(0x1_0000, 2), None, "affinity level 2");
    }

    #[test]
    fn unknown_function_is_not_supported() {
        assert_eq!(call(0x8400_00ff, 0), Action::Return(ret::NOT_SUPPORTED));
    }
}
