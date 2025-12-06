use crate::Trap;
use crate::devices::clint::{CLINT_BASE, CLINT_SIZE, Clint};
use crate::devices::plic::{PLIC_BASE, PLIC_SIZE, Plic, UART_IRQ, VIRTIO0_IRQ};
use crate::devices::sysinfo::{SYSINFO_BASE, SYSINFO_SIZE, SysInfo};
use crate::devices::uart::{UART_BASE, UART_SIZE, Uart};
use crate::devices::virtio::VirtioDevice;
use crate::dram::Dram;

#[cfg(target_arch = "wasm32")]
use js_sys::SharedArrayBuffer;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;

/// Global mutex for AMO (Atomic Memory Operations) to ensure atomicity across harts.
///
/// On real RISC-V hardware, AMO instructions perform read-modify-write atomically.
/// In our emulator, each hart runs in a separate thread, so we need explicit
/// synchronization to prevent race conditions.
///
/// For WASM builds, we use JavaScript Atomics API instead (see atomic_* methods below).
#[cfg(not(target_arch = "wasm32"))]
static AMO_LOCK: Mutex<()> = Mutex::new(());

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

    // ========== Atomic Operations for SMP ==========
    //
    // These are used by AMO instructions. Default implementations use
    // non-atomic read-modify-write which works for single-threaded mode.
    // WASM implementations override these to use JavaScript Atomics API.

    /// Atomic swap (AMOSWAP): atomically replace value and return old value.
    fn atomic_swap(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        // Default non-atomic implementation (for single-threaded native)
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, value as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, value)?;
            Ok(old)
        }
    }

    /// Atomic add (AMOADD): atomically add and return old value.
    fn atomic_add(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, old.wrapping_add(value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old.wrapping_add(value))?;
            Ok(old)
        }
    }

    /// Atomic AND (AMOAND): atomically AND and return old value.
    fn atomic_and(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old & value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old & value)?;
            Ok(old)
        }
    }

    /// Atomic OR (AMOOR): atomically OR and return old value.
    fn atomic_or(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old | value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old | value)?;
            Ok(old)
        }
    }

    /// Atomic XOR (AMOXOR): atomically XOR and return old value.
    fn atomic_xor(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old ^ value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old ^ value)?;
            Ok(old)
        }
    }

    /// Atomic MIN signed (AMOMIN): atomically store min and return old value.
    fn atomic_min(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            let new = if (old as i64) < (value as i64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            let new = if (old as i64) < (value as i64) {
                old
            } else {
                value
            };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    /// Atomic MAX signed (AMOMAX): atomically store max and return old value.
    fn atomic_max(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            let new = if (old as i64) > (value as i64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            let new = if (old as i64) > (value as i64) {
                old
            } else {
                value
            };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    /// Atomic MIN unsigned (AMOMINU): atomically store min and return old value.
    fn atomic_minu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as u32 as u64;
            let new = if old < (value as u32 as u64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old as i32 as i64 as u64)
        } else {
            let old = self.read64(addr)?;
            let new = if old < value { old } else { value };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    /// Atomic MAX unsigned (AMOMAXU): atomically store max and return old value.
    fn atomic_maxu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if is_word {
            let old = self.read32(addr)? as u32 as u64;
            let new = if old > (value as u32 as u64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old as i32 as i64 as u64)
        } else {
            let old = self.read64(addr)?;
            let new = if old > value { old } else { value };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    /// Atomic compare-and-swap (for SC): returns (success, old_value).
    fn atomic_compare_exchange(
        &self,
        addr: u64,
        expected: u64,
        new_value: u64,
        is_word: bool,
    ) -> Result<(bool, u64), Trap> {
        if is_word {
            let old = self.read32(addr)? as u32;
            if old == expected as u32 {
                self.write32(addr, new_value as u32)?;
                Ok((true, old as i32 as i64 as u64))
            } else {
                Ok((false, old as i32 as i64 as u64))
            }
        } else {
            let old = self.read64(addr)?;
            if old == expected {
                self.write64(addr, new_value)?;
                Ok((true, old))
            } else {
                Ok((false, old))
            }
        }
    }
}

// A simple system bus that just wraps DRAM for now (Phase 1)
pub struct SystemBus {
    pub dram: Dram,
    pub clint: Clint,
    pub plic: Plic,
    pub uart: Uart,
    pub sysinfo: SysInfo,
    pub virtio_devices: Vec<Box<dyn VirtioDevice>>,
    /// Shared CLINT for WASM workers (routes CLINT accesses to SharedArrayBuffer)
    #[cfg(target_arch = "wasm32")]
    shared_clint: Option<crate::shared_mem::wasm::SharedClint>,
    /// Shared UART output for WASM workers (routes UART output to main thread)
    #[cfg(target_arch = "wasm32")]
    shared_uart_output: Option<crate::shared_mem::wasm::SharedUartOutput>,
    /// Shared UART input for WASM workers (receives keyboard input from main thread)
    #[cfg(target_arch = "wasm32")]
    shared_uart_input: Option<crate::shared_mem::wasm::SharedUartInput>,
}

impl SystemBus {
    pub fn new(dram_base: u64, dram_size: usize) -> Self {
        Self {
            dram: Dram::new(dram_base, dram_size),
            clint: Clint::new(),
            plic: Plic::new(),
            uart: Uart::new(),
            sysinfo: SysInfo::new(),
            virtio_devices: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            shared_clint: None,
            #[cfg(target_arch = "wasm32")]
            shared_uart_output: None,
            #[cfg(target_arch = "wasm32")]
            shared_uart_input: None,
        }
    }

    /// Create a SystemBus from an existing SharedArrayBuffer for SMP mode.
    ///
    /// Used by main thread and Web Workers to attach to shared memory.
    /// Both get a view of the shared DRAM and use the shared CLINT region
    /// for cross-hart communication (IPI, timer).
    ///
    /// # Arguments
    /// * `buffer` - The full SharedArrayBuffer containing control + CLINT + UART + DRAM regions
    /// * `dram_offset` - Byte offset where DRAM region starts within the buffer
    /// * `shared_clint` - SharedClint accessor for the shared CLINT region
    /// * `is_worker` - If true, enables shared UART input for receiving keyboard input.
    ///                 Main thread (hart 0) should pass false since it reads from local UART.
    ///
    /// IMPORTANT: Pass the FULL SharedArrayBuffer, not a sliced copy!
    /// SharedArrayBuffer::slice() creates a copy, breaking shared memory.
    #[cfg(target_arch = "wasm32")]
    pub fn from_shared_buffer(
        buffer: SharedArrayBuffer,
        dram_offset: usize,
        shared_clint: crate::shared_mem::wasm::SharedClint,
        is_worker: bool,
    ) -> Self {
        // Use the shared CLINT's hart count for local CLINT initialization
        // The local CLINT is a fallback; shared_clint is used for actual MMIO
        let num_harts = shared_clint.num_harts();
        let clint = Clint::with_harts(num_harts);

        // Create shared UART output for workers to send output to main thread
        let shared_uart_output = crate::shared_mem::wasm::SharedUartOutput::new(&buffer);

        // Create shared UART input only for workers - main thread reads from local UART
        let shared_uart_input = if is_worker {
            Some(crate::shared_mem::wasm::SharedUartInput::new(&buffer))
        } else {
            None
        };

        Self {
            dram: Dram::from_shared(DRAM_BASE, buffer, dram_offset),
            clint,
            plic: Plic::new(),
            uart: Uart::new(),
            sysinfo: SysInfo::new(),
            virtio_devices: Vec::new(),
            shared_clint: Some(shared_clint),
            shared_uart_output: Some(shared_uart_output),
            shared_uart_input,
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
    #[cfg(not(target_arch = "wasm32"))]
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
        if self
            .plic
            .is_interrupt_pending_for_fast(Plic::s_context(hart_id))
        {
            mip |= 1 << 9;
        }

        // MEIP (Machine External Interrupt) - Bit 11
        // Use fast lock-free check
        if self
            .plic
            .is_interrupt_pending_for_fast(Plic::m_context(hart_id))
        {
            mip |= 1 << 11;
        }

        mip
    }

    /// Check interrupts for a specific hart (WASM version).
    ///
    /// For WASM with shared memory, uses the shared CLINT to correctly
    /// receive IPIs between harts.
    #[cfg(target_arch = "wasm32")]
    pub fn check_interrupts_for_hart(&self, hart_id: usize) -> u64 {
        // Calculate MIP bits for this hart
        let mut mip: u64 = 0;

        // Get CLINT interrupts - use shared CLINT if available (for SMP),
        // otherwise fall back to local CLINT
        let (msip, timer) = if let Some(ref shared) = self.shared_clint {
            // Use shared CLINT for cross-hart IPI visibility
            shared.check_interrupts(hart_id)
        } else {
            self.clint.check_interrupts_for_hart(hart_id)
        };

        // MSIP (Machine Software Interrupt) - Bit 3
        if msip {
            mip |= 1 << 3;
        }

        // MTIP (Machine Timer Interrupt) - Bit 7
        if timer {
            mip |= 1 << 7;
        }

        // Hart 0 handles devices - advance timer and update PLIC
        // Workers (hart 1+) don't have virtio_devices, so PLIC checks are safe but no-op
        if hart_id == 0 {
            // Advance local CLINT timer from hart 0 only
            // Note: Shared CLINT timer is ticked separately in WasmVm::step()
            self.clint.tick();

            // Update PLIC with UART interrupt status
            let uart_irq = self.uart.is_interrupting();
            self.plic.set_source_level(UART_IRQ, uart_irq);

            // Update PLIC with VirtIO interrupts
            for (i, dev) in self.virtio_devices.iter().enumerate() {
                let irq = VIRTIO0_IRQ + i as u32;
                if irq < 32 {
                    self.plic.set_source_level(irq, dev.is_interrupting());
                }
            }
        }

        // SEIP (Supervisor External Interrupt) - Bit 9
        if self
            .plic
            .is_interrupt_pending_for_fast(Plic::s_context(hart_id))
        {
            mip |= 1 << 9;
        }

        // MEIP (Machine External Interrupt) - Bit 11
        if self
            .plic
            .is_interrupt_pending_for_fast(Plic::m_context(hart_id))
        {
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

    /// Load from CLINT, routing through shared CLINT when available (WASM workers).
    #[cfg(target_arch = "wasm32")]
    #[inline]
    fn clint_load(&self, offset: u64, size: u64) -> u64 {
        if let Some(ref shared) = self.shared_clint {
            shared.load(offset, size)
        } else {
            self.clint.load(offset, size)
        }
    }

    /// Load from CLINT (native builds always use local CLINT).
    #[cfg(not(target_arch = "wasm32"))]
    #[inline]
    fn clint_load(&self, offset: u64, size: u64) -> u64 {
        self.clint.load(offset, size)
    }

    /// Store to CLINT, routing through shared CLINT when available (WASM workers).
    #[cfg(target_arch = "wasm32")]
    #[inline]
    fn clint_store(&self, offset: u64, size: u64, value: u64) {
        if let Some(ref shared) = self.shared_clint {
            shared.store(offset, size, value);
        } else {
            self.clint.store(offset, size, value);
        }
    }

    /// Store to CLINT (native builds always use local CLINT).
    #[cfg(not(target_arch = "wasm32"))]
    #[inline]
    fn clint_store(&self, offset: u64, size: u64, value: u64) {
        self.clint.store(offset, size, value);
    }

    // Slow path methods for MMIO device access (moved out of hot path)

    #[cold]
    fn read8_slow(&self, addr: u64) -> Result<u8, Trap> {
        // Test finisher region: reads are harmless and return zero.
        if addr >= TEST_FINISHER_BASE && addr < TEST_FINISHER_BASE + TEST_FINISHER_SIZE {
            return Ok(0);
        }

        // SysInfo device
        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            let val = self.sysinfo.load(offset, 1);
            return Ok(val as u8);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint_load(offset, 1);
            return Ok(val as u8);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self
                .plic
                .load(offset, 1)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u8);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            // For workers with shared UART input, route reads to shared buffer
            #[cfg(target_arch = "wasm32")]
            if let Some(ref shared_uart) = self.shared_uart_input {
                if offset == 0 {
                    // Offset 0 is RBR (Receiver Buffer Register) - the data input
                    // Read from shared buffer (input from main thread)
                    if let Some(byte) = shared_uart.read_byte() {
                        return Ok(byte);
                    } else {
                        // No data available - return 0
                        return Ok(0);
                    }
                } else if offset == 5 {
                    // Offset 5 is LSR (Line Status Register)
                    // For workers, we need to check shared input for Data Ready bit
                    let mut lsr: u8 = 0x60; // TX empty bits always set
                    if shared_uart.has_data() {
                        lsr |= 0x01; // Data Ready bit
                    }
                    return Ok(lsr);
                }
            }
            // Fall through to local UART for main thread or other registers
            let val = self
                .uart
                .load(offset, 1)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u8);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            // Emulate narrow MMIO reads by extracting from the 32-bit register value
            let aligned = offset & !3;
            let word = self.virtio_devices[idx]
                .read(aligned)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            let val = self.sysinfo.load(offset, 2);
            return Ok(val as u16);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint_load(offset, 2);
            return Ok(val as u16);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self
                .plic
                .load(offset, 2)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u16);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            let val = self
                .uart
                .load(offset, 2)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u16);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let aligned = offset & !3;
            let word = self.virtio_devices[idx]
                .read(aligned)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            let val = self.sysinfo.load(offset, 4);
            return Ok(val as u32);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint_load(offset, 4);
            return Ok(val as u32);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self
                .plic
                .load(offset, 4)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u32);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            let val = self
                .uart
                .load(offset, 4)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val as u32);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let val = self.virtio_devices[idx]
                .read(offset)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            let val = self.sysinfo.load(offset, 8);
            return Ok(val);
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            let val = self.clint_load(offset, 8);
            return Ok(val);
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            let val = self
                .plic
                .load(offset, 8)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val);
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            let val = self
                .uart
                .load(offset, 8)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            return Ok(val);
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            let low = self.virtio_devices[idx]
                .read(offset)
                .map_err(|_| Trap::LoadAccessFault(addr))?;
            let high = self.virtio_devices[idx]
                .read(offset + 4)
                .map_err(|_| Trap::LoadAccessFault(addr + 4))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            self.sysinfo.store(offset, 1, val as u64);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint_store(offset, 1, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic
                .store(offset, 1, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            // For workers with shared UART output, route THR writes to shared buffer
            #[cfg(target_arch = "wasm32")]
            if offset == 0 {
                // Offset 0 is THR (Transmit Holding Register) - the data output
                if let Some(ref shared_uart) = self.shared_uart_output {
                    // Write to shared buffer so main thread can read it
                    let _ = shared_uart.write_byte(val);
                    return Ok(());
                }
            }
            // Fall through to local UART for main thread or non-THR registers
            self.uart
                .store(offset, 1, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            self.sysinfo.store(offset, 2, val as u64);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint_store(offset, 2, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic
                .store(offset, 2, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            self.uart
                .store(offset, 2, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            self.sysinfo.store(offset, 4, val as u64);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint_store(offset, 4, val as u64);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic
                .store(offset, 4, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            self.uart
                .store(offset, 4, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if let Some((idx, offset)) = self.get_virtio_device(addr) {
            self.virtio_devices[idx]
                .write(offset, val as u64, &self.dram)
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

        if addr >= SYSINFO_BASE && addr < SYSINFO_BASE + SYSINFO_SIZE {
            let offset = addr - SYSINFO_BASE;
            self.sysinfo.store(offset, 8, val);
            return Ok(());
        }

        if addr >= CLINT_BASE && addr < CLINT_BASE + CLINT_SIZE {
            let offset = addr - CLINT_BASE;
            self.clint_store(offset, 8, val);
            return Ok(());
        }

        if addr >= PLIC_BASE && addr < PLIC_BASE + PLIC_SIZE {
            let offset = addr - PLIC_BASE;
            self.plic
                .store(offset, 8, val)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
            return Ok(());
        }

        if addr >= UART_BASE && addr < UART_BASE + UART_SIZE {
            let offset = addr - UART_BASE;
            self.uart
                .store(offset, 8, val)
                .map_err(|_| Trap::StoreAccessFault(addr))?;
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

    // ========== WASM Atomic Operations ==========
    //
    // For WASM with SharedArrayBuffer, we use JavaScript Atomics API
    // to ensure proper synchronization across Web Workers.

    #[cfg(target_arch = "wasm32")]
    fn atomic_swap(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let old = self
                    .dram
                    .atomic_swap_32(off as u64, value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self
                    .dram
                    .atomic_swap_64(off as u64, value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old)
            }
        } else {
            // Non-DRAM addresses use non-atomic fallback
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                self.write32(addr, value as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                self.write64(addr, value)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_add(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let old = self
                    .dram
                    .atomic_add_32(off as u64, value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self
                    .dram
                    .atomic_add_64(off as u64, value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old)
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                self.write32(addr, old.wrapping_add(value) as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                self.write64(addr, old.wrapping_add(value))?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_and(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let old = self
                    .dram
                    .atomic_and_32(off as u64, value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self
                    .dram
                    .atomic_and_64(off as u64, value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old)
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                self.write32(addr, (old & value) as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                self.write64(addr, old & value)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_or(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let old = self
                    .dram
                    .atomic_or_32(off as u64, value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self
                    .dram
                    .atomic_or_64(off as u64, value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old)
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                self.write32(addr, (old | value) as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                self.write64(addr, old | value)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_xor(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let old = self
                    .dram
                    .atomic_xor_32(off as u64, value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self
                    .dram
                    .atomic_xor_64(off as u64, value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok(old)
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                self.write32(addr, (old ^ value) as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                self.write64(addr, old ^ value)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_min(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        // AMOMIN doesn't have direct Atomics support, use CAS loop
        if let Some(off) = self.dram.offset(addr) {
            loop {
                let old = if is_word {
                    self.dram
                        .atomic_load_32(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))? as i32 as i64
                        as u64
                } else {
                    self.dram
                        .atomic_load_64(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))?
                };
                let new = if (old as i64) < (value as i64) {
                    old
                } else {
                    value
                };
                if is_word {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_32(off as u64, old as u32, new as u32)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                } else {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_64(off as u64, old, new)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                }
                std::hint::spin_loop();
            }
        } else {
            // Fallback for non-DRAM
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                let new = if (old as i64) < (value as i64) {
                    old
                } else {
                    value
                };
                self.write32(addr, new as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                let new = if (old as i64) < (value as i64) {
                    old
                } else {
                    value
                };
                self.write64(addr, new)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_max(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            loop {
                let old = if is_word {
                    self.dram
                        .atomic_load_32(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))? as i32 as i64
                        as u64
                } else {
                    self.dram
                        .atomic_load_64(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))?
                };
                let new = if (old as i64) > (value as i64) {
                    old
                } else {
                    value
                };
                if is_word {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_32(off as u64, old as u32, new as u32)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                } else {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_64(off as u64, old, new)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                }
                std::hint::spin_loop();
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as i32 as i64 as u64;
                let new = if (old as i64) > (value as i64) {
                    old
                } else {
                    value
                };
                self.write32(addr, new as u32)?;
                Ok(old)
            } else {
                let old = self.read64(addr)?;
                let new = if (old as i64) > (value as i64) {
                    old
                } else {
                    value
                };
                self.write64(addr, new)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_minu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            loop {
                let old = if is_word {
                    self.dram
                        .atomic_load_32(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))? as u64
                } else {
                    self.dram
                        .atomic_load_64(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))?
                };
                let cmp_old = if is_word { old as u32 as u64 } else { old };
                let cmp_val = if is_word { value as u32 as u64 } else { value };
                let new = if cmp_old < cmp_val { old } else { value };
                if is_word {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_32(off as u64, old as u32, new as u32)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old as i32 as i64 as u64);
                    }
                } else {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_64(off as u64, old, new)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                }
                std::hint::spin_loop();
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as u32 as u64;
                let new = if old < (value as u32 as u64) {
                    old
                } else {
                    value
                };
                self.write32(addr, new as u32)?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self.read64(addr)?;
                let new = if old < value { old } else { value };
                self.write64(addr, new)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_maxu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        if let Some(off) = self.dram.offset(addr) {
            loop {
                let old = if is_word {
                    self.dram
                        .atomic_load_32(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))? as u64
                } else {
                    self.dram
                        .atomic_load_64(off as u64)
                        .map_err(|_| Trap::LoadAccessFault(addr))?
                };
                let cmp_old = if is_word { old as u32 as u64 } else { old };
                let cmp_val = if is_word { value as u32 as u64 } else { value };
                let new = if cmp_old > cmp_val { old } else { value };
                if is_word {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_32(off as u64, old as u32, new as u32)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old as i32 as i64 as u64);
                    }
                } else {
                    let (success, _) = self
                        .dram
                        .atomic_compare_exchange_64(off as u64, old, new)
                        .map_err(|_| Trap::StoreAccessFault(addr))?;
                    if success {
                        return Ok(old);
                    }
                }
                std::hint::spin_loop();
            }
        } else {
            if is_word {
                let old = self.read32(addr)? as u32 as u64;
                let new = if old > (value as u32 as u64) {
                    old
                } else {
                    value
                };
                self.write32(addr, new as u32)?;
                Ok(old as i32 as i64 as u64)
            } else {
                let old = self.read64(addr)?;
                let new = if old > value { old } else { value };
                self.write64(addr, new)?;
                Ok(old)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn atomic_compare_exchange(
        &self,
        addr: u64,
        expected: u64,
        new_value: u64,
        is_word: bool,
    ) -> Result<(bool, u64), Trap> {
        if let Some(off) = self.dram.offset(addr) {
            if is_word {
                let (success, old) = self
                    .dram
                    .atomic_compare_exchange_32(off as u64, expected as u32, new_value as u32)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok((success, old as i32 as i64 as u64))
            } else {
                let (success, old) = self
                    .dram
                    .atomic_compare_exchange_64(off as u64, expected, new_value)
                    .map_err(|_| Trap::StoreAccessFault(addr))?;
                Ok((success, old))
            }
        } else {
            // Fallback for non-DRAM
            if is_word {
                let old = self.read32(addr)? as u32;
                if old == expected as u32 {
                    self.write32(addr, new_value as u32)?;
                    Ok((true, old as i32 as i64 as u64))
                } else {
                    Ok((false, old as i32 as i64 as u64))
                }
            } else {
                let old = self.read64(addr)?;
                if old == expected {
                    self.write64(addr, new_value)?;
                    Ok((true, old))
                } else {
                    Ok((false, old))
                }
            }
        }
    }

    // ========== Native Atomic Operations ==========
    //
    // For native builds, we use a global lock to ensure atomicity of AMO operations.
    // This is simpler than per-address locking and correct for RISC-V semantics.

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_swap(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, value as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, value)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_add(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, old.wrapping_add(value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old.wrapping_add(value))?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_and(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old & value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old & value)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_or(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old | value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old | value)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_xor(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            self.write32(addr, (old ^ value) as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            self.write64(addr, old ^ value)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_min(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            let new = if (old as i64) < (value as i64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            let new = if (old as i64) < (value as i64) {
                old
            } else {
                value
            };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_max(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as i32 as i64 as u64;
            let new = if (old as i64) > (value as i64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old)
        } else {
            let old = self.read64(addr)?;
            let new = if (old as i64) > (value as i64) {
                old
            } else {
                value
            };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_minu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as u32 as u64;
            let new = if old < (value as u32 as u64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old as i32 as i64 as u64)
        } else {
            let old = self.read64(addr)?;
            let new = if old < value { old } else { value };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_maxu(&self, addr: u64, value: u64, is_word: bool) -> Result<u64, Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as u32 as u64;
            let new = if old > (value as u32 as u64) {
                old
            } else {
                value
            };
            self.write32(addr, new as u32)?;
            Ok(old as i32 as i64 as u64)
        } else {
            let old = self.read64(addr)?;
            let new = if old > value { old } else { value };
            self.write64(addr, new)?;
            Ok(old)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_compare_exchange(
        &self,
        addr: u64,
        expected: u64,
        new_value: u64,
        is_word: bool,
    ) -> Result<(bool, u64), Trap> {
        let _guard = AMO_LOCK.lock().unwrap();
        if is_word {
            let old = self.read32(addr)? as u32;
            if old == expected as u32 {
                self.write32(addr, new_value as u32)?;
                Ok((true, old as i32 as i64 as u64))
            } else {
                Ok((false, old as i32 as i64 as u64))
            }
        } else {
            let old = self.read64(addr)?;
            if old == expected {
                self.write64(addr, new_value)?;
                Ok((true, old))
            } else {
                Ok((false, old))
            }
        }
    }

    #[inline(always)]
    fn read8(&self, addr: u64) -> Result<u8, Trap> {
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self
                .dram
                .load_8(off as u64)
                .map_err(|_| Trap::LoadAccessFault(addr));
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
            return self
                .dram
                .load_16(off as u64)
                .map_err(|_| Trap::LoadAccessFault(addr));
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
            return self
                .dram
                .load_32(off as u64)
                .map_err(|_| Trap::LoadAccessFault(addr));
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
            return self
                .dram
                .load_64(off as u64)
                .map_err(|_| Trap::LoadAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.read64_slow(addr)
    }

    #[inline(always)]
    fn write8(&self, addr: u64, val: u8) -> Result<(), Trap> {
        // Fast path: DRAM access (most common case)
        if let Some(off) = self.dram.offset(addr) {
            return self
                .dram
                .store_8(off as u64, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr));
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
            return self
                .dram
                .store_16(off as u64, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr));
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
            return self
                .dram
                .store_32(off as u64, val as u64)
                .map_err(|_| Trap::StoreAccessFault(addr));
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
            return self
                .dram
                .store_64(off as u64, val)
                .map_err(|_| Trap::StoreAccessFault(addr));
        }
        // Slow path: MMIO devices
        self.write64_slow(addr, val)
    }
}
