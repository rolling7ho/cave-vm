//! An Arm PL011 UART, enough for the Linux `amba-pl011` driver and
//! `earlycon=pl011`.
//!
//! Transmit is immediate (the FIFO is always empty), receive buffers input
//! bytes the host pushes. The interrupt line is level-triggered and follows
//! `RIS & IMSC`.

use std::collections::VecDeque;
use std::io::Write;

// Register offsets.
const DR: u64 = 0x000;
const RSR: u64 = 0x004;
const FR: u64 = 0x018;
const ILPR: u64 = 0x020;
const IBRD: u64 = 0x024;
const FBRD: u64 = 0x028;
const LCR_H: u64 = 0x02c;
const CR: u64 = 0x030;
const IFLS: u64 = 0x034;
const IMSC: u64 = 0x038;
const RIS: u64 = 0x03c;
const MIS: u64 = 0x040;
const ICR: u64 = 0x044;
const DMACR: u64 = 0x048;
const ID_BASE: u64 = 0xfe0;

// Flag register bits.
const FR_RXFE: u32 = 1 << 4;
const FR_TXFE: u32 = 1 << 7;

// Interrupt bits.
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;

/// PeriphID0-3 then PCellID0-3: a PL011 r1p5.
const ID: [u32; 8] = [0x11, 0x10, 0x34, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

/// The UART's registers and waiting input, for a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pl011State {
    /// `ILPR`, `IBRD`, `FBRD`, `LCR_H`, `CR`, `IFLS`, `IMSC`, `RIS` and
    /// `DMACR`, in that order.
    pub regs: [u32; 9],
    pub rx: Vec<u8>,
}

/// The UART. `out` receives what the guest transmits.
pub struct Pl011<W: Write> {
    out: W,
    rx: VecDeque<u8>,
    ilpr: u32,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    ris: u32,
    dmacr: u32,
}

impl<W: Write> Pl011<W> {
    pub fn new(out: W) -> Pl011<W> {
        Pl011 {
            out,
            rx: VecDeque::new(),
            ilpr: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            // UARTEN off, TXE and RXE on: the reset value.
            cr: 0x300,
            ifls: 0x12,
            imsc: 0,
            // The transmit FIFO starts empty.
            ris: INT_TX,
            dmacr: 0,
        }
    }

    /// Saves the registers and waiting input.
    pub fn state(&self) -> Pl011State {
        Pl011State {
            regs: [
                self.ilpr, self.ibrd, self.fbrd, self.lcr_h, self.cr, self.ifls, self.imsc,
                self.ris, self.dmacr,
            ],
            rx: self.rx.iter().copied().collect(),
        }
    }

    /// Restores what [`state`](Self::state) saved.
    pub fn set_state(&mut self, state: &Pl011State) {
        [
            self.ilpr, self.ibrd, self.fbrd, self.lcr_h, self.cr, self.ifls, self.imsc, self.ris,
            self.dmacr,
        ] = state.regs;
        self.rx = state.rx.iter().copied().collect();
    }

    /// Queues bytes typed on the host for the guest to read.
    pub fn push_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.rx.extend(bytes);
        self.ris |= INT_RX | INT_RT;
    }

    /// Whether the interrupt line is high.
    pub fn irq_level(&self) -> bool {
        self.ris & self.imsc != 0
    }

    /// Handles a guest read of the register at `offset`.
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            DR => {
                let byte = self.rx.pop_front().unwrap_or(0);
                if self.rx.is_empty() {
                    self.ris &= !(INT_RX | INT_RT);
                }
                u32::from(byte)
            }
            RSR => 0,
            FR => {
                let mut fr = FR_TXFE;
                if self.rx.is_empty() {
                    fr |= FR_RXFE;
                }
                fr
            }
            ILPR => self.ilpr,
            IBRD => self.ibrd,
            FBRD => self.fbrd,
            LCR_H => self.lcr_h,
            CR => self.cr,
            IFLS => self.ifls,
            IMSC => self.imsc,
            RIS => self.ris,
            MIS => self.ris & self.imsc,
            DMACR => self.dmacr,
            o if (ID_BASE..ID_BASE + 0x20).contains(&o) && o % 4 == 0 => {
                ID[((o - ID_BASE) / 4) as usize]
            }
            _ => 0,
        }
    }

    /// Handles a guest write of `value` to the register at `offset`.
    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            DR => {
                // A console write error cannot be reported to the guest; drop it.
                let _ = self.out.write_all(&[value as u8]);
                let _ = self.out.flush();
                self.ris |= INT_TX;
            }
            RSR => {}
            ILPR => self.ilpr = value,
            IBRD => self.ibrd = value,
            FBRD => self.fbrd = value,
            LCR_H => self.lcr_h = value,
            CR => self.cr = value,
            IFLS => self.ifls = value,
            IMSC => self.imsc = value & 0x7ff,
            ICR => {
                self.ris &= !value;
                // Transmit stays ready, and receive stays raised while bytes wait.
                self.ris |= INT_TX;
                if !self.rx.is_empty() {
                    self.ris |= INT_RX;
                }
            }
            DMACR => self.dmacr = value,
            _ => {}
        }
    }
}

