use crate::bus::{SystemBus, DRAM_BASE};
use crate::cpu::Cpu;
use crate::Trap;
use goblin::elf::{program_header::PT_LOAD, Elf};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

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
        let cpu = Cpu::new(dram_base);

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
        match self.cpu.step(&mut self.bus) {
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
        Ok(self.bus.dram.data[offset..end].to_vec())
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
    pub msip: [u32; crate::clint::MAX_HARTS],
    pub mtime: u64,
    pub mtimecmp: [u64; crate::clint::MAX_HARTS],
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
            msip: self.bus.clint.msip,
            mtime: self.bus.clint.mtime,
            mtimecmp: self.bus.clint.mtimecmp,
        };

        let plic = PlicSnapshot {
            priority: self.bus.plic.priority.to_vec(),
            pending: self.bus.plic.pending,
            enable: self.bus.plic.enable.to_vec(),
            threshold: self.bus.plic.threshold.to_vec(),
            active: self.bus.plic.active.to_vec(),
        };

        let uart = UartSnapshot {
            rx_fifo: self.bus.uart.input.iter().copied().collect(),
            tx_fifo: self.bus.uart.output.iter().copied().collect(),
            ier: self.bus.uart.ier,
            iir: self.bus.uart.iir,
            fcr: self.bus.uart.fcr,
            lcr: self.bus.uart.lcr,
            mcr: self.bus.uart.mcr,
            lsr: self.bus.uart.lsr,
            msr: self.bus.uart.msr,
            scr: self.bus.uart.scr,
            dll: self.bus.uart.dll,
            dlm: self.bus.uart.dlm,
        };

        let mut hasher = Sha256::new();
        hasher.update(&self.bus.dram.data);
        let hash = hex::encode(hasher.finalize());

        let region = MemRegionSnapshot {
            base: self.bus.dram.base,
            size: self.bus.dram.data.len() as u64,
            hash,
            data: Some(self.bus.dram.data.clone()),
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
        self.bus.clint.msip = snapshot.devices.clint.msip;
        self.bus.clint.set_mtime(snapshot.devices.clint.mtime);
        self.bus.clint.mtimecmp = snapshot.devices.clint.mtimecmp;

        // Restore PLIC (truncate if snapshot has more sources/contexts).
        for (i, &val) in snapshot.devices.plic.priority.iter().enumerate() {
            if i < self.bus.plic.priority.len() {
                self.bus.plic.priority[i] = val;
            }
        }
        self.bus.plic.pending = snapshot.devices.plic.pending;
        for (i, &val) in snapshot.devices.plic.enable.iter().enumerate() {
            if i < self.bus.plic.enable.len() {
                self.bus.plic.enable[i] = val;
            }
        }
        for (i, &val) in snapshot.devices.plic.threshold.iter().enumerate() {
            if i < self.bus.plic.threshold.len() {
                self.bus.plic.threshold[i] = val;
            }
        }
        for (i, &val) in snapshot.devices.plic.active.iter().enumerate() {
            if i < self.bus.plic.active.len() {
                self.bus.plic.active[i] = val;
            }
        }

        // Restore UART.
        self.bus.uart.input.clear();
        self.bus.uart.input.extend(snapshot.devices.uart.rx_fifo.iter().copied());
        self.bus.uart.output.clear();
        self.bus.uart.output.extend(snapshot.devices.uart.tx_fifo.iter().copied());
        self.bus.uart.ier = snapshot.devices.uart.ier;
        self.bus.uart.iir = snapshot.devices.uart.iir;
        self.bus.uart.fcr = snapshot.devices.uart.fcr;
        self.bus.uart.lcr = snapshot.devices.uart.lcr;
        self.bus.uart.mcr = snapshot.devices.uart.mcr;
        self.bus.uart.lsr = snapshot.devices.uart.lsr;
        self.bus.uart.msr = snapshot.devices.uart.msr;
        self.bus.uart.scr = snapshot.devices.uart.scr;
        self.bus.uart.dll = snapshot.devices.uart.dll;
        self.bus.uart.dlm = snapshot.devices.uart.dlm;
        self.bus.uart.update_interrupts();

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
        if self.bus.dram.data.len() != data.len() {
            return Err(format!(
                "snapshot DRAM size mismatch: emulator={} bytes, snapshot={} bytes",
                self.bus.dram.data.len(),
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

        self.bus.dram.data.clone_from_slice(data);

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

    #[test]
    fn snapshot_roundtrip_preserves_state() {
        let mut emu = Emulator::with_memory(1024 * 1024);

        // Simple CPU state.
        emu.cpu.pc = DRAM_BASE + 0x1000;
        emu.cpu.write_reg(crate::decoder::Register::X5, 0xdead_beef_dead_beef);

        // Touch DRAM and devices.
        let addr = emu.bus.dram_base() + 0x80;
        emu.bus.write64(addr, 0x0123_4567_89ab_cdef).unwrap();
        emu.bus.clint.mtime = 1234;
        emu.bus.clint.mtimecmp[0] = 5678;
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
        assert_eq!(emu.bus.dram.data, emu2.bus.dram.data);
        assert_eq!(emu.bus.clint.mtime, emu2.bus.clint.mtime);
        assert_eq!(emu.bus.clint.mtimecmp, emu2.bus.clint.mtimecmp);
        assert_eq!(emu.bus.uart.input, emu2.bus.uart.input);
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


