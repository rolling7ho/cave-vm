//! vCPU exit reasons and exception syndrome decoding.
//!
//! Pure logic: nothing here calls Hypervisor.framework, so it is tested on
//! any host.

use crate::regs::SysReg;

// `hv_exit_reason_t` values from hv_vcpu_types.h.
const REASON_CANCELED: u32 = 0;
const REASON_EXCEPTION: u32 = 1;
const REASON_VTIMER_ACTIVATED: u32 = 2;

/// Why `hv_vcpu_run` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Another thread asked the vCPU to stop (`hv_vcpus_exit`).
    Canceled,
    /// The guest trapped to the host.
    Exception(Exception),
    /// The guest's virtual timer fired. The framework masks the timer
    /// until the host unmasks it.
    VtimerActivated,
    /// A reason this wrapper does not know.
    Unknown(u32),
}

impl Exit {
    /// Builds an exit from the fields of `hv_vcpu_exit_t`.
    pub fn from_raw(
        reason: u32,
        syndrome: u64,
        virtual_address: u64,
        physical_address: u64,
    ) -> Exit {
        match reason {
            REASON_CANCELED => Exit::Canceled,
            REASON_EXCEPTION => Exit::Exception(Exception {
                syndrome,
                virtual_address,
                physical_address,
            }),
            REASON_VTIMER_ACTIVATED => Exit::VtimerActivated,
            other => Exit::Unknown(other),
        }
    }
}

/// A trapped exception, as reported in `hv_vcpu_exit_exception_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    /// The exception syndrome (`ESR_EL2`).
    pub syndrome: u64,
    /// The faulting virtual address (`FAR_EL2`), when there is one.
    pub virtual_address: u64,
    /// The faulting guest physical address (IPA), when there is one.
    pub physical_address: u64,
}

impl Exception {
    /// The exception class, `ESR.EC`.
    pub fn class(&self) -> u8 {
        ((self.syndrome >> 26) & 0x3f) as u8
    }

    /// The instruction-specific syndrome, `ESR.ISS`.
    pub fn iss(&self) -> u32 {
        (self.syndrome & 0x01ff_ffff) as u32
    }

    /// Whether the trapping instruction was 32 bits long (`ESR.IL`). When it
    /// was, the host advances `PC` by 4 after emulating it.
    pub fn is_32bit_instruction(&self) -> bool {
        self.syndrome & (1 << 25) != 0
    }

    /// Decodes the syndrome into what the guest was doing.
    pub fn decode(&self) -> Trap {
        let iss = self.iss();
        match self.class() {
            EC_WFX => Trap::WaitFor {
                event: iss & 1 != 0,
            },
            EC_HVC64 => Trap::Hvc { imm: iss as u16 },
            EC_SMC64 => Trap::Smc { imm: iss as u16 },
            EC_SYS64 => Trap::SysReg {
                reg: SysReg::from_encoding(
                    ((iss >> 20) & 0b11) as u8,
                    ((iss >> 14) & 0b111) as u8,
                    ((iss >> 10) & 0b1111) as u8,
                    ((iss >> 1) & 0b1111) as u8,
                    ((iss >> 17) & 0b111) as u8,
                ),
                rt: ((iss >> 5) & 0x1f) as u8,
                read: iss & 1 != 0,
            },
            EC_IABT_LOWER => Trap::InstructionAbort {
                ipa: self.physical_address,
            },
            EC_DABT_LOWER => Trap::DataAbort {
                ipa: self.physical_address,
                access: decode_data_access(iss),
            },
            EC_BRK64 => Trap::Breakpoint { imm: iss as u16 },
            class => Trap::Other { class, iss },
        }
    }
}

const EC_WFX: u8 = 0x01;
const EC_HVC64: u8 = 0x16;
const EC_SMC64: u8 = 0x17;
const EC_SYS64: u8 = 0x18;
const EC_IABT_LOWER: u8 = 0x20;
const EC_DABT_LOWER: u8 = 0x24;
const EC_BRK64: u8 = 0x3c;

/// What a trapped exception was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// `WFI` (or `WFE` when `event` is true).
    WaitFor { event: bool },
    /// `HVC #imm`. Linux uses this for PSCI calls. `PC` already points past
    /// the instruction (checked on hardware).
    Hvc { imm: u16 },
    /// `SMC #imm`. Per the Arm architecture, a trapped `SMC` leaves `PC` at
    /// the instruction (not yet checked on hardware).
    Smc { imm: u16 },
    /// An `MRS` (`read`) or `MSR` to a trapped system register. `rt` 31 is
    /// the zero register.
    SysReg { reg: SysReg, rt: u8, read: bool },
    /// The guest fetched an instruction from an unmapped guest physical
    /// address.
    InstructionAbort { ipa: u64 },
    /// The guest loaded or stored to an unmapped guest physical address,
    /// usually an MMIO device. `access` is `None` when the syndrome does not
    /// describe the access (`ISV` clear), and the host must then decode the
    /// instruction itself or fail.
    DataAbort {
        ipa: u64,
        access: Option<DataAccess>,
    },
    /// `BRK #imm`.
    Breakpoint { imm: u16 },
    /// Any other exception class.
    Other { class: u8, iss: u32 },
}

