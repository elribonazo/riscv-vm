use crate::bus::{SystemBus, DRAM_BASE};
use crate::cpu::Cpu;
use crate::Trap;
use goblin::elf::{program_header::PT_LOAD, Elf};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[cfg(not(target_arch = "wasm32"))]
use crate::console::Console;

/// Shared state between main thread and worker threads.
///
/// This struct is wrapped in Arc and shared across all threads.
/// All fields use atomics for lock-free synchronization.
///
/// Aligned to 64 bytes to prevent false sharing with adjacent data.
/// Combined flags into a single atomic for faster polling.
#[repr(align(64))]
pub struct SharedState {
    /// Combined flags: bit 0 = halt_requested, bit 1 = halted
    /// Using a single atomic reduces should_stop() from 2 loads to 1.
    flags: AtomicU8,
    /// Halt code (e.g., from TEST_FINISHER).
    halt_code: AtomicU64,
    /// Padding to prevent false sharing with adjacent data.
    _padding: [u8; 64 - std::mem::size_of::<AtomicU8>() - std::mem::size_of::<AtomicU64>()],
}

impl SharedState {
    /// Flag bit for halt_requested
    const HALT_REQUESTED: u8 = 0x01;
    /// Flag bit for halted
    const HALTED: u8 = 0x02;

    /// Create new shared state.
    pub fn new() -> Self {
        Self {
            flags: AtomicU8::new(0),
            halt_code: AtomicU64::new(0),
            _padding: [0; 64 - std::mem::size_of::<AtomicU8>() - std::mem::size_of::<AtomicU64>()],
        }
    }

    /// Request all threads to halt.
    pub fn request_halt(&self) {
        // Use Release ordering - ensures all prior writes are visible
        // before the halt flag becomes visible
        self.flags.fetch_or(Self::HALT_REQUESTED, Ordering::Release);
    }

    /// Check if halt has been requested.
    pub fn is_halt_requested(&self) -> bool {
        // Use Relaxed - we just need to eventually see the flag
        (self.flags.load(Ordering::Relaxed) & Self::HALT_REQUESTED) != 0
    }

    /// Signal that a thread has halted (e.g., due to trap).
    pub fn signal_halted(&self, code: u64) {
        self.halt_code.store(code, Ordering::Relaxed);
        // Use Release for the halted flag to ensure halt_code is visible
        self.flags.fetch_or(Self::HALTED, Ordering::Release);
    }

    /// Check if any thread has halted.
    pub fn is_halted(&self) -> bool {
        (self.flags.load(Ordering::Relaxed) & Self::HALTED) != 0
    }

    /// Get the halt code.
    pub fn halt_code(&self) -> u64 {
        // Use Acquire to ensure we see the halt_code written before halted was set
        self.halt_code.load(Ordering::Acquire)
    }

