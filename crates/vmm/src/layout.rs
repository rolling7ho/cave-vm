//! The guest physical address map.
//!
//! Low addresses hold devices and the interrupt controller; RAM starts at
//! 1 GiB, as on QEMU's `virt` machine, so familiar kernel configs work.

/// Base of the GICv3 distributor (64 KiB on current hosts).
pub const GIC_DIST_BASE: u64 = 0x0800_0000;
/// Base of the PL011 UART.
pub const UART_BASE: u64 = 0x0900_0000;
/// Size of the PL011 UART's register window.
pub const UART_SIZE: u64 = 0x1000;
/// Shared peripheral interrupt number of the UART, as written in the device
/// tree. The GIC interrupt ID is this plus 32.
pub const UART_SPI: u32 = 1;
/// Base of the GICv3 redistributor region, one redistributor per vCPU
/// (32 MiB on current hosts, enough for the 64-vCPU maximum).
pub const GIC_REDIST_BASE: u64 = 0x0a00_0000;
/// Start of guest RAM.
pub const RAM_BASE: u64 = 0x4000_0000;

/// First GIC interrupt ID of shared peripheral interrupts.
pub const SPI_BASE_INTID: u32 = 32;
/// First GIC interrupt ID of private peripheral interrupts.
pub const PPI_BASE_INTID: u32 = 16;

/// GIC interrupt ID of the UART.
pub const fn uart_intid() -> u32 {
    SPI_BASE_INTID + UART_SPI
}

// Devices must not overlap each other or RAM.
const _: () = assert!(GIC_DIST_BASE + 0x1_0000 <= UART_BASE);
const _: () = assert!(UART_BASE + UART_SIZE <= GIC_REDIST_BASE);
const _: () = assert!(GIC_REDIST_BASE + 0x200_0000 <= RAM_BASE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_is_spi_one() {
        assert_eq!(uart_intid(), 33);
    }
}
