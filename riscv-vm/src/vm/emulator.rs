use crate::Trap;
use crate::bus::{DRAM_BASE, SystemBus};
use crate::cpu::Cpu;
use crate::snapshot::{
    ClintSnapshot, CpuSnapshot, DeviceSnapshot, MemRegionSnapshot, PlicSnapshot, SNAPSHOT_VERSION,
    Snapshot, UartSnapshot,
};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

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
        let cpu = Cpu::new(dram_base, 0); // hart_id = 0

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
    pub fn load_elf<P: AsRef<Path>>(&mut self, path: P) -> Result<u64, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        #[cfg(not(target_arch = "wasm32"))]
        let entry_pc = crate::loader::load_elf_into_dram(&buffer, &self.bus)?;

        #[cfg(target_arch = "wasm32")]
        let entry_pc = crate::loader::load_elf_wasm(&buffer, &self.bus)?;

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

        self.bus
            .dram
            .read_range(offset, self.signature_size as usize)
            .map_err(|e| format!("failed to read signature: {}", e))
    }

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
        self.bus
            .clint
            .set_mtimecmp_array(&snapshot.devices.clint.mtimecmp);

        // Restore PLIC.
        self.bus.plic.set_priority(&snapshot.devices.plic.priority);
        self.bus.plic.set_pending(snapshot.devices.plic.pending);
        self.bus.plic.set_enable(&snapshot.devices.plic.enable);
        self.bus
            .plic
            .set_threshold(&snapshot.devices.plic.threshold);
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

        self.bus
            .dram
            .set_data(data)
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
    use crate::engine::decoder::Register;

    #[test]
    fn snapshot_roundtrip_preserves_state() {
        let mut emu = Emulator::with_memory(1024 * 1024);

        // Simple CPU state.
        emu.cpu.pc = DRAM_BASE + 0x1000;
        emu.cpu.write_reg(Register::X5, 0xdead_beef_dead_beef);

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
            emu.cpu.read_reg(Register::X5),
            emu2.cpu.read_reg(Register::X5)
        );
        assert_eq!(emu.bus.dram.get_data(), emu2.bus.dram.get_data());
        assert_eq!(emu.bus.clint.mtime(), emu2.bus.clint.mtime());
        assert_eq!(
            emu.bus.clint.get_mtimecmp_array(),
            emu2.bus.clint.get_mtimecmp_array()
        );
        assert_eq!(emu.bus.uart.get_input(), emu2.bus.uart.get_input());
    }
}
