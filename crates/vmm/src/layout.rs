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
/// Base of the virtio-mmio slots, one device each.
pub const VIRTIO_MMIO_BASE: u64 = 0x0c00_0000;
/// Size of one virtio-mmio slot's register window.
pub const VIRTIO_MMIO_SIZE: u64 = 0x200;
/// Number of virtio-mmio slots.
pub const VIRTIO_MMIO_SLOTS: u32 = 32;
/// Shared peripheral interrupt number of the first virtio-mmio slot; slot
/// `n` uses this plus `n`.
pub const VIRTIO_SPI_BASE: u32 = 16;
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

/// Guest physical address of virtio-mmio slot `slot`.
pub const fn virtio_mmio_base(slot: u32) -> u64 {
    VIRTIO_MMIO_BASE + slot as u64 * VIRTIO_MMIO_SIZE
}

/// Shared peripheral interrupt number of virtio-mmio slot `slot`.
pub const fn virtio_spi(slot: u32) -> u32 {
    VIRTIO_SPI_BASE + slot
}

/// GIC interrupt ID of virtio-mmio slot `slot`.
pub const fn virtio_intid(slot: u32) -> u32 {
    SPI_BASE_INTID + virtio_spi(slot)
}

/// The virtio-mmio slot holding guest address `gpa`, and the offset into it.
pub fn virtio_slot_of(gpa: u64) -> Option<(u32, u64)> {
    let end = virtio_mmio_base(VIRTIO_MMIO_SLOTS);
    if !(VIRTIO_MMIO_BASE..end).contains(&gpa) {
        return None;
    }
    let rel = gpa - VIRTIO_MMIO_BASE;
    Some(((rel / VIRTIO_MMIO_SIZE) as u32, rel % VIRTIO_MMIO_SIZE))
}

// Devices must not overlap each other or RAM.
const _: () = assert!(GIC_DIST_BASE + 0x1_0000 <= UART_BASE);
const _: () = assert!(UART_BASE + UART_SIZE <= GIC_REDIST_BASE);
const _: () = assert!(GIC_REDIST_BASE + 0x200_0000 <= VIRTIO_MMIO_BASE);
const _: () = assert!(virtio_mmio_base(VIRTIO_MMIO_SLOTS) <= RAM_BASE);
// Virtio interrupts must not collide with the UART's.
const _: () = assert!(UART_SPI < VIRTIO_SPI_BASE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_is_spi_one() {
        assert_eq!(uart_intid(), 33);
    }

    #[test]
    fn virtio_slots_are_consecutive() {
        assert_eq!(virtio_mmio_base(0), 0x0c00_0000);
        assert_eq!(virtio_mmio_base(3), 0x0c00_0600);
        assert_eq!(virtio_intid(0), 48);
        assert_eq!(virtio_slot_of(0x0c00_0604), Some((3, 4)));
        assert_eq!(virtio_slot_of(0x0c00_0000 - 1), None);
        assert_eq!(virtio_slot_of(virtio_mmio_base(VIRTIO_MMIO_SLOTS)), None);
    }
}