    /// Check if we should stop (either requested or already halted).
    /// 
    /// Performance note: This is called frequently, so we use Relaxed ordering
    /// and a single atomic load (combined flags). The flags will eventually
    /// become visible.
    #[inline(always)]
    pub fn should_stop(&self) -> bool {
        // Single atomic load instead of two - faster polling
        self.flags.load(Ordering::Relaxed) != 0
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Default DRAM size used when constructing an [`Emulator`] via [`Emulator::new`].
///
/// This is large enough for riscv-arch-test binaries and small kernels, while
/// still being reasonably light for host machines.
const DEFAULT_DRAM_MIB: usize = 128;

/// Default size of the signature region when only a base address is provided.
///
/// RISCOF test signatures are typically small; 4 KiB is a conservative
/// default and can be overridden via [`Emulator::set_signature_region`].
const DEFAULT_SIGNATURE_SIZE: u64 = 4 * 1024;

/// High-level emulator wrapper used by test harnesses (e.g. RISCOF backend).
///
/// This mirrors the sketch in `phase-6.md`:
///
/// ```ignore
/// let mut emu = Emulator::new();
/// emu.load_elf("test.elf")?;
/// emu.set_signature_addr(0x8001_0000);
/// while !emu.trapped() { emu.step()?; }
/// let sig = emu.read_signature()?;
/// ```
pub struct Emulator {
    /// CPU core (GPRs, CSRs, privilege mode, TLB, etc).
    pub cpu: Cpu,
    /// System bus with DRAM and all memory-mapped devices.
    pub bus: SystemBus,

    signature_addr: Option<u64>,
    signature_size: u64,

    trapped: bool,
    last_trap: Option<Trap>,

    /// Optional UART output callback invoked once per transmitted byte.
    ///
    /// This provides a deterministic, buffered integration point for hosts
    /// (CLI, web UI, tests) without requiring them to poll the UART FIFO.
    uart_callback: Option<Box<dyn FnMut(u8) + 'static>>,
}

impl Emulator {
    /// Create a new emulator instance with the default DRAM size and reset PC.
    ///
    /// The reset PC is initialised to the DRAM base (0x8000_0000) but will be
    /// overwritten by [`load_elf`] when an ELF image is loaded.
    pub fn new() -> Self {
        Self::with_memory(DEFAULT_DRAM_MIB * 1024 * 1024)
    }

    /// Create a new emulator instance with an explicit DRAM size in bytes.
    pub fn with_memory(dram_size_bytes: usize) -> Self {
        let dram_base = DRAM_BASE;
        let bus = SystemBus::new(dram_base, dram_size_bytes);
        let cpu = Cpu::new(dram_base, 0);  // hart_id = 0

        Self {
            cpu,
            bus,
            signature_addr: None,
            signature_size: 0,
            trapped: false,
            last_trap: None,
            uart_callback: None,
        }
    }

    /// Returns `true` once execution has terminated due to a trap or
    /// an explicit host-level stop condition.
    pub fn trapped(&self) -> bool {
        self.trapped
    }

    /// Returns the last architectural trap observed, if any.
    pub fn last_trap(&self) -> Option<&Trap> {
        self.last_trap.as_ref()
    }

    /// Register a UART output callback.
    ///
    /// The callback is invoked from [`step`] for each byte emitted by the
    /// emulated NS16550A UART. Hosts that prefer pull-based I/O can ignore
    /// this and call [`drain_uart_output`] instead.
    pub fn set_uart_callback<F>(&mut self, cb: F)
    where
        F: FnMut(u8) + 'static,
    {
        self.uart_callback = Some(Box::new(cb));
    }

    /// Push a single input byte into the UART RX FIFO.
    ///
    /// This models a host keystroke or serial input event in a buffered,
    /// deterministic way: given the same sequence of calls and instruction
    /// stream, the guest will see identical input ordering.
    pub fn push_key(&mut self, byte: u8) {
        self.bus.uart.push_input(byte);
    }

    /// Drain all pending UART output bytes into a vector.
    ///
    /// This is useful for tests or hosts that do not wish to use the callback
    /// interface.
    pub fn drain_uart_output(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(b) = self.bus.uart.pop_output() {
            out.push(b);
        }
        out
    }

    /// Execute a single instruction.
    ///
    /// On success, returns `Ok(())`. On architectural traps, this records the
    /// trap in [`last_trap`] and sets [`trapped`] before returning `Err(trap)`.
    pub fn step(&mut self) -> Result<(), Trap> {
        match self.cpu.step(&self.bus) {
            Ok(()) => {
                // Deliver UART bytes to host callback if registered.
                if let Some(cb) = self.uart_callback.as_mut() {
                    while let Some(byte) = self.bus.uart.pop_output() {
                        cb(byte);
                    }
                }

                Ok(())
            }
            Err(trap) => {
                self.trapped = true;
                self.last_trap = Some(trap.clone());
                Err(trap)
            }
        }
    }

    /// Load an ELF image from disk into DRAM and update the CPU's PC to the
    /// ELF entry point.
    ///
    /// Returns the resolved entry PC on success.
    pub fn load_elf<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let entry_pc = load_elf_into_dram(&buffer, &mut self.bus)?;
        self.cpu.pc = entry_pc;
        Ok(entry_pc)
    }

    /// Configure the signature region used by `read_signature`.
    ///
    /// - `base` is the physical start address of the signature buffer.
    /// - `size` is the number of bytes to read.
    pub fn set_signature_region(&mut self, base: u64, size: u64) {
        self.signature_addr = Some(base);
        self.signature_size = size;
    }

    /// Convenience helper matching the `phase-6.md` sketch.
    ///
    /// This sets the base address and uses a default size of 4 KiB unless a
    /// region size has already been configured via [`set_signature_region`].
    pub fn set_signature_addr(&mut self, base: u64) {
        self.signature_addr = Some(base);
        if self.signature_size == 0 {
            self.signature_size = DEFAULT_SIGNATURE_SIZE;
        }
    }

    /// Read the configured signature region from DRAM.
    ///
    /// Returns an owned `Vec<u8>` which callers can hex-encode or compare
    /// against reference signatures.
    pub fn read_signature(&self) -> Result<Vec<u8>, String> {
        let base = self
            .signature_addr
            .ok_or_else(|| "signature address not configured".to_string())?;
        if self.signature_size == 0 {
            return Err("signature size is zero; call set_signature_region first".to_string());
        }

        let dram_base = self.bus.dram_base();
        let dram_size = self.bus.dram_size() as u64;

        if base < dram_base || base >= dram_base + dram_size {
            return Err(format!(
                "signature base 0x{base:016x} lies outside DRAM (0x{dram_base:016x}..0x{:016x})",
                dram_base + dram_size
            ));
        }

        let offset = (base - dram_base) as usize;
        let end = offset
            .checked_add(self.signature_size as usize)
            .ok_or_else(|| "signature range overflow".to_string())?;

        if end > self.bus.dram_size() {
            return Err("signature range extends beyond DRAM".to_string());
        }

        // SAFETY: bounds checked above.
        self.bus.dram.read_range(offset, self.signature_size as usize)
            .map_err(|e| format!("failed to read signature: {}", e))
    }
}

const SNAPSHOT_VERSION: &str = "2.0";

/// Serializable CPU state used in snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSnapshot {
    pub pc: u64,
    pub mode: crate::csr::Mode,
    pub regs: [u64; 32],
    pub csrs: HashMap<u16, u64>,
}

