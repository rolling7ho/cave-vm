//! vCPU register identifiers.
//!
//! Values match `hv_vcpu_types.h` and `hv_gic_types.h` in the macOS SDK.

/// A general register (`hv_reg_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Reg(pub(crate) u32);

impl Reg {
    /// Frame pointer, `x29`.
    pub const FP: Reg = Reg(29);
    /// Link register, `x30`.
    pub const LR: Reg = Reg(30);
    /// Program counter.
    pub const PC: Reg = Reg(31);
    /// Floating-point control register.
    pub const FPCR: Reg = Reg(32);
    /// Floating-point status register.
    pub const FPSR: Reg = Reg(33);
    /// Processor state (`PSTATE`, as `CPSR`).
    pub const CPSR: Reg = Reg(34);

    /// General register `x<n>` for `n` in `0..=30`.
    pub const fn x(n: u8) -> Option<Reg> {
        if n <= 30 { Some(Reg(n as u32)) } else { None }
    }

    /// Every general register a snapshot saves, in order.
    pub fn all() -> impl Iterator<Item = Reg> {
        (0..=34).map(Reg)
    }

    /// The raw `hv_reg_t` value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Number of SIMD and floating-point registers, `q0` to `q31`.
pub const SIMD_FP_REGS: u8 = 32;

/// A system register (`hv_sys_reg_t`).
///
/// The value packs the `MRS`/`MSR` operands as
/// `op0 << 14 | op1 << 11 | CRn << 7 | CRm << 3 | op2`, which is also how a
/// trapped system register access is decoded (see [`crate::Trap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SysReg(pub(crate) u16);

impl SysReg {
    /// Builds the identifier from the instruction operands.
    pub const fn from_encoding(op0: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> SysReg {
        SysReg(
            ((op0 as u16 & 0b11) << 14)
                | ((op1 as u16 & 0b111) << 11)
                | ((crn as u16 & 0b1111) << 7)
                | ((crm as u16 & 0b1111) << 3)
                | (op2 as u16 & 0b111),
        )
    }

    /// The raw `hv_sys_reg_t` value.
    pub const fn raw(self) -> u16 {
        self.0
    }

    pub const MIDR_EL1: SysReg = SysReg(0xc000);
    pub const MPIDR_EL1: SysReg = SysReg(0xc005);
    pub const ID_AA64PFR0_EL1: SysReg = SysReg(0xc020);
    pub const ID_AA64MMFR0_EL1: SysReg = SysReg(0xc038);
    pub const SCTLR_EL1: SysReg = SysReg(0xc080);
    pub const CPACR_EL1: SysReg = SysReg(0xc082);
    pub const TTBR0_EL1: SysReg = SysReg(0xc100);
    pub const TTBR1_EL1: SysReg = SysReg(0xc101);
    pub const TCR_EL1: SysReg = SysReg(0xc102);
    pub const APIAKEYLO_EL1: SysReg = SysReg(0xc108);
    pub const APIAKEYHI_EL1: SysReg = SysReg(0xc109);
    pub const APIBKEYLO_EL1: SysReg = SysReg(0xc10a);
    pub const APIBKEYHI_EL1: SysReg = SysReg(0xc10b);
    pub const APDAKEYLO_EL1: SysReg = SysReg(0xc110);
    pub const APDAKEYHI_EL1: SysReg = SysReg(0xc111);
    pub const APDBKEYLO_EL1: SysReg = SysReg(0xc112);
    pub const APDBKEYHI_EL1: SysReg = SysReg(0xc113);
    pub const APGAKEYLO_EL1: SysReg = SysReg(0xc118);
    pub const APGAKEYHI_EL1: SysReg = SysReg(0xc119);
    pub const SPSR_EL1: SysReg = SysReg(0xc200);
    pub const ELR_EL1: SysReg = SysReg(0xc201);
    pub const SP_EL0: SysReg = SysReg(0xc208);
    pub const AFSR0_EL1: SysReg = SysReg(0xc288);
    pub const AFSR1_EL1: SysReg = SysReg(0xc289);
    pub const ESR_EL1: SysReg = SysReg(0xc290);
    pub const FAR_EL1: SysReg = SysReg(0xc300);
    pub const PAR_EL1: SysReg = SysReg(0xc3a0);
    pub const MAIR_EL1: SysReg = SysReg(0xc510);
    pub const AMAIR_EL1: SysReg = SysReg(0xc518);
    pub const VBAR_EL1: SysReg = SysReg(0xc600);
    pub const CONTEXTIDR_EL1: SysReg = SysReg(0xc681);
    pub const TPIDR_EL1: SysReg = SysReg(0xc684);
    pub const CNTKCTL_EL1: SysReg = SysReg(0xc708);
    pub const CSSELR_EL1: SysReg = SysReg(0xd000);
    pub const MDSCR_EL1: SysReg = SysReg(0x8012);
    pub const TPIDR_EL0: SysReg = SysReg(0xde82);
    pub const TPIDRRO_EL0: SysReg = SysReg(0xde83);
    pub const CNTV_CTL_EL0: SysReg = SysReg(0xdf19);
    pub const CNTV_CVAL_EL0: SysReg = SysReg(0xdf1a);
    /// Only available when the VM has a GIC.
    pub const CNTP_CTL_EL0: SysReg = SysReg(0xdf11);
    /// Only available when the VM has a GIC.
    pub const CNTP_CVAL_EL0: SysReg = SysReg(0xdf12);
    pub const SP_EL1: SysReg = SysReg(0xe208);
}

/// The EL1 and EL0 system registers that hold a running guest's state, with
/// their names. This is the set a snapshot saves and restores (P2.9). The
/// identification registers (`MIDR_EL1` and friends) are left out because
/// they describe the host, and the physical timer is left out because it
/// needs a GIC.
pub const GUEST_STATE_SYS_REGS: &[(&str, SysReg)] = &[
    ("MPIDR_EL1", SysReg::MPIDR_EL1),
    ("SCTLR_EL1", SysReg::SCTLR_EL1),
    ("CPACR_EL1", SysReg::CPACR_EL1),
    ("TTBR0_EL1", SysReg::TTBR0_EL1),
    ("TTBR1_EL1", SysReg::TTBR1_EL1),
    ("TCR_EL1", SysReg::TCR_EL1),
    ("APIAKEYLO_EL1", SysReg::APIAKEYLO_EL1),
    ("APIAKEYHI_EL1", SysReg::APIAKEYHI_EL1),
    ("APIBKEYLO_EL1", SysReg::APIBKEYLO_EL1),
    ("APIBKEYHI_EL1", SysReg::APIBKEYHI_EL1),
    ("APDAKEYLO_EL1", SysReg::APDAKEYLO_EL1),
    ("APDAKEYHI_EL1", SysReg::APDAKEYHI_EL1),
    ("APDBKEYLO_EL1", SysReg::APDBKEYLO_EL1),
    ("APDBKEYHI_EL1", SysReg::APDBKEYHI_EL1),
    ("APGAKEYLO_EL1", SysReg::APGAKEYLO_EL1),
    ("APGAKEYHI_EL1", SysReg::APGAKEYHI_EL1),
    ("SPSR_EL1", SysReg::SPSR_EL1),
    ("ELR_EL1", SysReg::ELR_EL1),
    ("SP_EL0", SysReg::SP_EL0),
    ("AFSR0_EL1", SysReg::AFSR0_EL1),
    ("AFSR1_EL1", SysReg::AFSR1_EL1),
    ("ESR_EL1", SysReg::ESR_EL1),
    ("FAR_EL1", SysReg::FAR_EL1),
    ("PAR_EL1", SysReg::PAR_EL1),
    ("MAIR_EL1", SysReg::MAIR_EL1),
    ("AMAIR_EL1", SysReg::AMAIR_EL1),
    ("VBAR_EL1", SysReg::VBAR_EL1),
    ("CONTEXTIDR_EL1", SysReg::CONTEXTIDR_EL1),
    ("TPIDR_EL1", SysReg::TPIDR_EL1),
    ("CNTKCTL_EL1", SysReg::CNTKCTL_EL1),
    ("CSSELR_EL1", SysReg::CSSELR_EL1),
    ("MDSCR_EL1", SysReg::MDSCR_EL1),
    ("TPIDR_EL0", SysReg::TPIDR_EL0),
    ("TPIDRRO_EL0", SysReg::TPIDRRO_EL0),
    ("CNTV_CTL_EL0", SysReg::CNTV_CTL_EL0),
    ("CNTV_CVAL_EL0", SysReg::CNTV_CVAL_EL0),
    ("SP_EL1", SysReg::SP_EL1),
];

/// A GIC CPU interface register (`hv_gic_icc_reg_t`). Part of the GIC state a
/// snapshot must save per vCPU, since `hv_gic_state_get_data` covers only the
/// distributor and redistributors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IccReg(pub(crate) u16);

impl IccReg {
    pub const PMR_EL1: IccReg = IccReg(0xc230);
    pub const BPR0_EL1: IccReg = IccReg(0xc643);
    pub const AP0R0_EL1: IccReg = IccReg(0xc644);
    pub const AP1R0_EL1: IccReg = IccReg(0xc648);
    pub const RPR_EL1: IccReg = IccReg(0xc65b);
    pub const BPR1_EL1: IccReg = IccReg(0xc663);
    pub const CTLR_EL1: IccReg = IccReg(0xc664);
    pub const SRE_EL1: IccReg = IccReg(0xc665);
    pub const IGRPEN0_EL1: IccReg = IccReg(0xc666);
    pub const IGRPEN1_EL1: IccReg = IccReg(0xc667);

