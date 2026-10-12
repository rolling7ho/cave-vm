//! Spike snapshots (P2.9): save a paused guest to a directory and restore it
//! in another process.
//!
//! A snapshot directory holds `state`, a small binary file with the vCPU,
//! interrupt controller and UART state, and `memory`, guest RAM as a sparse
//! file (all-zero pages are holes). Restore maps `memory` copy-on-write, so
//! it costs no copy and the file stays unchanged.
//!
//! The `state` format is a spike format: versioned so a mismatch is caught,
//! but not yet the stable format phase 4 needs.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::backend::{self, Backend, BackendVcpu, IccReg, RedistReg, Reg, SysReg};
use crate::devices::pl011::Pl011State;
use crate::layout;
use crate::memory::{self, FileMode, GuestMemory, PAGE_SIZE, Region};
use crate::runner::Power;

use cave_vm_hvf::{
    GUEST_STATE_ICC_REGS, GUEST_STATE_REDIST_REGS, GUEST_STATE_SYS_REGS, SIMD_FP_REGS,
};

const MAGIC: &[u8; 8] = b"CAVESNAP";
/// Version 2 adds each vCPU's PSCI power state and its redistributor's SGI
/// and PPI registers. Version 1 files are refused: without the
/// redistributor state, the restored guest never gets another timer
/// interrupt.
const VERSION: u32 = 2;
const STATE_FILE: &str = "state";
const MEMORY_FILE: &str = "memory";

/// One vCPU's architectural state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState {
    pub regs: Vec<(Reg, u64)>,
    pub simd: Vec<u128>,
    pub sys_regs: Vec<(SysReg, u64)>,
    pub icc_regs: Vec<(IccReg, u64)>,
    /// This vCPU's GIC redistributor SGI and PPI state.
    pub redist_regs: Vec<(RedistReg, u64)>,
    /// The guest's virtual counter when it was paused. Restore sets the
    /// timer offset so the counter continues from here.
    pub guest_counter: u64,
    pub vtimer_masked: bool,
    /// Whether the guest had the vCPU on, off, or about to start.
    pub power: Power,
}

/// Everything but guest memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub memory_size: u64,
    pub irqchip: Vec<u8>,
    pub uart: Pl011State,
    pub vcpus: Vec<VcpuState>,
}

/// An error saving or loading a snapshot.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Backend(backend::Error),
    Memory(memory::Error),
    /// The `state` file is not a snapshot this build can read.
    Format(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "snapshot file error: {e}"),
            Error::Backend(e) => e.fmt(f),
            Error::Memory(e) => e.fmt(f),
            Error::Format(why) => write!(f, "cannot read this snapshot: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<backend::Error> for Error {
    fn from(e: backend::Error) -> Error {
        Error::Backend(e)
    }
}

