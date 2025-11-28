pub mod bus;
pub mod cpu;
pub mod decoder;
pub mod csr;
pub mod mmu;
pub mod dram;
pub mod clint;
pub mod plic;
pub mod uart;
pub mod net;
pub mod net_webtransport;
pub mod virtio;
pub mod emulator;

#[cfg(not(target_arch = "wasm32"))]
pub mod console;


use serde::{Deserialize, Serialize};

// WASM bindings
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
use crate::bus::{SystemBus, DRAM_BASE};

/// Network connection status for the WASM VM.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NetworkStatus {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
    Error = 3,
}

/// WASM-exposed VM wrapper for running RISC-V kernels in the browser.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct WasmVm {
    bus: SystemBus,
    cpu: cpu::Cpu,
    net_status: NetworkStatus,
    poll_counter: u32,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmVm {
    /// Create a new VM instance and load a kernel (ELF or raw binary).
    #[wasm_bindgen(constructor)]
    pub fn new(kernel: &[u8]) -> Result<WasmVm, JsValue> {
        // Set up panic hook for better error messages in the browser console
        console_error_panic_hook::set_once();

        const DRAM_SIZE: usize = 512 * 1024 * 1024; // 512 MiB
        let mut bus = SystemBus::new(DRAM_BASE, DRAM_SIZE);
        
        // Check if it's an ELF file and load appropriately
        let entry_pc = if kernel.starts_with(b"\x7FELF") {
            // Parse and load ELF
            load_elf_wasm(kernel, &mut bus)
                .map_err(|e| JsValue::from_str(&format!("Failed to load ELF kernel: {}", e)))?
        } else {
            // Load raw binary at DRAM_BASE
            bus.dram
                .load(kernel, 0)
                .map_err(|e| JsValue::from_str(&format!("Failed to load kernel: {}", e)))?;
            DRAM_BASE
        };

        let cpu = cpu::Cpu::new(entry_pc);

        Ok(WasmVm { 
            bus, 
            cpu, 
            net_status: NetworkStatus::Disconnected,
            poll_counter: 0,
        })
    }

    /// Load a disk image and attach it as a VirtIO block device.
    /// This should be called before starting execution if the kernel needs a filesystem.
    pub fn load_disk(&mut self, disk_image: &[u8]) {
        let vblk = virtio::VirtioBlock::new(disk_image.to_vec());
        self.bus.virtio_devices.push(Box::new(vblk));
    }
    
    /// Connect to a WebTransport relay server.
    pub fn connect_webtransport(&mut self, url: &str, cert_hash: Option<String>) -> Result<(), JsValue> {
        use crate::net_webtransport::WebTransportBackend;
        use crate::virtio::VirtioNet;

        self.net_status = NetworkStatus::Connecting;

        let backend = WebTransportBackend::new(url, cert_hash);
        // Note: WebTransport connect is async, so backend.init() will start connection
        // but actual connection happens in background.
        let mut vnet = VirtioNet::new(Box::new(backend));
        vnet.debug = false;

        self.bus.virtio_devices.push(Box::new(vnet));
        self.net_status = NetworkStatus::Connected;

        Ok(())
    }

    /// Disconnect from the network.
    pub fn disconnect_network(&mut self) {
        // Remove VirtioNet devices (device_id == 1)
        self.bus.virtio_devices.retain(|dev| dev.device_id() != 1);
        self.net_status = NetworkStatus::Disconnected;
    }
    
    /// Get the current network connection status.
    pub fn network_status(&self) -> NetworkStatus {
        self.net_status
    }

    /// Execute a single instruction.
    pub fn step(&mut self) {
        // Poll VirtIO devices periodically for incoming network packets
        // Poll every 100 instructions for good network responsiveness
        self.poll_counter = self.poll_counter.wrapping_add(1);
        if self.poll_counter % 100 == 0 {
            self.bus.poll_virtio();
        }
        
        // Ignore traps for now - the kernel handles them
        let _ = self.cpu.step(&mut self.bus);
    }

    /// Get a byte from the UART output buffer, if available.
    pub fn get_output(&mut self) -> Option<u8> {
        self.bus.uart.pop_output()
    }

    /// Push an input byte to the UART.
    pub fn input(&mut self, byte: u8) {
        self.bus.uart.push_input(byte);
    }

    /// Get current memory usage (DRAM size) in bytes.
    pub fn get_memory_usage(&self) -> u64 {
        self.bus.dram.data.len() as u64
    }
}

/// Load an ELF kernel into DRAM (WASM-compatible version).
#[cfg(target_arch = "wasm32")]
fn load_elf_wasm(buffer: &[u8], bus: &mut SystemBus) -> Result<u64, String> {
    use goblin::elf::{program_header::PT_LOAD, Elf};
    
    let elf = Elf::parse(buffer).map_err(|e| format!("ELF parse error: {}", e))?;
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
            ));
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
            ));
        }
        let seg_end = target_addr
            .checked_add(mem_size as u64)
            .ok_or_else(|| "Segment end overflow".to_string())?;
        if seg_end > dram_end {
            return Err(format!(
                "Segment 0x{:x}-0x{:x} exceeds DRAM (end 0x{:x})",
                target_addr, seg_end, dram_end
            ));
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
    }

    Ok(elf.entry)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trap {
    InstructionAddressMisaligned(u64),
    InstructionAccessFault(u64),
    IllegalInstruction(u64),
    Breakpoint,
    LoadAddressMisaligned(u64),
    LoadAccessFault(u64),
    StoreAddressMisaligned(u64),
    StoreAccessFault(u64),
    EnvironmentCallFromU,
    EnvironmentCallFromS,
    EnvironmentCallFromM,
    InstructionPageFault(u64),
    LoadPageFault(u64),
    StorePageFault(u64),
    
    MachineSoftwareInterrupt,
    MachineTimerInterrupt,
    MachineExternalInterrupt,
    SupervisorSoftwareInterrupt,
    SupervisorTimerInterrupt,
    SupervisorExternalInterrupt,

    // Custom internal errors
    RequestedTrap(u64), // For testing (software interrupts, etc)
    Fatal(String),
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Trap {}