/// Serializable CLINT state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClintSnapshot {
    pub msip: Vec<u32>,
    pub mtime: u64,
    pub mtimecmp: Vec<u64>,
}

/// Serializable PLIC state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlicSnapshot {
    pub priority: Vec<u32>,
    pub pending: u32,
    pub enable: Vec<u32>,
    pub threshold: Vec<u32>,
    pub active: Vec<u32>,
}

/// Serializable UART state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UartSnapshot {
    pub rx_fifo: Vec<u8>,
    pub tx_fifo: Vec<u8>,
    pub ier: u8,
    pub iir: u8,
    pub fcr: u8,
    pub lcr: u8,
    pub mcr: u8,
    pub lsr: u8,
    pub msr: u8,
    pub scr: u8,
    pub dll: u8,
    pub dlm: u8,
}

/// Serializable device state bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub clint: ClintSnapshot,
    pub plic: PlicSnapshot,
    pub uart: UartSnapshot,
}

/// Memory region snapshot (currently we only snapshot DRAM as a single region).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemRegionSnapshot {
    pub base: u64,
    pub size: u64,
    pub hash: String,
    pub data: Option<Vec<u8>>,
}

/// Full emulator snapshot including CPU, devices and DRAM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: String,
    pub cpu: CpuSnapshot,
    pub devices: DeviceSnapshot,
    pub memory: Vec<MemRegionSnapshot>,
}

impl Emulator {
    /// Capture a complete, deterministic snapshot of the current emulator state.
    pub fn snapshot(&self) -> Snapshot {
        let cpu = CpuSnapshot {
            pc: self.cpu.pc,
            mode: self.cpu.mode,
            regs: self.cpu.regs,
            csrs: self.cpu.export_csrs(),
        };

        let clint = ClintSnapshot {
            msip: self.bus.clint.get_msip_array().to_vec(),
            mtime: self.bus.clint.mtime(),
            mtimecmp: self.bus.clint.get_mtimecmp_array().to_vec(),
        };

        let plic = PlicSnapshot {
            priority: self.bus.plic.get_priority(),
            pending: self.bus.plic.get_pending(),
            enable: self.bus.plic.get_enable(),
            threshold: self.bus.plic.get_threshold(),
            active: self.bus.plic.get_active(),
        };

        let (ier, iir, fcr, lcr, mcr, lsr, msr, scr, dll, dlm) = self.bus.uart.get_registers();
        let uart = UartSnapshot {
            rx_fifo: self.bus.uart.get_input(),
            tx_fifo: self.bus.uart.get_output(),
            ier,
            iir,
            fcr,
            lcr,
            mcr,
            lsr,
            msr,
            scr,
            dll,
            dlm,
        };

        let dram_data = self.bus.dram.get_data();
        let mut hasher = Sha256::new();
        hasher.update(&dram_data);
        let hash = hex::encode(hasher.finalize());

        let region = MemRegionSnapshot {
            base: self.bus.dram.base,
            size: self.bus.dram.size() as u64,
            hash,
            data: Some(dram_data),
        };

        Snapshot {
            version: SNAPSHOT_VERSION.to_string(),
            cpu,
            devices: DeviceSnapshot { clint, plic, uart },
            memory: vec![region],
        }
    }

