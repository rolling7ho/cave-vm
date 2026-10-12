//! Device tree generation for direct kernel boot.
//!
//! The tree describes memory, the CPUs, PSCI over HVC, the architected
//! timer, the GICv3, one PL011 UART and the used virtio-mmio slots.

use vm_fdt::{Error, FdtWriter};

use crate::layout;

/// Phandle of the interrupt controller.
const GIC_PHANDLE: u32 = 1;
/// Phandle of the fixed clock feeding the UART.
const CLOCK_PHANDLE: u32 = 2;

// Interrupt specifier fields for `arm,gic-v3` with three cells.
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;

/// What the device tree describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTreeConfig {
    /// Number of vCPUs.
    pub cpus: u32,
    /// Guest RAM size in bytes, starting at [`layout::RAM_BASE`].
    pub memory_size: u64,
    /// Size of the GIC distributor.
    pub gic_dist_size: u64,
    /// Size of one vCPU's GIC redistributor.
    pub gic_redist_size: u64,
    /// Kernel command line.
    pub cmdline: String,
    /// Guest physical start and end of the initramfs, if any.
    pub initrd: Option<(u64, u64)>,
    /// Number of virtio-mmio slots in use, from slot 0.
    pub virtio_slots: u32,
}

/// The `MPIDR_EL1` value, and device tree `reg`, of vCPU `index`. Affinity
/// level 0 holds the index; bit 31 is RES1.
pub fn mpidr(index: u32) -> u64 {
    (1 << 31) | u64::from(index & 0xff) | (u64::from(index >> 8) << 8)
}

