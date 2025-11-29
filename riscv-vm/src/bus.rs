use crate::clint::{Clint, CLINT_BASE, CLINT_SIZE};
use crate::plic::{Plic, PLIC_BASE, PLIC_SIZE};
use crate::uart::{Uart, UART_BASE, UART_SIZE};
use crate::virtio::VirtioDevice;
use crate::Trap;
use crate::dram::Dram;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen;

/// Default DRAM base for the virt platform.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// Base address of the RISC-V test finisher MMIO region.
pub const TEST_FINISHER_BASE: u64 = 0x0010_0000;
pub const TEST_FINISHER_SIZE: u64 = 0x1000;

/// VirtIO MMIO base address (for the first device).
pub const VIRTIO_BASE: u64 = 0x1000_1000;
/// Size of each VirtIO MMIO region.
pub const VIRTIO_STRIDE: u64 = 0x1000;

pub trait Bus {
    fn read8(&mut self, addr: u64) -> Result<u8, Trap>;
    fn read16(&mut self, addr: u64) -> Result<u16, Trap>;
    fn read32(&mut self, addr: u64) -> Result<u32, Trap>;
    fn read64(&mut self, addr: u64) -> Result<u64, Trap>;

    fn write8(&mut self, addr: u64, val: u8) -> Result<(), Trap>;
    fn write16(&mut self, addr: u64, val: u16) -> Result<(), Trap>;
    fn write32(&mut self, addr: u64, val: u32) -> Result<(), Trap>;
    fn write64(&mut self, addr: u64, val: u64) -> Result<(), Trap>;

    /// Generic load helper used by the MMU for page-table walks.
    fn load(&mut self, addr: u64, size: u64) -> Result<u64, Trap> {
        match size {
            1 => self.read8(addr).map(|v| v as u64),
            2 => self.read16(addr).map(|v| v as u64),
            4 => self.read32(addr).map(|v| v as u64),
            8 => self.read64(addr),
            _ => Err(Trap::Fatal(format!("Unsupported bus load size: {}", size))),
        }
    }

    /// Generic store helper used by the MMU for page-table A/D updates.
    fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), Trap> {
        match size {
            1 => self.write8(addr, value as u8),
            2 => self.write16(addr, value as u16),
            4 => self.write32(addr, value as u32),
            8 => self.write64(addr, value),
            _ => Err(Trap::Fatal(format!("Unsupported bus store size: {}", size))),
        }
    }

    fn fetch_u32(&mut self, addr: u64) -> Result<u32, Trap> {
        if addr % 4 != 0 {
            return Err(Trap::InstructionAddressMisaligned(addr));
        }
        // Map LoadAccessFault to InstructionAccessFault for fetch
        self.read32(addr).map_err(|e| match e {
            Trap::LoadAccessFault(a) => Trap::InstructionAccessFault(a),
            Trap::LoadAddressMisaligned(a) => Trap::InstructionAddressMisaligned(a),
            _ => e,
        })
    }

    fn poll_interrupts(&mut self) -> u64 {
        0
    }
}

// A simple system bus that just wraps DRAM for now (Phase 1)
pub struct SystemBus {
    pub dram: Dram,
    pub clint: Clint,
    pub plic: Plic,
    pub uart: Uart,
    pub virtio_devices: Vec<Box<dyn VirtioDevice>>,
}

impl SystemBus {
    pub fn new(dram_base: u64, dram_size: usize) -> Self {
        Self {
            dram: Dram::new(dram_base, dram_size),
            clint: Clint::new(),
            plic: Plic::new(),
            uart: Uart::new(),
            virtio_devices: Vec::new(),
        }
    }

    pub fn dram_base(&self) -> u64 {
        self.dram.base
    }

    pub fn dram_size(&self) -> usize {
        self.dram.data.len()
    }