impl From<memory::Error> for Error {
    fn from(e: memory::Error) -> Error {
        Error::Memory(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Reads a paused vCPU's state. `now` is the backend's host clock. The
/// power state is set to on; the caller fills in the machine's.
pub fn save_vcpu<V: BackendVcpu>(vcpu: &V, now: u64) -> backend::Result<VcpuState> {
    let regs = Reg::all()
        .map(|r| Ok((r, vcpu.reg(r)?)))
        .collect::<backend::Result<_>>()?;
    let simd = (0..SIMD_FP_REGS)
        .map(|n| vcpu.simd_reg(n))
        .collect::<backend::Result<_>>()?;
    let sys_regs = GUEST_STATE_SYS_REGS
        .iter()
        .map(|&(_, r)| Ok((r, vcpu.sys_reg(r)?)))
        .collect::<backend::Result<_>>()?;
    let icc_regs = GUEST_STATE_ICC_REGS
        .iter()
        .map(|&(_, r)| Ok((r, vcpu.icc_reg(r)?)))
        .collect::<backend::Result<_>>()?;
    let redist_regs = GUEST_STATE_REDIST_REGS
        .iter()
        .map(|&(_, r)| Ok((r, vcpu.redist_reg(r)?)))
        .collect::<backend::Result<_>>()?;
    Ok(VcpuState {
        regs,
        simd,
        sys_regs,
        icc_regs,
        redist_regs,
        guest_counter: now.wrapping_sub(vcpu.vtimer_offset()?),
        vtimer_masked: vcpu.vtimer_masked()?,
        power: Power::On,
    })
}

/// Writes saved state into a new vCPU. The GIC state must already be
/// restored, since this overwrites the vCPU's redistributor. `now` is the backend's host clock.
pub fn restore_vcpu<V: BackendVcpu>(
    vcpu: &mut V,
    state: &VcpuState,
    now: u64,
) -> backend::Result<()> {
    for &(reg, value) in &state.sys_regs {
        vcpu.set_sys_reg(reg, value)?;
    }
    for &(reg, value) in &state.regs {
        vcpu.set_reg(reg, value)?;
    }
    for (n, &value) in state.simd.iter().enumerate() {
        vcpu.set_simd_reg(n as u8, value)?;
    }
    for &(reg, value) in &state.icc_regs {
        vcpu.set_icc_reg(reg, value)?;
    }
    for &(reg, value) in &state.redist_regs {
        // Bitwise registers are written as clear-all, then set.
        let clear = match reg {
            RedistReg::ISENABLER0 => Some(RedistReg::ICENABLER0),
            RedistReg::ISPENDR0 => Some(RedistReg::ICPENDR0),
            RedistReg::ISACTIVER0 => Some(RedistReg::ICACTIVER0),
            _ => None,
        };
        if let Some(clear) = clear {
            vcpu.set_redist_reg(clear, u64::from(u32::MAX))?;
        }
        vcpu.set_redist_reg(reg, value)?;
    }
    vcpu.set_vtimer_offset(now.wrapping_sub(state.guest_counter))?;
    vcpu.set_vtimer_masked(state.vtimer_masked)?;
    Ok(())
}

/// Writes `state` and guest memory into `dir`, which must exist.
pub fn save(dir: &Path, state: &State, memory: &GuestMemory) -> Result<()> {
    File::create(dir.join(STATE_FILE))?.write_all(&encode(state))?;
    let mut file = File::create(dir.join(MEMORY_FILE))?;
    file.set_len(state.memory_size)?;
    let mut page = vec![0u8; PAGE_SIZE as usize];
    let mut offset = 0;
    while offset < state.memory_size {
        memory.read(layout::RAM_BASE + offset, &mut page)?;
        if page.iter().any(|&b| b != 0) {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&page)?;
        }
        offset += PAGE_SIZE;
    }
    Ok(())
}

/// Reads the `state` file in `dir`.
pub fn load_state(dir: &Path) -> Result<State> {
    let mut bytes = Vec::new();
    File::open(dir.join(STATE_FILE))?.read_to_end(&mut bytes)?;
    decode(&bytes)
}

/// Maps the `memory` file in `dir` copy-on-write as guest RAM.
pub fn load_memory(dir: &Path, memory_size: u64) -> Result<GuestMemory> {
    let file = File::open(dir.join(MEMORY_FILE))?;
    let region = Region::from_file(&file, layout::RAM_BASE, memory_size, FileMode::CopyOnWrite)?;
    Ok(GuestMemory::new(vec![region])?)
}

/// Sets a new vCPU's `MPIDR_EL1` from saved state. The redistributors are
/// found by MPIDR, so every vCPU needs this before the GIC state is
/// restored. [`restore`] does it; a caller restoring vCPUs on their own
/// threads calls this, then [`Backend::set_irqchip_state`], then
/// [`restore_vcpu`].
pub fn restore_mpidr<V: BackendVcpu>(vcpu: &mut V, saved: &VcpuState) -> backend::Result<()> {
    if let Some(&(_, mpidr)) = saved.sys_regs.iter().find(|(r, _)| *r == SysReg::MPIDR_EL1) {
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, mpidr)?;
    }
    Ok(())
}

/// Restores interrupt controller and vCPU state into a backend whose
/// interrupt controller, memory and vCPUs already exist.
pub fn restore<B: Backend, V: BackendVcpu>(
    backend: &B,
    vcpus: &mut [V],
    state: &State,
) -> backend::Result<()> {
    for (vcpu, saved) in vcpus.iter_mut().zip(&state.vcpus) {
        restore_mpidr(vcpu, saved)?;
    }
    backend.set_irqchip_state(&state.irqchip)?;
    let now = backend.now_ticks();
    for (vcpu, saved) in vcpus.iter_mut().zip(&state.vcpus) {
        restore_vcpu(vcpu, saved, now)?;
    }
    Ok(())
}

fn encode(state: &State) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);
    put_u64(&mut out, state.memory_size);
    put_bytes(&mut out, &state.irqchip);
    for reg in state.uart.regs {
        put_u32(&mut out, reg);
    }
    put_bytes(&mut out, &state.uart.rx);
    put_u32(&mut out, state.vcpus.len() as u32);
    for vcpu in &state.vcpus {
        put_u32(&mut out, vcpu.regs.len() as u32);
        for &(reg, value) in &vcpu.regs {
            put_u32(&mut out, reg.raw());
            put_u64(&mut out, value);
        }
        put_u32(&mut out, vcpu.simd.len() as u32);
        for &value in &vcpu.simd {
            out.extend_from_slice(&value.to_le_bytes());
        }
        put_u32(&mut out, vcpu.sys_regs.len() as u32);
        for &(reg, value) in &vcpu.sys_regs {
            put_u32(&mut out, u32::from(reg.raw()));
            put_u64(&mut out, value);
        }
        put_u32(&mut out, vcpu.icc_regs.len() as u32);
        for &(reg, value) in &vcpu.icc_regs {
            put_u32(&mut out, u32::from(reg.raw()));
            put_u64(&mut out, value);
        }
        put_u32(&mut out, vcpu.redist_regs.len() as u32);
        for &(reg, value) in &vcpu.redist_regs {
            put_u32(&mut out, reg.raw());
            put_u64(&mut out, value);
        }
        put_u64(&mut out, vcpu.guest_counter);
        out.push(u8::from(vcpu.vtimer_masked));
        match vcpu.power {
            Power::Off => out.push(0),
            Power::On => out.push(1),
            Power::OnPending { entry, context } => {
                out.push(2);
                put_u64(&mut out, entry);
                put_u64(&mut out, context);
            }
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Result<State> {
    let mut r = Reader { bytes };
    if r.take(MAGIC.len())? != MAGIC {
        return Err(Error::Format("not a cave-vm snapshot".into()));
    }
    let version = r.u32()?;
    if version != VERSION {
        return Err(Error::Format(format!(
            "format version {version}, this build reads {VERSION}"
        )));
    }
    let memory_size = r.u64()?;
    let irqchip = r.bytes()?.to_vec();
    let mut uart = Pl011State::default();
    for reg in &mut uart.regs {
        *reg = r.u32()?;
    }
    uart.rx = r.bytes()?.to_vec();
    let mut vcpus = Vec::new();
    for _ in 0..r.u32()? {
        let regs = (0..r.u32()?)
            .map(|_| {
                let raw = r.u32()?;
                let reg = Reg::all()
                    .find(|g| g.raw() == raw)
                    .ok_or_else(|| unknown("register", raw))?;
                Ok((reg, r.u64()?))
            })
            .collect::<Result<_>>()?;
        let simd = (0..r.u32()?)
            .map(|_| {
                Ok(u128::from_le_bytes(
                    r.take(16)?.try_into().expect("16 bytes"),
                ))
            })
            .collect::<Result<_>>()?;
        let sys_regs = (0..r.u32()?)
            .map(|_| {
                let raw = r.u32()?;
                let &(_, reg) = GUEST_STATE_SYS_REGS
                    .iter()
                    .find(|(_, g)| u32::from(g.raw()) == raw)
                    .ok_or_else(|| unknown("system register", raw))?;
                Ok((reg, r.u64()?))
            })
            .collect::<Result<_>>()?;
        let icc_regs = (0..r.u32()?)
            .map(|_| {
                let raw = r.u32()?;
                let &(_, reg) = GUEST_STATE_ICC_REGS
                    .iter()
                    .find(|(_, g)| u32::from(g.raw()) == raw)
                    .ok_or_else(|| unknown("GIC CPU interface register", raw))?;
                Ok((reg, r.u64()?))
            })
            .collect::<Result<_>>()?;
        let redist_regs = (0..r.u32()?)
            .map(|_| {
                let raw = r.u32()?;
                let &(_, reg) = GUEST_STATE_REDIST_REGS
                    .iter()
                    .find(|(_, g)| g.raw() == raw)
                    .ok_or_else(|| unknown("GIC redistributor register", raw))?;
                Ok((reg, r.u64()?))
            })
            .collect::<Result<_>>()?;
        let guest_counter = r.u64()?;
        let vtimer_masked = r.take(1)?[0] != 0;
        let power = match r.take(1)?[0] {
            0 => Power::Off,
            1 => Power::On,
            2 => Power::OnPending {
                entry: r.u64()?,
                context: r.u64()?,
            },
            other => return Err(unknown("power state", u32::from(other))),
        };
        vcpus.push(VcpuState {
            regs,
            simd,
            sys_regs,
            icc_regs,
            redist_regs,
            guest_counter,
            vtimer_masked,
            power,
        });
    }
    if !r.bytes.is_empty() {
        return Err(Error::Format("unexpected bytes at the end".into()));
    }
    Ok(State {
        memory_size,
        irqchip,
        uart,
        vcpus,
    })
}

fn unknown(what: &str, raw: u32) -> Error {
    Error::Format(format!("unknown {what} {raw:#x}"))
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.bytes.len() < n {
            return Err(Error::Format("file is cut short".into()));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len =
            usize::try_from(self.u64()?).map_err(|_| Error::Format("length too large".into()))?;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        State {
            memory_size: 0x80_0000,
            irqchip: vec![1, 2, 3],
            uart: Pl011State {
                regs: [1, 2, 3, 4, 5, 6, 7, 8, 9],
                rx: b"ls\n".to_vec(),
            },
            vcpus: vec![VcpuState {
                regs: Reg::all().map(|r| (r, u64::from(r.raw()) * 3)).collect(),
                simd: (0..32).map(|n| u128::MAX - n).collect(),
                sys_regs: GUEST_STATE_SYS_REGS
                    .iter()
                    .map(|&(_, r)| (r, u64::from(r.raw())))
                    .collect(),
                icc_regs: GUEST_STATE_ICC_REGS
                    .iter()
                    .map(|&(_, r)| (r, 0xff))
                    .collect(),
                redist_regs: GUEST_STATE_REDIST_REGS
                    .iter()
                    .map(|&(_, r)| (r, u64::from(r.raw())))
                    .collect(),
                guest_counter: 123_456,
                vtimer_masked: true,
                power: Power::On,
            }],
        }
    }

    #[test]
    fn power_states_round_trip() {
        let mut state = sample();
        let first = state.vcpus[0].clone();
        state.vcpus = [
            Power::On,
            Power::Off,
            Power::OnPending {
                entry: 0x4008_0000,
                context: 9,
            },
        ]
        .into_iter()
        .map(|power| VcpuState {
            power,
            ..first.clone()
        })
        .collect();
        assert_eq!(decode(&encode(&state)).unwrap(), state);
    }

    #[test]
    fn version_1_is_refused() {
        let mut bytes = encode(&sample());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(err.contains("version 1"), "{err}");
    }

    #[test]
    fn state_round_trips() {
        let state = sample();
        assert_eq!(decode(&encode(&state)).unwrap(), state);
    }

    #[test]
    fn rejects_other_files_and_versions() {
        assert!(decode(b"hello").is_err());
        let mut bytes = encode(&sample());
        bytes[8] = 99;
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(err.contains("version 99"), "{err}");
    }

    #[test]
    fn rejects_truncated_and_trailing_bytes() {
        let bytes = encode(&sample());
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
    }
}