/// Builds the flattened device tree blob.
pub fn build(config: &DeviceTreeConfig) -> Result<Vec<u8>, Error> {
    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "cave-vm,spike")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", &config.cmdline)?;
    fdt.property_string("stdout-path", &format!("/pl011@{:x}", layout::UART_BASE))?;
    if let Some((start, end)) = config.initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64("linux,initrd-end", end)?;
    }
    fdt.end_node(chosen)?;

    let memory = fdt.begin_node(&format!("memory@{:x}", layout::RAM_BASE))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[layout::RAM_BASE, config.memory_size])?;
    fdt.end_node(memory)?;

    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    for index in 0..config.cpus {
        let reg = (mpidr(index) & 0x00ff_ffff) as u32;
        let cpu = fdt.begin_node(&format!("cpu@{reg:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_string("enable-method", "psci")?;
        fdt.property_u32("reg", reg)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let psci = fdt.begin_node("psci")?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()],
    )?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    let gic = fdt.begin_node(&format!("intc@{:x}", layout::GIC_DIST_BASE))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_array_u64(
        "reg",
        &[
            layout::GIC_DIST_BASE,
            config.gic_dist_size,
            layout::GIC_REDIST_BASE,
            config.gic_redist_size * u64::from(config.cpus),
        ],
    )?;
    fdt.property_phandle(GIC_PHANDLE)?;
    fdt.end_node(gic)?;

    // Secure physical, non-secure physical, virtual and hypervisor timers,
    // as PPIs 13, 14, 11 and 10 (interrupt IDs 29, 30, 27 and 26).
    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    let mut cells = Vec::new();
    for ppi in [13, 14, 11, 10] {
        cells.extend_from_slice(&[GIC_PPI, ppi, IRQ_TYPE_LEVEL_HIGH]);
    }
    fdt.property_array_u32("interrupts", &cells)?;
    fdt.end_node(timer)?;

    let clock = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_string("clock-output-names", "clk24mhz")?;
    fdt.property_phandle(CLOCK_PHANDLE)?;
    fdt.end_node(clock)?;

    let uart = fdt.begin_node(&format!("pl011@{:x}", layout::UART_BASE))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )?;
    fdt.property_array_u64("reg", &[layout::UART_BASE, layout::UART_SIZE])?;
    fdt.property_array_u32(
        "interrupts",
        &[GIC_SPI, layout::UART_SPI, IRQ_TYPE_LEVEL_HIGH],
    )?;
    fdt.property_array_u32("clocks", &[CLOCK_PHANDLE, CLOCK_PHANDLE])?;
    fdt.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])?;
    fdt.end_node(uart)?;

    for slot in 0..config.virtio_slots.min(layout::VIRTIO_MMIO_SLOTS) {
        let base = layout::virtio_mmio_base(slot);
        let node = fdt.begin_node(&format!("virtio_mmio@{base:x}"))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[base, layout::VIRTIO_MMIO_SIZE])?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_SPI, layout::virtio_spi(slot), IRQ_TYPE_LEVEL_HIGH],
        )?;
        fdt.property_null("dma-coherent")?;
        fdt.end_node(node)?;
    }

    fdt.end_node(root)?;
    fdt.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spike_config() -> DeviceTreeConfig {
        DeviceTreeConfig {
            cpus: 2,
            memory_size: 512 << 20,
            gic_dist_size: 0x1_0000,
            gic_redist_size: 0x2_0000,
            cmdline: "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init".into(),
            initrd: Some((0x5f00_0000, 0x5f40_0000)),
            virtio_slots: 0,
        }
    }

    /// The blob is compared byte for byte with a checked-in file. To accept
    /// an intended change, run with `CAVE_UPDATE_GOLDEN=1` and review the
    /// diff of `tests/golden/spike.dtb` (`dtc -I dtb -O dts` decodes it).
    #[test]
    fn matches_golden_file() {
        let blob = build(&spike_config()).unwrap();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/spike.dtb");
        if std::env::var_os("CAVE_UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &blob).unwrap();
        }
        let golden =
            std::fs::read(path).expect("golden file missing; run with CAVE_UPDATE_GOLDEN=1");
        assert!(
            blob == golden,
            "device tree changed; see the comment on this test"
        );
    }

    #[test]
    fn header_is_valid() {
        let blob = build(&spike_config()).unwrap();
        let be32 = |at: usize| u32::from_be_bytes(blob[at..at + 4].try_into().unwrap());
        assert_eq!(be32(0), 0xd00d_feed, "magic");
        assert_eq!(be32(4) as usize, blob.len(), "totalsize");
        assert_eq!(be32(20), 17, "version");
    }

    #[test]
    fn contains_the_command_line_and_devices() {
        let blob = build(&spike_config()).unwrap();
        let contains = |needle: &str| blob.windows(needle.len()).any(|w| w == needle.as_bytes());
        for needle in [
            "rdinit=/init",
            "arm,gic-v3",
            "arm,pl011",
            "arm,psci-1.0",
            "arm,armv8-timer",
            "cpu@1",
            "memory@40000000",
            "linux,initrd-start",
        ] {
            assert!(contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn virtio_slots_appear_with_their_interrupts() {
        let blob = build(&DeviceTreeConfig {
            virtio_slots: 2,
            ..spike_config()
        })
        .unwrap();
        let count = |needle: &str| {
            blob.windows(needle.len())
                .filter(|w| *w == needle.as_bytes())
                .count()
        };
        assert_eq!(count("virtio,mmio"), 2);
        assert_eq!(count("virtio_mmio@c000000"), 1);
        assert_eq!(count("virtio_mmio@c000200"), 1);
        assert_eq!(count("virtio_mmio@c000400"), 0);
        // Slot 1's interrupt specifier: SPI 17, level high.
        let spec: Vec<u8> = [0u32, 17, 4].iter().flat_map(|c| c.to_be_bytes()).collect();
        assert!(blob.windows(spec.len()).any(|w| w == spec.as_slice()));
    }

    #[test]
    fn mpidr_sets_affinity_and_res1() {
        assert_eq!(mpidr(0), 0x8000_0000);
        assert_eq!(mpidr(3), 0x8000_0003);
        assert_eq!(mpidr(0x102), 0x8000_0102);
    }
}