    pub fn check_interrupts(&mut self) -> u64 {
        // 0. Advance CLINT timer each step
        self.clint.tick();
        
        // 1. Update PLIC with UART status
        let uart_irq = self.uart.interrupting;
        self.plic.set_source_level(crate::plic::UART_IRQ, uart_irq);

        // 1b. Update PLIC with VirtIO interrupts
        // We map VirtIO devices to IRQs starting at VIRTIO0_IRQ (1).
        // Device 0 -> IRQ 1
        // Device 1 -> IRQ 2
        // ...
        // Device 7 -> IRQ 8
        // Note: xv6 expects VIRTIO0 at IRQ 1.
        for (i, dev) in self.virtio_devices.iter().enumerate() {
            let irq = crate::plic::VIRTIO0_IRQ + i as u32;
            if irq < 32 { // PLIC limit
                let intr = dev.is_interrupting();
                self.plic.set_source_level(irq, intr);
            }
        }

        // 2. Calculate MIP bits
        let mut mip = 0;

        // MSIP (Machine Software Interrupt) - Bit 3
        if self.clint.msip[0] & 1 != 0 {
            mip |= 1 << 3;
        }

        // MTIP (Machine Timer Interrupt) - Bit 7
        if self.clint.mtime >= self.clint.mtimecmp[0] {
            mip |= 1 << 7;
        }

        // SEIP (Supervisor External Interrupt) - Bit 9
        if self.plic.is_interrupt_pending_for(1) {
            mip |= 1 << 9;
        }

        // MEIP (Machine External Interrupt) - Bit 11
        if self.plic.is_interrupt_pending_for(0) {
            mip |= 1 << 11;
        }

        mip
    }

    fn get_virtio_device(&mut self, addr: u64) -> Option<(usize, u64)> {
        if addr >= VIRTIO_BASE {
            let offset = addr - VIRTIO_BASE;
            let idx = (offset / VIRTIO_STRIDE) as usize;
            if idx < self.virtio_devices.len() {
                return Some((idx, offset % VIRTIO_STRIDE));
            }
        }
        None
    }
    
    /// Check if an address is in the VirtIO MMIO region (even if no device present).
    /// Returns the offset within the device region if in range.
    fn is_virtio_region(&self, addr: u64) -> Option<u64> {
        if addr >= VIRTIO_BASE && addr < VIRTIO_BASE + VIRTIO_STRIDE * 8 {
            Some((addr - VIRTIO_BASE) % VIRTIO_STRIDE)
        } else {
            None
        }
    }
    
    /// Poll all VirtIO devices for pending work (e.g., incoming network packets).
    /// Should be called periodically from the main emulation loop.
    pub fn poll_virtio(&mut self) {
        for device in &mut self.virtio_devices {
            if let Err(e) = device.poll(&mut self.dram) {
                log::warn!("[Bus] VirtIO poll error: {:?}", e);
            }
        }
    }
}

impl Bus for SystemBus {
    fn poll_interrupts(&mut self) -> u64 {
        self.check_interrupts()
    }

    fn read8(&mut self, addr: u64) -> Result<u8, Trap> {
        // Test finisher region: reads are harmless and return zero.
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
        }