/// A single load or store described by a data abort syndrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAccess {
    /// Store (`true`) or load.
    pub write: bool,
    /// Access size in bytes: 1, 2, 4 or 8.
    pub size: u8,
    /// The general register transferred (`SRT`). 31 is the zero register.
    pub reg: u8,
    /// Whether a load sign-extends (`SSE`).
    pub sign_extend: bool,
    /// Whether the register is 64 bits wide (`SF`).
    pub wide_reg: bool,
}

fn decode_data_access(iss: u32) -> Option<DataAccess> {
    let isv = iss & (1 << 24) != 0;
    if !isv {
        return None;
    }
    Some(DataAccess {
        write: iss & (1 << 6) != 0,
        size: 1 << ((iss >> 22) & 0b11),
        reg: ((iss >> 16) & 0x1f) as u8,
        sign_extend: iss & (1 << 21) != 0,
        wide_reg: iss & (1 << 15) != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esr(class: u8, iss: u32) -> u64 {
        ((class as u64) << 26) | (1 << 25) | iss as u64
    }

    fn exception(class: u8, iss: u32, ipa: u64) -> Exception {
        Exception {
            syndrome: esr(class, iss),
            virtual_address: 0,
            physical_address: ipa,
        }
    }

    #[test]
    fn exit_reasons() {
        assert_eq!(Exit::from_raw(0, 0, 0, 0), Exit::Canceled);
        assert_eq!(Exit::from_raw(2, 0, 0, 0), Exit::VtimerActivated);
        assert_eq!(Exit::from_raw(9, 0, 0, 0), Exit::Unknown(9));
        match Exit::from_raw(1, esr(0x16, 0), 1, 2) {
            Exit::Exception(e) => {
                assert_eq!(e.virtual_address, 1);
                assert_eq!(e.physical_address, 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hvc_carries_immediate() {
        let e = exception(0x16, 0x1234, 0);
        assert_eq!(e.decode(), Trap::Hvc { imm: 0x1234 });
        assert!(e.is_32bit_instruction());
    }

    #[test]
    fn wfi_and_wfe() {
        assert_eq!(
            exception(0x01, 0, 0).decode(),
            Trap::WaitFor { event: false }
        );
        assert_eq!(
            exception(0x01, 1, 0).decode(),
            Trap::WaitFor { event: true }
        );
    }

    #[test]
    fn store_to_mmio() {
        // str x1, [x2]: ISV, SAS=3 (8 bytes), SRT=1, SF, WnR, DFSC translation fault level 1.
        let iss = (1 << 24) | (3 << 22) | (1 << 16) | (1 << 15) | (1 << 6) | 0b000101;
        let trap = exception(0x24, iss, 0x0900_0000).decode();
        assert_eq!(
            trap,
            Trap::DataAbort {
                ipa: 0x0900_0000,
                access: Some(DataAccess {
                    write: true,
                    size: 8,
                    reg: 1,
                    sign_extend: false,
                    wide_reg: true,
                }),
            }
        );
    }

    #[test]
    fn sign_extending_byte_load() {
        // ldrsb w3, [x2]: ISV, SAS=0, SSE, SRT=3, not SF, read.
        let iss = (1 << 24) | (1 << 21) | (3 << 16);
        let Trap::DataAbort {
            access: Some(a), ..
        } = exception(0x24, iss, 0).decode()
        else {
            panic!("expected a described data abort");
        };
        assert_eq!(
            (a.write, a.size, a.reg, a.sign_extend, a.wide_reg),
            (false, 1, 3, true, false)
        );
    }

    #[test]
    fn data_abort_without_syndrome() {
        assert_eq!(
            exception(0x24, 0b000101, 0x1000).decode(),
            Trap::DataAbort {
                ipa: 0x1000,
                access: None
            }
        );
    }

    #[test]
    fn sysreg_trap_decodes_register_and_direction() {
        // mrs x5, CNTV_CTL_EL0: op0=3 op2=1 op1=3 CRn=14 Rt=5 CRm=3 read.
        let iss = (3 << 20) | (1 << 17) | (3 << 14) | (14 << 10) | (5 << 5) | (3 << 1) | 1;
        assert_eq!(
            exception(0x18, iss, 0).decode(),
            Trap::SysReg {
                reg: SysReg::CNTV_CTL_EL0,
                rt: 5,
                read: true,
            }
        );
    }

    #[test]
    fn unknown_class_is_kept() {
        assert_eq!(
            exception(0x07, 0x42, 0).decode(),
            Trap::Other {
                class: 0x07,
                iss: 0x42
            }
        );
    }
}