    /// The raw `hv_gic_icc_reg_t` value.
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// A GIC redistributor register (`hv_gic_redistributor_reg_t`), the offset
/// from the GICv3 specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RedistReg(pub(crate) u32);

impl RedistReg {
    /// Set-pending for SGIs and PPIs 0 to 31, one bit each.
    pub const ISPENDR0: RedistReg = RedistReg(0x1_0200);
    /// Clear-pending for SGIs and PPIs 0 to 31, one bit each.
    pub const ICPENDR0: RedistReg = RedistReg(0x1_0280);
    /// Set-enable for SGIs and PPIs.
    pub const ISENABLER0: RedistReg = RedistReg(0x1_0100);

    /// The raw `hv_gic_redistributor_reg_t` value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// The GIC CPU interface registers a snapshot saves, with their names.
/// `RPR_EL1` is read-only and derived, so it is left out.
pub const GUEST_STATE_ICC_REGS: &[(&str, IccReg)] = &[
    ("PMR_EL1", IccReg::PMR_EL1),
    ("BPR0_EL1", IccReg::BPR0_EL1),
    ("AP0R0_EL1", IccReg::AP0R0_EL1),
    ("AP1R0_EL1", IccReg::AP1R0_EL1),
    ("BPR1_EL1", IccReg::BPR1_EL1),
    ("CTLR_EL1", IccReg::CTLR_EL1),
    ("SRE_EL1", IccReg::SRE_EL1),
    ("IGRPEN0_EL1", IccReg::IGRPEN0_EL1),
    ("IGRPEN1_EL1", IccReg::IGRPEN1_EL1),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_registers_are_bounded() {
        assert_eq!(Reg::x(0), Some(Reg(0)));
        assert_eq!(Reg::x(30), Some(Reg::LR));
        assert_eq!(Reg::x(31), None);
    }

    #[test]
    fn all_registers_end_with_cpsr() {
        let all: Vec<Reg> = Reg::all().collect();
        assert_eq!(all.len(), 35);
        assert_eq!(all.last(), Some(&Reg::CPSR));
    }

    #[test]
    fn sys_reg_encoding_matches_sdk_values() {
        // (op0, op1, CRn, CRm, op2) from the Arm ARM; values from hv_vcpu_types.h.
        let cases = [
            ((3, 0, 0, 0, 0), SysReg::MIDR_EL1),
            ((3, 0, 0, 0, 5), SysReg::MPIDR_EL1),
            ((3, 0, 1, 0, 0), SysReg::SCTLR_EL1),
            ((3, 0, 2, 0, 2), SysReg::TCR_EL1),
            ((3, 0, 4, 0, 1), SysReg::ELR_EL1),
            ((3, 0, 5, 2, 0), SysReg::ESR_EL1),
            ((3, 0, 10, 2, 0), SysReg::MAIR_EL1),
            ((3, 0, 12, 0, 0), SysReg::VBAR_EL1),
            ((3, 3, 13, 0, 2), SysReg::TPIDR_EL0),
            ((3, 3, 14, 3, 1), SysReg::CNTV_CTL_EL0),
            ((3, 3, 14, 3, 2), SysReg::CNTV_CVAL_EL0),
            ((3, 4, 4, 1, 0), SysReg::SP_EL1),
            ((2, 0, 0, 2, 2), SysReg::MDSCR_EL1),
        ];
        for ((op0, op1, crn, crm, op2), want) in cases {
            assert_eq!(
                SysReg::from_encoding(op0, op1, crn, crm, op2),
                want,
                "{want:?}"
            );
        }
    }

    #[test]
    fn guest_state_tables_have_no_duplicates() {
        let mut sys: Vec<u16> = GUEST_STATE_SYS_REGS.iter().map(|(_, r)| r.raw()).collect();
        sys.sort_unstable();
        sys.dedup();
        assert_eq!(sys.len(), GUEST_STATE_SYS_REGS.len());

        let mut icc: Vec<u16> = GUEST_STATE_ICC_REGS.iter().map(|(_, r)| r.raw()).collect();
        icc.sort_unstable();
        icc.dedup();
        assert_eq!(icc.len(), GUEST_STATE_ICC_REGS.len());
    }
}
