//! Device tree generation, modelled on QEMU's `virt` board so that unmodified OpenSBI and
//! Linux find everything where they expect it.

use vm_fdt::{Error, FdtWriter};

use crate::layout::*;

pub struct FdtParams<'a> {
    pub num_harts: usize,
    pub ram_size: u64,
    pub isa_extensions: &'a [&'a str],
    pub mmu_type: &'a str,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    /// Base and size of the 64-bit PCIe memory window.
    pub pcie_mmio_high: (u64, u64),
}

const PHANDLE_PLIC: u32 = 1;
const PHANDLE_TEST: u32 = 2;
const fn phandle_cpu_intc(hart: usize) -> u32 {
    0x100 + hart as u32
}

pub fn build(p: &FdtParams) -> Result<Vec<u8>, Error> {
    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_string("compatible", "riscv-virtio")?;
    fdt.property_string("model", "rust-v,virt")?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", p.cmdline)?;
    fdt.property_string("stdout-path", &format!("/soc/serial@{UART_BASE:x}"))?;
    if let Some((start, end)) = p.initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64("linux,initrd-end", end)?;
    }
    fdt.end_node(chosen)?;

    let memory = fdt.begin_node(&format!("memory@{RAM_BASE:x}"))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[RAM_BASE, p.ram_size])?;
    fdt.end_node(memory)?;

    let isa = format!("rv64{}", isa_string(p.isa_extensions));
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    fdt.property_u32("timebase-frequency", TIMEBASE_HZ as u32)?;
    for hart in 0..p.num_harts {
        let cpu = fdt.begin_node(&format!("cpu@{hart}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_u32("reg", hart as u32)?;
        fdt.property_string("status", "okay")?;
        fdt.property_string("compatible", "riscv")?;
        fdt.property_string("riscv,isa", &isa)?;
        fdt.property_string("riscv,isa-base", "rv64i")?;
        fdt.property_string_list(
            "riscv,isa-extensions",
            p.isa_extensions.iter().map(|s| s.to_string()).collect(),
        )?;
        fdt.property_string("mmu-type", p.mmu_type)?;
        let block = rv_core::exec::CACHE_BLOCK as u32;
        fdt.property_u32("riscv,cbom-block-size", block)?;
        fdt.property_u32("riscv,cbop-block-size", block)?;
        fdt.property_u32("riscv,cboz-block-size", block)?;
        let intc = fdt.begin_node("interrupt-controller")?;
        fdt.property_u32("#interrupt-cells", 1)?;
        fdt.property_null("interrupt-controller")?;
        fdt.property_string("compatible", "riscv,cpu-intc")?;
        fdt.property_phandle(phandle_cpu_intc(hart))?;
        fdt.end_node(intc)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let soc = fdt.begin_node("soc")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_string("compatible", "simple-bus")?;
    fdt.property_null("ranges")?;

    let clint = fdt.begin_node(&format!("clint@{CLINT_BASE:x}"))?;
    fdt.property_string_list("compatible", vec!["sifive,clint0".into(), "riscv,clint0".into()])?;
    fdt.property_array_u64("reg", &[CLINT_BASE, crate::clint::SIZE])?;
    let cells: Vec<u32> = (0..p.num_harts)
        .flat_map(|h| {
            [
                phandle_cpu_intc(h),
                rv_core::trap::irq::MSI as u32,
                phandle_cpu_intc(h),
                rv_core::trap::irq::MTI as u32,
            ]
        })
        .collect();
    fdt.property_array_u32("interrupts-extended", &cells)?;
    fdt.end_node(clint)?;

    let plic = fdt.begin_node(&format!("plic@{PLIC_BASE:x}"))?;
    fdt.property_string_list("compatible", vec!["sifive,plic-1.0.0".into(), "riscv,plic0".into()])?;
    fdt.property_array_u64("reg", &[PLIC_BASE, crate::plic::SIZE])?;
    fdt.property_u32("#interrupt-cells", 1)?;
    fdt.property_u32("#address-cells", 0)?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("riscv,ndev", crate::plic::NUM_SOURCES as u32 - 1)?;
    let cells: Vec<u32> = (0..p.num_harts)
        .flat_map(|h| {
            [
                phandle_cpu_intc(h),
                rv_core::trap::irq::MEI as u32,
                phandle_cpu_intc(h),
                rv_core::trap::irq::SEI as u32,
            ]
        })
        .collect();
    fdt.property_array_u32("interrupts-extended", &cells)?;
    fdt.property_phandle(PHANDLE_PLIC)?;
    fdt.end_node(plic)?;

    let serial = fdt.begin_node(&format!("serial@{UART_BASE:x}"))?;
    fdt.property_string("compatible", "ns16550a")?;
    fdt.property_array_u64("reg", &[UART_BASE, crate::uart::SIZE])?;
    fdt.property_u32("clock-frequency", 3_686_400)?;
    fdt.property_u32("interrupt-parent", PHANDLE_PLIC)?;
    fdt.property_u32("interrupts", UART_IRQ)?;
    fdt.end_node(serial)?;

    let rtc = fdt.begin_node(&format!("rtc@{RTC_BASE:x}"))?;
    fdt.property_string("compatible", "google,goldfish-rtc")?;
    fdt.property_array_u64("reg", &[RTC_BASE, crate::rtc::SIZE])?;
    fdt.property_u32("interrupt-parent", PHANDLE_PLIC)?;
    fdt.property_u32("interrupts", RTC_IRQ)?;
    fdt.end_node(rtc)?;

    let test = fdt.begin_node(&format!("test@{TEST_BASE:x}"))?;
    fdt.property_string_list(
        "compatible",
        vec!["sifive,test1".into(), "sifive,test0".into(), "syscon".into()],
    )?;
    fdt.property_array_u64("reg", &[TEST_BASE, crate::power::SIZE])?;
    fdt.property_phandle(PHANDLE_TEST)?;
    fdt.end_node(test)?;

    for (name, value) in [("poweroff", 0x5555), ("reboot", 0x7777)] {
        let node = fdt.begin_node(name)?;
        fdt.property_string("compatible", &format!("syscon-{name}"))?;
        fdt.property_u32("regmap", PHANDLE_TEST)?;
        fdt.property_u32("offset", 0)?;
        fdt.property_u32("value", value)?;
        fdt.end_node(node)?;
    }

    pcie(&mut fdt, p)?;

    fdt.end_node(soc)?;
    fdt.end_node(root)?;
    fdt.finish()
}

/// The PCIe host bridge, described like QEMU's `gpex`.
fn pcie(fdt: &mut FdtWriter, p: &FdtParams) -> Result<(), Error> {
    use crate::pci::{ECAM_SIZE, NUM_INTX};
    let node = fdt.begin_node(&format!("pci@{PCIE_ECAM_BASE:x}"))?;
    fdt.property_string("compatible", "pci-host-ecam-generic")?;
    fdt.property_string("device_type", "pci")?;
    fdt.property_u32("#address-cells", 3)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("#interrupt-cells", 1)?;
    fdt.property_u32("linux,pci-domain", 0)?;
    fdt.property_array_u32("bus-range", &[0, (ECAM_SIZE >> 20) as u32 - 1])?;
    fdt.property_null("dma-coherent")?;
    fdt.property_array_u64("reg", &[PCIE_ECAM_BASE, ECAM_SIZE])?;
    // (PCI space code + PCI address, CPU address, size), with 64-bit values split into cells.
    let (high_base, high_size) = p.pcie_mmio_high;
    let ranges: Vec<u32> = [
        (0x0100_0000, 0, PCIE_PIO_BASE, PCIE_PIO_SIZE),
        (0x0200_0000, PCIE_MMIO_BASE, PCIE_MMIO_BASE, PCIE_MMIO_SIZE),
        (0x0300_0000, high_base, high_base, high_size),
    ]
    .iter()
    .flat_map(|&(space, pci, cpu, size)| {
        [
            space,
            (pci >> 32) as u32,
            pci as u32,
            (cpu >> 32) as u32,
            cpu as u32,
            (size >> 32) as u32,
            size as u32,
        ]
    })
    .collect();
    fdt.property_array_u32("ranges", &ranges)?;
    // INTx swizzling: pin P of slot S arrives on line (S + P) % 4.
    let mut map = Vec::new();
    for slot in 0..NUM_INTX {
        for pin in 0..NUM_INTX {
            let irq = PCIE_IRQ + (slot + pin) % NUM_INTX;
            map.extend_from_slice(&[slot << 11, 0, 0, pin + 1, PHANDLE_PLIC, irq]);
        }
    }
    fdt.property_array_u32("interrupt-map", &map)?;
    fdt.property_array_u32("interrupt-map-mask", &[0x1800, 0, 0, 7])?;
    fdt.end_node(node)
}

/// Formats an extension list as an ISA string suffix: single letters first, then `_`-separated
/// multi-letter extensions.
fn isa_string(extensions: &[&str]) -> String {
    let mut s: String = extensions.iter().filter(|e| e.len() == 1).copied().collect();
    for e in extensions.iter().filter(|e| e.len() > 1) {
        s.push('_');
        s.push_str(e);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isa_string_format() {
        assert_eq!(isa_string(&["i", "m", "a", "c", "zicsr", "zifencei"]), "imac_zicsr_zifencei");
    }

    #[test]
    fn builds_a_valid_blob() {
        let blob = build(&FdtParams {
            num_harts: 4,
            ram_size: 512 << 20,
            isa_extensions: &["i", "m", "a", "c"],
            mmu_type: "riscv,sv48",
            cmdline: "console=ttyS0",
            initrd: Some((0x9000_0000, 0x9010_0000)),
            pcie_mmio_high: (0x4_0000_0000, 16 << 30),
        })
        .unwrap();
        assert_eq!(&blob[..4], &[0xd0, 0x0d, 0xfe, 0xed]);
    }
}
