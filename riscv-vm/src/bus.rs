use crate::clint::{Clint, CLINT_BASE, CLINT_SIZE};
use crate::plic::{Plic, PLIC_BASE, PLIC_SIZE, UART_IRQ, VIRTIO0_IRQ};
use crate::uart::{Uart, UART_BASE, UART_SIZE};
use crate::virtio::VirtioDevice;
use crate::Trap;
use crate::dram::Dram;

#[cfg(target_arch = "wasm32")]
use js_sys::SharedArrayBuffer;

/// Default DRAM base for the virt platform.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// Base address of the RISC-V test finisher MMIO region.
pub const TEST_FINISHER_BASE: u64 = 0x0010_0000;
pub const TEST_FINISHER_SIZE: u64 = 0x1000;

/// VirtIO MMIO base address (for the first device).
pub const VIRTIO_BASE: u64 = 0x1000_1000;
/// Size of each VirtIO MMIO region.
pub const VIRTIO_STRIDE: u64 = 0x1000;

/// System bus trait for memory and MMIO access.
///
/// All methods take `&self` to allow concurrent access from multiple harts.
/// Implementations must use interior mutability (Mutex, RwLock, atomics)
/// for any mutable state.
///
/// The `Send + Sync` bounds ensure implementations are thread-safe.
pub trait Bus: Send + Sync {
    fn read8(&self, addr: u64) -> Result<u8, Trap>;
    fn read16(&self, addr: u64) -> Result<u16, Trap>;
    fn read32(&self, addr: u64) -> Result<u32, Trap>;
    fn read64(&self, addr: u64) -> Result<u64, Trap>;

    fn write8(&self, addr: u64, val: u8) -> Result<(), Trap>;
    fn write16(&self, addr: u64, val: u16) -> Result<(), Trap>;
    fn write32(&self, addr: u64, val: u32) -> Result<(), Trap>;
    fn write64(&self, addr: u64, val: u64) -> Result<(), Trap>;

    /// Generic load helper used by the MMU for page-table walks.
    fn load(&self, addr: u64, size: u64) -> Result<u64, Trap> {
        match size {
            1 => self.read8(addr).map(|v| v as u64),
            2 => self.read16(addr).map(|v| v as u64),
            4 => self.read32(addr).map(|v| v as u64),
            8 => self.read64(addr),
            _ => Err(Trap::Fatal(format!("Unsupported bus load size: {}", size))),
        }
    }

    /// Generic store helper used by the MMU for page-table A/D updates.
    fn store(&self, addr: u64, size: u64, value: u64) -> Result<(), Trap> {
        match size {
            1 => self.write8(addr, value as u8),
            2 => self.write16(addr, value as u16),
            4 => self.write32(addr, value as u32),
            8 => self.write64(addr, value),
            _ => Err(Trap::Fatal(format!("Unsupported bus store size: {}", size))),
        }
    }