/// Bytes of console output [`Console`] passes on for one VMM run.
pub const CONSOLE_LIMIT: u64 = 16 << 20;

/// The guest's console output, passed to `out` up to a limit, so a guest
/// writing without end cannot fill the Mac's disk through `vmm.log`. Past
/// it, one line says so and the rest is dropped.
pub struct Console<W> {
    out: W,
    left: u64,
}

impl<W: Write> Console<W> {
    pub fn new(out: W, limit: u64) -> Console<W> {
        Console { out, left: limit }
    }
}

impl<W: Write> Write for Console<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Ok(buf.len());
        }
        let n = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        self.out.write_all(&buf[..n])?;
        self.left -= n as u64;
        if self.left == 0 {
            self.out.write_all(
                b"\ncave-vmm: the guest's console wrote too much; the rest of it is dropped\n",
            )?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_output_stops_at_its_limit() {
        let mut console = Console::new(Vec::new(), 5);
        assert_eq!(console.write(b"abc").unwrap(), 3);
        assert_eq!(console.write(b"defg").unwrap(), 4);
        assert_eq!(console.write(b"hij").unwrap(), 3);
        let out = String::from_utf8(console.out).unwrap();
        assert!(out.starts_with("abcde\ncave-vmm: "), "{out}");
        assert!(out.ends_with("dropped\n") && !out.contains("hij"), "{out}");
    }

    #[test]
    fn state_round_trips() {
        let mut uart = Pl011::new(Vec::new());
        uart.write(IMSC, INT_RX);
        uart.write(LCR_H, 0x70);
        uart.push_input(b"ab");
        let state = uart.state();
        let mut other = Pl011::new(Vec::new());
        other.set_state(&state);
        assert_eq!(other.state(), state);
        assert!(other.irq_level());
        assert_eq!(other.read(DR), u32::from(b'a'));
    }

    #[test]
    fn transmit_goes_to_output() {
        let mut out = Vec::new();
        {
            let mut uart = Pl011::new(&mut out);
            for b in b"hi\n" {
                uart.write(DR, u32::from(*b));
            }
        }
        assert_eq!(out, b"hi\n");
    }

    #[test]
    fn flags_track_the_receive_fifo() {
        let mut uart = Pl011::new(Vec::new());
        assert_eq!(uart.read(FR) & FR_RXFE, FR_RXFE);
        uart.push_input(b"ab");
        assert_eq!(uart.read(FR) & FR_RXFE, 0);
        assert_eq!(uart.read(DR), u32::from(b'a'));
        assert_eq!(uart.read(DR), u32::from(b'b'));
        assert_eq!(uart.read(FR) & FR_RXFE, FR_RXFE);
        assert_eq!(uart.read(FR) & FR_TXFE, FR_TXFE, "transmit is always empty");
    }

    #[test]
    fn receive_interrupt_follows_mask_and_fifo() {
        let mut uart = Pl011::new(Vec::new());
        uart.push_input(b"x");
        assert!(!uart.irq_level(), "masked");
        uart.write(IMSC, INT_RX);
        assert!(uart.irq_level());
        assert_eq!(uart.read(MIS), INT_RX);
        uart.read(DR);
        assert!(!uart.irq_level(), "drained");
    }

    #[test]
    fn transmit_interrupt_when_unmasked() {
        let mut uart = Pl011::new(Vec::new());
        uart.write(IMSC, INT_TX);
        assert!(uart.irq_level());
        uart.write(IMSC, 0);
        assert!(!uart.irq_level());
    }

    #[test]
    fn primecell_id_matches_pl011() {
        let mut uart = Pl011::new(Vec::new());
        let ids: Vec<u32> = (0..8).map(|i| uart.read(ID_BASE + i * 4)).collect();
        assert_eq!(&ids[..2], &[0x11, 0x10]);
        assert_eq!(&ids[4..], &[0x0d, 0xf0, 0x05, 0xb1]);
    }

    #[test]
    fn control_registers_read_back() {
        let mut uart = Pl011::new(Vec::new());
        uart.write(IBRD, 13);
        uart.write(LCR_H, 0x70);
        uart.write(CR, 0x301);
        assert_eq!(
            (uart.read(IBRD), uart.read(LCR_H), uart.read(CR)),
            (13, 0x70, 0x301)
        );
    }
}