        if let Some(off) = self.dram.offset(addr) {
            let val = self.dram.data[off];
            return Ok(val);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint.load(offset, 1);
            return Ok(val as u8);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self.plic.load(offset, 1).map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u8);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             let val = self.uart.load(offset, 1).map_err(|_| Trap::LoadAccessFault(addr))?;
             return Ok(val as u8);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            // Emulate narrow MMIO reads by extracting from the 32-bit register value
            let aligned = offset & !3;
            let word = self.virtio_devices[idx].read(aligned).map_err(|_| Trap::LoadAccessFault(addr))?;
            let shift = ((offset & 3) * 8) as u64;
            return Ok(((word >> shift) & 0xff) as u8);
        }
        
        // Unmapped VirtIO slots return 0 (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(0);
        }

        Err(Trap::LoadAccessFault(addr))
    }

    fn read16(&mut self, addr: u64) -> Result<u16, Trap> {
        if addr % 2 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 2 > self.dram.data.len() {
                return Err(Trap::LoadAccessFault(addr));
            }
            let bytes = &self.dram.data[off..off + 2];
            return Ok(u16::from_le_bytes(bytes.try_into().unwrap()));
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint.load(offset, 2);
            return Ok(val as u16);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self.plic.load(offset, 2).map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u16);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             let val = self.uart.load(offset, 2).map_err(|_| Trap::LoadAccessFault(addr))?;
             return Ok(val as u16);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let aligned = offset & !3;
            let word = self.virtio_devices[idx].read(aligned).map_err(|_| Trap::LoadAccessFault(addr))?;
            let shift = ((offset & 3) * 8) as u64;
            return Ok(((word >> shift) & 0xffff) as u16);
        }
        
        // Unmapped VirtIO slots return 0 (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(0);
        }

        Err(Trap::LoadAccessFault(addr))
    }

    fn read32(&mut self, addr: u64) -> Result<u32, Trap> {
        if addr % 4 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 4 > self.dram.data.len() {
                return Err(Trap::LoadAccessFault(addr));
            }
            let bytes = &self.dram.data[off..off + 4];
            return Ok(u32::from_le_bytes(bytes.try_into().unwrap()));
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint.load(offset, 4);
            return Ok(val as u32);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self.plic.load(offset, 4).map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u32);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             let val = self.uart.load(offset, 4).map_err(|_| Trap::LoadAccessFault(addr))?;
             return Ok(val as u32);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let val = self.virtio_devices[idx].read(offset).map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u32);
        }
        
        // Unmapped VirtIO slots return 0 (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(0);
        }

        Err(Trap::LoadAccessFault(addr))
    }

    fn read64(&mut self, addr: u64) -> Result<u64, Trap> {
        if addr % 8 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 8 > self.dram.data.len() {
                return Err(Trap::LoadAccessFault(addr));
            }
            let bytes = &self.dram.data[off..off + 8];
            let val = u64::from_le_bytes(bytes.try_into().unwrap());
            return Ok(val);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint.load(offset, 8);
            return Ok(val);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self.plic.load(offset, 8).map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             let val = self.uart.load(offset, 8).map_err(|_| Trap::LoadAccessFault(addr))?;
             return Ok(val);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let low = self.virtio_devices[idx].read(offset).map_err(|_| Trap::LoadAccessFault(addr))?;
            let high = self.virtio_devices[idx].read(offset + 4).map_err(|_| Trap::LoadAccessFault(addr + 4))?;
            return Ok((low as u64) | ((high as u64) << 32));
        }
        
        // Unmapped VirtIO slots return 0 (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(0);
        }

        Err(Trap::LoadAccessFault(addr))
    }

    fn write8(&mut self, addr: u64, val: u8) -> Result<(), Trap> {
        // Any write in the test finisher region signals a requested trap to the host.
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
        }

        if let Some(off) = self.dram.offset(addr) {
            self.dram.data[off] = val;
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint.store(offset, 1, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic.store(offset, 1, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             self.uart.store(offset, 1, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
             return Ok(());
        }

        if let Some((_idx, _offset)) = self.get_virtio_device(addr) {
            // VirtIO registers are 32-bit. Byte writes are not strictly supported by the spec for all registers.
            // We ignore them for now to be safe.
            return Ok(());
        }

        Err(Trap::StoreAccessFault(addr))
    }

    fn write16(&mut self, addr: u64, val: u16) -> Result<(), Trap> {
        if addr % 2 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 2 > self.dram.data.len() {
                return Err(Trap::StoreAccessFault(addr));
            }
            let bytes = val.to_le_bytes();
            self.dram.data[off..off + 2].copy_from_slice(&bytes);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint.store(offset, 2, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic.store(offset, 2, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             self.uart.store(offset, 2, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
             return Ok(());
        }

        if let Some((_idx, _offset)) = self.get_virtio_device(addr) {
            return Ok(());
        }

        Err(Trap::StoreAccessFault(addr))
    }

    fn write32(&mut self, addr: u64, val: u32) -> Result<(), Trap> {
        if addr % 4 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 4 > self.dram.data.len() {
                return Err(Trap::StoreAccessFault(addr));
            }
            let bytes = val.to_le_bytes();
            self.dram.data[off..off + 4].copy_from_slice(&bytes);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint.store(offset, 4, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic.store(offset, 4, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             self.uart.store(offset, 4, val as u64).map_err(|_| Trap::StoreAccessFault(addr))?;
             return Ok(());
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            self.virtio_devices[idx].write(offset, val as u64, &mut self.dram)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }
        
        // Writes to unmapped VirtIO slots are silently ignored (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(());
        }

        Err(Trap::StoreAccessFault(addr))
    }

    fn write64(&mut self, addr: u64, val: u64) -> Result<(), Trap> {
        if addr % 8 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }

        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val));
        }

        if let Some(off) = self.dram.offset(addr) {
            if off + 8 > self.dram.data.len() {
                return Err(Trap::StoreAccessFault(addr));
            }
            let bytes = val.to_le_bytes();
            self.dram.data[off..off + 8].copy_from_slice(&bytes);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint.store(offset, 8, val);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic.store(offset, 8, val).map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
             let offset = addr - UART_BASE;
             self.uart.store(offset, 8, val).map_err(|_| Trap::StoreAccessFault(addr))?;
             return Ok(());
        }

        if let Some((_idx, _offset)) = self.get_virtio_device(addr) {
            // VirtIO registers are 32-bit. 64-bit writes are not typically supported directly via MMIO
            // except for legacy queue PFN which is 32-bit anyway.
            return Ok(());
        }
        
        // Writes to unmapped VirtIO slots are silently ignored (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(());
        }

        Err(Trap::StoreAccessFault(addr))
    }
}