    /// Restore emulator state from a previously captured snapshot.
    pub fn apply_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(format!(
                "snapshot version mismatch: expected {}, found {}",
                SNAPSHOT_VERSION, snapshot.version
            ));
        }

        // Restore CPU core.
        self.cpu.pc = snapshot.cpu.pc;
        self.cpu.mode = snapshot.cpu.mode;
        self.cpu.regs = snapshot.cpu.regs;
        self.cpu.import_csrs(&snapshot.cpu.csrs);
        self.trapped = false;
        self.last_trap = None;

        // Restore CLINT.
        self.bus.clint.set_msip_array(&snapshot.devices.clint.msip);
        self.bus.clint.set_mtime(snapshot.devices.clint.mtime);
        self.bus.clint.set_mtimecmp_array(&snapshot.devices.clint.mtimecmp);

        // Restore PLIC.
        self.bus.plic.set_priority(&snapshot.devices.plic.priority);
        self.bus.plic.set_pending(snapshot.devices.plic.pending);
        self.bus.plic.set_enable(&snapshot.devices.plic.enable);
        self.bus.plic.set_threshold(&snapshot.devices.plic.threshold);
        self.bus.plic.set_active(&snapshot.devices.plic.active);

        // Restore UART.
        self.bus.uart.set_input(&snapshot.devices.uart.rx_fifo);
        self.bus.uart.set_output(&snapshot.devices.uart.tx_fifo);
        self.bus.uart.set_registers(
            snapshot.devices.uart.ier,
            snapshot.devices.uart.iir,
            snapshot.devices.uart.fcr,
            snapshot.devices.uart.lcr,
            snapshot.devices.uart.mcr,
            snapshot.devices.uart.lsr,
            snapshot.devices.uart.msr,
            snapshot.devices.uart.scr,
            snapshot.devices.uart.dll,
            snapshot.devices.uart.dlm,
        );

        // Restore DRAM.
        let region = snapshot
            .memory
            .get(0)
            .ok_or_else(|| "snapshot missing primary memory region".to_string())?;

        let data = region
            .data
            .as_ref()
            .ok_or_else(|| "snapshot memory region has no inline data".to_string())?;

        if self.bus.dram.base != region.base {
            return Err(format!(
                "snapshot DRAM base mismatch: emulator=0x{:x}, snapshot=0x{:x}",
                self.bus.dram.base, region.base
            ));
        }
        if self.bus.dram.size() != data.len() {
            return Err(format!(
                "snapshot DRAM size mismatch: emulator={} bytes, snapshot={} bytes",
                self.bus.dram.size(),
                data.len()
            ));
        }

        let mut hasher = Sha256::new();
        hasher.update(data);
        let current_hash = hex::encode(hasher.finalize());
        if current_hash != region.hash {
            return Err(format!(
                "snapshot DRAM hash mismatch for base 0x{:x}",
                region.base
            ));
        }

        self.bus.dram.set_data(data)
            .map_err(|e| format!("failed to restore DRAM: {}", e))?;

        Ok(())
    }

    /// Construct a new emulator instance from a snapshot.
    pub fn from_snapshot(snapshot: Snapshot) -> Result<Self, String> {
        let region = snapshot
            .memory
            .get(0)
            .ok_or_else(|| "snapshot missing primary memory region".to_string())?;
        let dram_size = region
            .size
            .try_into()
            .map_err(|_| "snapshot DRAM size does not fit in usize".to_string())?;

        let mut emu = Emulator::with_memory(dram_size);
        emu.apply_snapshot(&snapshot)?;
        Ok(emu)
    }

    /// Save a snapshot to disk using bincode.
    pub fn save_snapshot_to_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let snap = self.snapshot();
        let mut file = File::create(path)?;
        bincode::serialize_into(&mut file, &snap)?;
        file.flush()?;
        Ok(())
    }

    /// Load a snapshot from disk and construct a new emulator instance.
    pub fn load_snapshot_from_path<P: AsRef<Path>>(
        path: P,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let snapshot: Snapshot = bincode::deserialize_from(&mut file)?;
        let emu = Emulator::from_snapshot(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(emu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;

    // --- Memory layout tests (Task 10.1/10.4) ---

    #[test]
    fn measure_shared_state_size() {
        use crate::cpu::Cpu;
        use crate::bus::SystemBus;
        use crate::clint::Clint;
        use crate::plic::Plic;

        println!("=== Phase 10 Memory Layout Analysis ===");
        println!("Cpu size: {} bytes", std::mem::size_of::<Cpu>());
        println!("Cpu align: {} bytes", std::mem::align_of::<Cpu>());
        println!("SharedState size: {} bytes", std::mem::size_of::<SharedState>());
        println!("SharedState align: {} bytes", std::mem::align_of::<SharedState>());
        println!("SystemBus size: {} bytes", std::mem::size_of::<SystemBus>());
        println!("Clint size: {} bytes", std::mem::size_of::<Clint>());
        println!("Plic size: {} bytes", std::mem::size_of::<Plic>());
    }

    #[test]
    fn test_shared_state_alignment() {
        // SharedState should be cache-line aligned (64 bytes)
        assert_eq!(
            std::mem::align_of::<SharedState>(),
            64,
            "SharedState should be aligned to 64 bytes"
        );

        // Size should be exactly 64 bytes (one cache line)
        assert_eq!(
            std::mem::size_of::<SharedState>(),
            64,
            "SharedState should be exactly 64 bytes (one cache line)"
        );
    }

    #[test]
    fn test_shared_state_should_stop() {
        let state = SharedState::new();

        // Initially not stopped
        assert!(!state.should_stop());
        assert!(!state.is_halt_requested());
        assert!(!state.is_halted());

        // Request halt
        state.request_halt();
        assert!(state.should_stop());
        assert!(state.is_halt_requested());
        assert!(!state.is_halted());

        // Reset for next test (create new instance)
        let state2 = SharedState::new();
        assert!(!state2.should_stop());

        // Signal halted
        state2.signal_halted(42);
        assert!(state2.should_stop());
        assert!(!state2.is_halt_requested());
        assert!(state2.is_halted());
        assert_eq!(state2.halt_code(), 42);
    }

    #[test]
    fn test_shared_state_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let state = Arc::new(SharedState::new());
        let mut handles = vec![];

        // Spawn multiple threads polling should_stop
        for _ in 0..4 {
            let state_clone = Arc::clone(&state);
            let handle = thread::spawn(move || {
                for _ in 0..100_000 {
                    let _ = state_clone.should_stop();
                }
            });
            handles.push(handle);
        }

        // All threads should complete without panic
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn snapshot_roundtrip_preserves_state() {
        let mut emu = Emulator::with_memory(1024 * 1024);

        // Simple CPU state.
        emu.cpu.pc = DRAM_BASE + 0x1000;
        emu.cpu.write_reg(crate::decoder::Register::X5, 0xdead_beef_dead_beef);

        // Touch DRAM and devices.
        let addr = emu.bus.dram_base() + 0x80;
        emu.bus.write64(addr, 0x0123_4567_89ab_cdef).unwrap();
        emu.bus.clint.set_mtime(1234);
        {
            let mut mtimecmp = emu.bus.clint.get_mtimecmp_array();
            mtimecmp[0] = 5678;
            emu.bus.clint.set_mtimecmp_array(&mtimecmp);
        }
        emu.bus.uart.push_input(b'A');

        let snap = emu.snapshot();
        let bytes = bincode::serialize(&snap).unwrap();
        let snap2: Snapshot = bincode::deserialize(&bytes).unwrap();

        let emu2 = Emulator::from_snapshot(snap2).unwrap();

        assert_eq!(emu.cpu.pc, emu2.cpu.pc);
        assert_eq!(
            emu.cpu.read_reg(crate::decoder::Register::X5),
            emu2.cpu.read_reg(crate::decoder::Register::X5)
        );
        assert_eq!(emu.bus.dram.get_data(), emu2.bus.dram.get_data());
        assert_eq!(emu.bus.clint.mtime(), emu2.bus.clint.mtime());
        assert_eq!(emu.bus.clint.get_mtimecmp_array(), emu2.bus.clint.get_mtimecmp_array());
        assert_eq!(emu.bus.uart.get_input(), emu2.bus.uart.get_input());
    }
}

fn load_elf_into_dram(
    buffer: &[u8],
    bus: &mut SystemBus,
) -> Result<u64, Box<dyn std::error::Error>> {
    let elf = Elf::parse(buffer)?;
    let base = bus.dram_base();
    let dram_end = base + bus.dram_size() as u64;

    for ph in &elf.program_headers {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }

        let file_size = ph.p_filesz as usize;
        let mem_size = ph.p_memsz as usize;
        let file_offset = ph.p_offset as usize;
        if file_offset + file_size > buffer.len() {
            return Err(format!(
                "ELF segment exceeds file bounds (offset 0x{:x})",
                file_offset
            )
            .into());
        }

        let target_addr = if ph.p_paddr != 0 {
            ph.p_paddr
        } else {
            ph.p_vaddr
        };
        if target_addr < base {
            return Err(format!(
                "Segment start 0x{:x} lies below DRAM base 0x{:x}",
                target_addr, base
            )
            .into());
        }
        let seg_end = target_addr
            .checked_add(mem_size as u64)
            .ok_or_else(|| "Segment end overflow".to_string())?;
        if seg_end > dram_end {
            return Err(format!(
                "Segment 0x{:x}-0x{:x} exceeds DRAM (end 0x{:x})",
                target_addr, seg_end, dram_end
            )
            .into());
        }

        let dram_offset = (target_addr - base) as u64;
        if file_size > 0 {
            let end = file_offset + file_size;
            bus.dram
                .load(&buffer[file_offset..end], dram_offset)
                .map_err(|e| format!("Failed to load segment: {}", e))?;
        }
        if mem_size > file_size {
            let zero_start = dram_offset as usize + file_size;
            bus.dram
                .zero_range(zero_start, mem_size - file_size)
                .map_err(|e| format!("Failed to zero bss: {}", e))?;
        }
        log::debug!(
            "Loaded segment: addr=0x{:x}, filesz=0x{:x}, memsz=0x{:x}",
            target_addr,
            file_size,
            mem_size
        );
    }

    Ok(elf.entry)
}

// ============================================================================
// NativeVm - Multi-threaded VM for native execution
// ============================================================================

/// Reason for halting batch execution.
#[cfg(not(target_arch = "wasm32"))]
enum HaltReason {
    /// VM shutdown requested (e.g., via TEST_FINISHER)
    Shutdown(u64),
    /// Fatal emulator error (message, PC)
    Fatal(String, u64),
}

#[cfg(not(target_arch = "wasm32"))]
/// Native multi-threaded VM.
///
/// Manages one thread per hart, with hart 0 running on the main thread
/// for I/O coordination.
pub struct NativeVm {
    /// Shared bus (thread-safe after Phase 2).
    bus: Arc<SystemBus>,
    /// Thread handles for worker threads (harts 1+).
    handles: Vec<JoinHandle<()>>,
    /// Primary hart CPU (runs on main thread for I/O).
    primary_cpu: Option<Cpu>,
    /// Shared state for coordination.
    pub shared: Arc<SharedState>,
    /// Number of harts.
    num_harts: usize,
    /// Kernel entry point.
    entry_pc: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeVm {
    /// Create a new VM with the given kernel.
    ///
    /// # Arguments
    /// * `kernel` - Kernel binary (ELF or raw)
    /// * `num_harts` - Number of harts (CPUs) to create
    pub fn new(kernel: &[u8], num_harts: usize) -> Result<Self, String> {
        const DRAM_SIZE: usize = 512 * 1024 * 1024;
        let bus = SystemBus::new(DRAM_BASE, DRAM_SIZE);
        
        // Set hart count in CLINT so kernel can read it
        bus.set_num_harts(num_harts);

        // Load kernel (ELF or raw)
        let entry_pc = if kernel.starts_with(b"\x7FELF") {
            load_elf_native(kernel, &bus)?
        } else {
            bus.dram
                .load(kernel, 0)
                .map_err(|e| format!("Failed to load kernel: {:?}", e))?;
            DRAM_BASE
        };

        let bus = Arc::new(bus);
        let shared = Arc::new(SharedState::new());

        // Create primary CPU (hart 0)
        let primary_cpu = Some(Cpu::new(entry_pc, 0));

        println!("[VM] Created with {} harts, entry=0x{:x}", num_harts, entry_pc);

        Ok(Self {
            bus,
            handles: Vec::new(),
            primary_cpu,
            shared,
            num_harts,
            entry_pc,
        })
    }

    /// Create a VM with auto-detected hart count.
    /// Uses half the available CPU cores on the host.
    pub fn new_auto(kernel: &[u8]) -> Result<Self, String> {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let num_harts = (cpus / 2).max(1); // Use half the CPUs, ensure at least 1
        Self::new(kernel, num_harts)
    }

    /// Load a disk image and attach as VirtIO block device.
    pub fn load_disk(&mut self, disk: Vec<u8>) {
        use crate::virtio::VirtioBlock;

        // Need to get mutable access to bus - this requires unsafe or RefCell
        // For now, load disk before creating workers
        if let Some(bus) = Arc::get_mut(&mut self.bus) {
            let vblk = VirtioBlock::new(disk);
            bus.virtio_devices.push(Box::new(vblk));
            println!("[VM] Loaded disk image");
        } else {
            eprintln!("[VM] Cannot load disk: workers already running");
        }
    }

    /// Connect to a WebTransport relay for networking.
    ///
    /// Must be called before `run()` / `start_workers()`.
    /// The network backend is automatically wrapped in `AsyncNetworkBackend`
    /// for non-blocking I/O and better performance.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn connect_webtransport(&mut self, url: &str, cert_hash: Option<String>) {
        use crate::net_async::AsyncNetworkBackend;
        use crate::net_webtransport::WebTransportBackend;
        use crate::virtio::VirtioNet;

        if let Some(bus) = Arc::get_mut(&mut self.bus) {
            // Create the WebTransport backend
            let backend = WebTransportBackend::new(url, cert_hash);
            // Wrap in async backend for non-blocking I/O
            let async_backend = AsyncNetworkBackend::new(Box::new(backend));
            let vnet = VirtioNet::new(Box::new(async_backend));
            bus.virtio_devices.push(Box::new(vnet));
            println!("[VM] WebTransport network configured (async): {}", url);
        } else {
            eprintln!("[VM] Cannot configure network: workers already running");
        }
    }

    /// Get the number of harts.
    pub fn num_harts(&self) -> usize {
        self.num_harts
    }

    /// Get the kernel entry point.
    pub fn entry_pc(&self) -> u64 {
        self.entry_pc
    }

    /// Get a reference to the shared bus.
    pub fn bus(&self) -> &Arc<SystemBus> {
        &self.bus
    }

    /// Start worker threads for secondary harts.
    ///
    /// This spawns threads for harts 1, 2, ..., N-1.
    /// Hart 0 runs on the main thread (in `run()`).
    pub fn start_workers(&mut self) {
        for hart_id in 1..self.num_harts {
            let bus = Arc::clone(&self.bus);
            let shared = Arc::clone(&self.shared);
            let entry_pc = self.entry_pc;

            let handle = thread::Builder::new()
                .name(format!("hart-{}", hart_id))
                .spawn(move || {
                    hart_thread(hart_id, entry_pc, bus, shared);
                })
                .expect("Failed to spawn hart thread");

            self.handles.push(handle);
            println!("[VM] Started thread for hart {}", hart_id);
        }
    }

    /// Check if workers have been started.
    pub fn workers_started(&self) -> bool {
        !self.handles.is_empty() || self.num_harts == 1
    }

    /// Run the VM until halted.
    ///
    /// This runs hart 0 on the main thread while secondary harts
    /// run on worker threads.
    pub fn run(&mut self) {
        // Start worker threads if not already started
        if !self.workers_started() {
            self.start_workers();
        }

        let mut cpu = self.primary_cpu.take().expect("CPU already taken");
        let mut step_count: u64 = 0;
        let start_time = Instant::now();

        // Initialize console for non-blocking input
        let console = Console::new();
        let mut escaped = false;

        // Performance metrics
        let mut last_report_time = Instant::now();
        let mut last_report_steps: u64 = 0;
        let report_interval = Duration::from_secs(5);

        println!("[VM] Running hart 0 on main thread...");

        // Batch execution parameters
        // Execute instructions in batches to reduce loop overhead
        const BATCH_SIZE: u64 = 256;
        // VirtIO poll interval - with async backend, polling is non-blocking
        // so we can poll more frequently for better network latency
        const VIRTIO_POLL_INTERVAL: u64 = 4096;
        // Console poll interval - console I/O has more overhead
        const CONSOLE_POLL_INTERVAL: u64 = 16384;

        loop {
            // Check halt before batch
            if self.shared.should_stop() {
                break;
            }

            // Execute a batch of instructions
            let (batch_steps, halt_reason) = self.execute_batch(&mut cpu, BATCH_SIZE);
            step_count += batch_steps;

            // Handle halt reason from batch
            if let Some(reason) = halt_reason {
                match reason {
                    HaltReason::Shutdown(code) => {
                        println!("[VM] Shutdown requested (code: {:#x})", code);
                        self.shared.signal_halted(code);
                        break;
                    }
                    HaltReason::Fatal(msg, pc) => {
                        eprintln!("[VM] Fatal error: {} at PC=0x{:x}", msg, pc);
                        self.shared.signal_halted(0xDEAD);
                        break;
                    }
                }
            }

            // Poll VirtIO more frequently (non-blocking with async backend)
            if step_count % VIRTIO_POLL_INTERVAL == 0 {
                self.bus.poll_virtio();
            }

            // Poll console less frequently (has more overhead)
            if step_count % CONSOLE_POLL_INTERVAL == 0 {
                self.pump_console(&console, &mut escaped);

                // Periodic performance report (debug mode)
                if log::log_enabled!(log::Level::Debug) {
                    let now = Instant::now();
                    if now.duration_since(last_report_time) >= report_interval {
                        let delta_steps = step_count - last_report_steps;
                        let delta_time = now.duration_since(last_report_time).as_secs_f64();
                        let current_ips = if delta_time > 0.0 {
                            delta_steps as f64 / delta_time
                        } else {
                            0.0
                        };
                        log::debug!(
                            "[Hart 0] {} steps, {:.2}M IPS (current), PC=0x{:x}",
                            step_count,
                            current_ips / 1_000_000.0,
                            cpu.pc
                        );
                        last_report_time = now;
                        last_report_steps = step_count;
                    }
                }
            }
        }

        // Clean up
        self.shutdown();

        // Report statistics
        let elapsed = start_time.elapsed().as_secs_f64();
        let ips = if elapsed > 0.0 {
            step_count as f64 / elapsed
        } else {
            0.0
        };
        println!(
            "[VM] Hart 0 halted after {} steps ({:.2}M IPS)",
            step_count,
            ips / 1_000_000.0
        );
    }

    /// Execute a batch of instructions.
    ///
    /// Returns the number of steps executed and an optional halt reason.
    /// This reduces per-instruction overhead by checking conditions less frequently.
    fn execute_batch(&self, cpu: &mut Cpu, max_steps: u64) -> (u64, Option<HaltReason>) {
        let mut count = 0u64;

        for _ in 0..max_steps {
            match cpu.step(&*self.bus) {
                Ok(()) => {
                    count += 1;
                }
                Err(Trap::RequestedTrap(code)) => {
                    return (count, Some(HaltReason::Shutdown(code)));
                }
                Err(Trap::Fatal(msg)) => {
                    return (count, Some(HaltReason::Fatal(msg, cpu.pc)));
                }
                Err(_trap) => {
                    // Architectural traps handled by CPU
                    count += 1;
                }
            }
        }

        (count, None)
    }

    /// Pump console I/O.
    /// 
    /// Handles UART output to stdout and console input to UART.
    /// Supports Ctrl-A escape sequences:
    /// - Ctrl-A x: terminate VM
    /// - Ctrl-A Ctrl-A: send literal Ctrl-A
    fn pump_console(&self, console: &Console, escaped: &mut bool) {
        // Output to stdout - drain all at once to reduce lock contention
        let output = self.bus.uart.drain_output();
        if !output.is_empty() {
            for byte in output {
                // In raw terminal mode, \n alone doesn't return cursor to column 0.
                // We need to emit \r\n for proper line breaks.
                if byte == b'\n' {
                    print!("\r\n");
                } else {
                    print!("{}", byte as char);
                }
            }
            io::stdout().flush().ok();
        }

        // Input from console (non-blocking)
        for byte in console.read_available() {
            if *escaped {
                if byte == b'x' {
                    // Ctrl-A x: terminate
                    println!("\r\n[VM] Terminated by user (Ctrl-A x)");
                    self.shared.request_halt();
                    return;
                } else if byte == 1 {
                    // Ctrl-A Ctrl-A: send literal Ctrl-A
                    self.bus.uart.push_input(1);
                } else {
                    // Ctrl-A then something else: send that something
                    self.bus.uart.push_input(byte);
                }
                *escaped = false;
            } else {
                if byte == 1 {
                    // Ctrl-A: start escape sequence
                    *escaped = true;
                } else {
                    self.bus.uart.push_input(byte);
                }
            }
        }
    }

    /// Request shutdown and wait for workers.
    fn shutdown(&mut self) {
        println!("[VM] Shutting down...");

        // Signal halt to all workers
        self.shared.request_halt();

        // Wait for all workers to exit
        for handle in self.handles.drain(..) {
            if let Err(e) = handle.join() {
                eprintln!("[VM] Worker thread panicked: {:?}", e);
            }
        }

        println!("[VM] All threads stopped");
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for NativeVm {
    fn drop(&mut self) {
        // Ensure threads are cleaned up
        self.shared.request_halt();
        for handle in self.handles.drain(..) {
            handle.join().ok();
        }
    }
}

/// Load ELF kernel into DRAM (for NativeVm).
///
/// Takes a shared reference to the bus since SystemBus uses interior
/// mutability for DRAM access.
#[cfg(not(target_arch = "wasm32"))]
fn load_elf_native(buffer: &[u8], bus: &SystemBus) -> Result<u64, String> {
    let elf = Elf::parse(buffer).map_err(|e| format!("ELF parse error: {}", e))?;
    let base = bus.dram.base;
    let dram_size = bus.dram.size();
    let dram_end = base + dram_size as u64;

    for ph in &elf.program_headers {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }

        let file_size = ph.p_filesz as usize;
        let mem_size = ph.p_memsz as usize;
        let file_offset = ph.p_offset as usize;

        if file_offset + file_size > buffer.len() {
            return Err("Segment exceeds file bounds".to_string());
        }

        let target_addr = if ph.p_paddr != 0 {
            ph.p_paddr
        } else {
            ph.p_vaddr
        };

        if target_addr < base || target_addr + mem_size as u64 > dram_end {
            return Err(format!("Segment 0x{:x} out of DRAM range", target_addr));
        }

        let dram_offset = target_addr - base;

        if file_size > 0 {
            bus.dram
                .load(&buffer[file_offset..file_offset + file_size], dram_offset)
                .map_err(|e| format!("Failed to load segment: {:?}", e))?;
        }

        if mem_size > file_size {
            bus.dram
                .zero_range((dram_offset as usize) + file_size, mem_size - file_size)
                .map_err(|e| format!("Failed to zero BSS: {:?}", e))?;
        }
    }

    Ok(elf.entry)
}

/// Hart thread entry point.
///
/// This function runs on a dedicated thread for each secondary hart.
/// It executes CPU instructions until halted.
#[cfg(not(target_arch = "wasm32"))]
fn hart_thread(hart_id: usize, entry_pc: u64, bus: Arc<SystemBus>, shared: Arc<SharedState>) {
    let mut cpu = Cpu::new(entry_pc, hart_id as u64);
    let mut step_count: u64 = 0;
    let start_time = Instant::now();

    // Performance metrics
    let mut last_report_time = Instant::now();
    let mut last_report_steps: u64 = 0;
    let report_interval = Duration::from_secs(5);

    println!("[Hart {}] Started at PC=0x{:x}", hart_id, entry_pc);

    // Batch execution parameters
    const BATCH_SIZE: u64 = 256;
    // Yield interval - allow other threads to run
    // Higher value = better throughput but potentially less fair scheduling
    const YIELD_INTERVAL: u64 = 4_000_000;

    loop {
        // Check halt before batch
        if shared.should_stop() {
            break;
        }

        // Execute a batch of instructions
        let (batch_steps, halt_reason) = execute_batch_worker(&mut cpu, &bus, BATCH_SIZE);
        step_count += batch_steps;

        // Handle halt reason from batch
        if let Some(reason) = halt_reason {
            match reason {
                HaltReason::Shutdown(code) => {
                    println!(
                        "[Hart {}] Shutdown requested (code: {:#x})",
                        hart_id, code
                    );
                    shared.signal_halted(code);
                    break;
                }
                HaltReason::Fatal(msg, pc) => {
                    eprintln!("[Hart {}] Fatal: {} at PC=0x{:x}", hart_id, msg, pc);
                    shared.signal_halted(0xDEAD);
                    break;
                }
            }
        }

        // Yield occasionally to prevent CPU hogging and reduce contention
        if step_count % YIELD_INTERVAL == 0 {
            thread::yield_now();

            // Periodic performance report (debug mode)
            if log::log_enabled!(log::Level::Debug) {
                let now = Instant::now();
                if now.duration_since(last_report_time) >= report_interval {
                    let delta_steps = step_count - last_report_steps;
                    let delta_time = now.duration_since(last_report_time).as_secs_f64();
                    let current_ips = if delta_time > 0.0 {
                        delta_steps as f64 / delta_time
                    } else {
                        0.0
                    };
                    log::debug!(
                        "[Hart {}] {} steps, {:.2}M IPS (current), PC=0x{:x}",
                        hart_id,
                        step_count,
                        current_ips / 1_000_000.0,
                        cpu.pc
                    );
                    last_report_time = now;
                    last_report_steps = step_count;
                }
            }
        }
    }

    // Report statistics
    let elapsed = start_time.elapsed().as_secs_f64();
    let ips = if elapsed > 0.0 {
        step_count as f64 / elapsed
    } else {
        0.0
    };
    println!(
        "[Hart {}] Exited after {} steps ({:.2}M IPS)",
        hart_id,
        step_count,
        ips / 1_000_000.0
    );
}

/// Execute a batch of instructions for worker threads.
///
/// Similar to NativeVm::execute_batch but takes bus reference directly.
#[cfg(not(target_arch = "wasm32"))]
fn execute_batch_worker(cpu: &mut Cpu, bus: &SystemBus, max_steps: u64) -> (u64, Option<HaltReason>) {
    let mut count = 0u64;

    for _ in 0..max_steps {
        match cpu.step(bus) {
            Ok(()) => {
                count += 1;
            }
            Err(Trap::RequestedTrap(code)) => {
                return (count, Some(HaltReason::Shutdown(code)));
            }
            Err(Trap::Fatal(msg)) => {
                return (count, Some(HaltReason::Fatal(msg, cpu.pc)));
            }
            Err(_trap) => {
                // Architectural traps handled by CPU
                count += 1;
            }
        }
    }

    (count, None)
}