    fn fetch_u32(&self, addr: u64) -> Result<u32, Trap> {
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

    fn poll_interrupts(&self) -> u64 {
        0
    }

    /// Poll hardware interrupt sources for a specific hart.
    /// Returns MIP bits for that hart.
    /// Default implementation returns 0 (no interrupts).
    fn poll_interrupts_for_hart(&self, _hart_id: usize) -> u64 {
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

    /// Create a SystemBus from an existing SharedArrayBuffer.
    ///
    /// Used by Web Workers to attach to shared memory created by main thread.
    /// Workers get a view of the shared DRAM but have their own CLINT/PLIC/UART
    /// instances (these are per-worker and not shared).
    #[cfg(target_arch = "wasm32")]
    pub fn from_shared_buffer(buffer: SharedArrayBuffer) -> Self {
        Self {
            dram: Dram::from_shared(DRAM_BASE, buffer),
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
        self.dram.size()
    }
    
    /// Set the number of harts (called by emulator at init).
    /// This writes the hart count to a CLINT register so the kernel can read it.
    pub fn set_num_harts(&self, num_harts: usize) {
        self.clint.set_num_harts(num_harts);
    }

    /// Check interrupts for hart 0 (backward compatibility).
    pub fn check_interrupts(&self) -> u64 {
        self.check_interrupts_for_hart(0)
    }

    /// Check interrupts for a specific hart.
    /// 
    /// Each hart has its own:
    /// - MSIP (software interrupt from CLINT)
    /// - MTIP (timer interrupt from CLINT)
    /// - SEIP/MEIP (external interrupt from PLIC)
    /// 
    /// Thread-safe: each device has internal locking.
    /// Optimized to minimize lock acquisitions.
    pub fn check_interrupts_for_hart(&self, hart_id: usize) -> u64 {
        // Advance CLINT timer - only from hart 0 to avoid Nx speedup with N harts
        if hart_id == 0 {
            self.clint.tick();
        }

        // Update PLIC with UART interrupt status
        let uart_irq = self.uart.is_interrupting();
        self.plic.set_source_level(UART_IRQ, uart_irq);

        // Update PLIC with VirtIO interrupts
        // Device 0 -> IRQ 1 (VIRTIO0_IRQ)
        // Device 1 -> IRQ 2
        // etc.
        for (i, dev) in self.virtio_devices.iter().enumerate() {
            let irq = VIRTIO0_IRQ + i as u32;
            if irq < 32 {
                self.plic.set_source_level(irq, dev.is_interrupting());
            }
        }

        // Calculate MIP bits for this hart
        let mut mip: u64 = 0;

        // Get CLINT interrupts in a single lock acquisition
        let (msip, timer) = self.clint.check_interrupts_for_hart(hart_id);
        
        // MSIP (Machine Software Interrupt) - Bit 3
        if msip {
            mip |= 1 << 3;
        }

        // MTIP (Machine Timer Interrupt) - Bit 7
        if timer {
            mip |= 1 << 7;
        }

        // SEIP (Supervisor External Interrupt) - Bit 9
        // Use fast lock-free check
        if self.plic.is_interrupt_pending_for_fast(Plic::s_context(hart_id)) {
            mip |= 1 << 9;
        }

        // MEIP (Machine External Interrupt) - Bit 11
        // Use fast lock-free check
        if self.plic.is_interrupt_pending_for_fast(Plic::m_context(hart_id)) {
            mip |= 1 << 11;
        }

        mip
    }

    fn get_virtio_device(&self, addr: u64) -> Option<(usize, u64)> {
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
    pub fn poll_virtio(&self) {
        for device in &self.virtio_devices {
            if let Err(e) = device.poll(&self.dram) {
                log::warn!("[Bus] VirtIO poll error: {:?}", e);
            }
        }
    }
    
    // Slow path methods for MMIO device access (moved out of hot path)
    
    #[cold]
    fn read8_slow(&self, addr: u64) -> Result<u8, Trap> {
        // Test finisher region: reads are harmless and return zero.
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
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
    
    #[cold]
    fn read16_slow(&self, addr: u64) -> Result<u16, Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
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
    
    #[cold]
    fn read32_slow(&self, addr: u64) -> Result<u32, Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
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
    
    #[cold]
    fn read64_slow(&self, addr: u64) -> Result<u64, Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
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
    
    #[cold]
    fn write8_slow(&self, addr: u64, val: u8) -> Result<(), Trap> {
        // Any write in the test finisher region signals a requested trap to the host.
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
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
    
    #[cold]
    fn write16_slow(&self, addr: u64, val: u16) -> Result<(), Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
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
    
    #[cold]
    fn write32_slow(&self, addr: u64, val: u32) -> Result<(), Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val as u64));
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
            self.virtio_devices[idx].write(offset, val as u64, &self.dram)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }
        
        // Writes to unmapped VirtIO slots are silently ignored (allows safe probing)
        if self.is_virtio_region(addr).is_some() {
            return Ok(());
        }

        Err(Trap::StoreAccessFault(addr))
    }
    
    #[cold]
    fn write64_slow(&self, addr: u64, val: u64) -> Result<(), Trap> {
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Err(Trap::RequestedTrap(val));
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

impl Bus for SystemBus {
    #[inline]
    fn poll_interrupts(&self) -> u64 {
        self.check_interrupts()
    }

    #[inline]
    fn poll_interrupts_for_hart(&self, hart_id: usize) -> u64 {
        self.check_interrupts_for_hart(hart_id)
    }

    #[inline(always)]
    fn read8(&self, addr: u64) -> Result<u8, Trap> {
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.load_8(off as u64).map_err(|_| Trap::LoadAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.read8_slow(addr)
    }

    #[inline(always)]
    fn read16(&self, addr: u64) -> Result<u16, Trap> {
        if addr % 2 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.load_16(off as u64).map_err(|_| Trap::LoadAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.read16_slow(addr)
    }

    #[inline(always)]
    fn read32(&self, addr: u64) -> Result<u32, Trap> {
        if addr % 4 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.load_32(off as u64).map_err(|_| Trap::LoadAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.read32_slow(addr)
    }

    #[inline(always)]
    fn read64(&self, addr: u64) -> Result<u64, Trap> {
        if addr % 8 != 0 {
            return Err(Trap::LoadAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.load_64(off as u64).map_err(|_| Trap::LoadAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.read64_slow(addr)
    }

    #[inline(always)]
    fn write8(&self, addr: u64, val: u8) -> Result<(), Trap> {
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.store_8(off as u64, val as u64).map_err(|_| Trap::StoreAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.write8_slow(addr, val)
    }

    #[inline(always)]
    fn write16(&self, addr: u64, val: u16) -> Result<(), Trap> {
        if addr % 2 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.store_16(off as u64, val as u64).map_err(|_| Trap::StoreAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.write16_slow(addr, val)
    }

    #[inline(always)]
    fn write32(&self, addr: u64, val: u32) -> Result<(), Trap> {
        if addr % 4 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.store_32(off as u64, val as u64).map_err(|_| Trap::StoreAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.write32_slow(addr, val)
    }

    #[inline(always)]
    fn write64(&self, addr: u64, val: u64) -> Result<(), Trap> {
        if addr % 8 != 0 {
            return Err(Trap::StoreAddressMisaligned(addr));
        }
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self.dram.store_64(off as u64, val).map_err(|_| Trap::StoreAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.write64_slow(addr, val)
    }
}
