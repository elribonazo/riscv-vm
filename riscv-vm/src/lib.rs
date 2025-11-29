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
    halted: bool,
    halt_code: u64,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmVm {
    /// Create a new VM instance and load a kernel (ELF or raw binary).
    #[wasm_bindgen(constructor)]
    pub fn new(kernel: &[u8]) -> Result<WasmVm, JsValue> {
        // Set up panic hook for better error messages in the browser console
        console_error_panic_hook::set_once();
        
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
            &format!("[VM] Creating new VM, kernel size: {} bytes", kernel.len())));

        const DRAM_SIZE: usize = 512 * 1024 * 1024; // 512 MiB
        let mut bus = SystemBus::new(DRAM_BASE, DRAM_SIZE);
        
        // Check if it's an ELF file and load appropriately
        let entry_pc = if kernel.starts_with(b"\x7FELF") {
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("[VM] Detected ELF kernel"));
            // Parse and load ELF
            let entry = load_elf_wasm(kernel, &mut bus)
                .map_err(|e| JsValue::from_str(&format!("Failed to load ELF kernel: {}", e)))?;
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                &format!("[VM] ELF loaded, entry point: 0x{:x}", entry)));
            entry
        } else {
            // Not an ELF - this is likely an error (HTML error page, corrupted file, etc.)
            web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(
                &format!("[VM] Warning: kernel does not appear to be an ELF file (magic: {:02x?}). Loading as raw binary.",
                    &kernel[..kernel.len().min(4)])));
            // Load raw binary at DRAM_BASE
            bus.dram
                .load(kernel, 0)
                .map_err(|e| JsValue::from_str(&format!("Failed to load kernel: {}", e)))?;
            DRAM_BASE
        };

        let cpu = cpu::Cpu::new(entry_pc);
        
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
            &format!("[VM] CPU initialized, starting at PC=0x{:x}", entry_pc)));

        Ok(WasmVm { 
            bus, 
            cpu, 
            net_status: NetworkStatus::Disconnected,
            poll_counter: 0,
            halted: false,
            halt_code: 0,
        })
    }

    /// Load a disk image and attach it as a VirtIO block device.
    /// This should be called before starting execution if the kernel needs a filesystem.
    pub fn load_disk(&mut self, disk_image: &[u8]) {
        let vblk = virtio::VirtioBlock::new(disk_image.to_vec());
        self.bus.virtio_devices.push(Box::new(vblk));
    }
    
    /// Connect to a WebTransport relay server.
    /// Note: Connection is asynchronous. Check network_status() to monitor connection state.
    pub fn connect_webtransport(&mut self, url: &str, cert_hash: Option<String>) -> Result<(), JsValue> {
        use crate::net_webtransport::WebTransportBackend;
        use crate::virtio::VirtioNet;

        // Status stays as Connecting until we can verify the connection is established
        // (when IP is assigned, the connection is confirmed)
        self.net_status = NetworkStatus::Connecting;

        let backend = WebTransportBackend::new(url, cert_hash);
        // Note: WebTransport connect is async, so backend.init() will start connection
        // but actual connection happens in background.
        let mut vnet = VirtioNet::new(Box::new(backend));
        vnet.debug = false;

        self.bus.virtio_devices.push(Box::new(vnet));
        // Don't set to Connected here - let network_status() check the actual state

        Ok(())
    }

    /// Disconnect from the network.
    pub fn disconnect_network(&mut self) {
        // Remove VirtioNet devices (device_id == 1)
        self.bus.virtio_devices.retain(|dev| dev.device_id() != 1);
        self.net_status = NetworkStatus::Disconnected;
    }
    
    /// Get the current network connection status.
    /// This checks the actual connection state by seeing if an IP was assigned.
    pub fn network_status(&mut self) -> NetworkStatus {
        // If we think we're connecting, check if we've actually connected
        // by seeing if we can read an assigned IP from the VirtIO config space
        if self.net_status == NetworkStatus::Connecting {
            // Look for a VirtioNet device (device_id == 1) and check if IP is assigned
            for (idx, device) in self.bus.virtio_devices.iter_mut().enumerate() {
                let dev_id = device.device_id();
                if dev_id == 1 {
                    // Read config space offset 8 (IP address)
                    // IP is at config offset 0x108 - 0x100 = 8
                    if let Ok(ip_val) = device.read(0x108) {
                        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                            &format!("[network_status] Device idx={} id={} read(0x108)={:#x}", 
                                idx, dev_id, ip_val)));
                        if ip_val != 0 {
                            self.net_status = NetworkStatus::Connected;
                            break;
                        }
                    }
                }
            }
        }
        self.net_status
    }

    /// Execute a single instruction.
    /// Returns true if the VM is still running, false if halted.
    pub fn step(&mut self) -> bool {
        // If already halted, don't execute more instructions
        if self.halted {
            return false;
        }
        
        // Poll VirtIO devices periodically for incoming network packets
        // Poll every 100 instructions for good network responsiveness
        self.poll_counter = self.poll_counter.wrapping_add(1);
        if self.poll_counter % 100 == 0 {
            self.bus.poll_virtio();
        }
        
        // Execute instruction and check for halt condition
        match self.cpu.step(&mut self.bus) {
            Ok(()) => true,
            Err(Trap::RequestedTrap(code)) => {
                // Guest requested shutdown via TEST_FINISHER
                self.halted = true;
                self.halt_code = code;
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                    &format!("[VM] Halted by guest request (code: {:#x})", code)));
                false
            }
            Err(Trap::Fatal(msg)) => {
                // Fatal emulator error - log and halt
                web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(
                    &format!("[VM] Fatal error: {} at PC=0x{:x}", msg, self.cpu.pc)));
                self.halted = true;
                false
            }
            Err(_trap) => {
                // Architectural traps (interrupts, page faults, ecalls) are handled
                // by the CPU's trap handler which updates CSRs and redirects PC.
                // These are normal operation - continue execution.
                true
            }
        }
    }

    /// Execute up to N instructions in a batch.
    /// Returns the number of instructions actually executed.
    /// This is more efficient than calling step() N times due to reduced
    /// JS-WASM boundary crossings.
    pub fn step_n(&mut self, count: u32) -> u32 {
        for i in 0..count {
            if !self.step() {
                return i;
            }
        }
        count
    }
    
    /// Check if the VM has halted (e.g., due to shutdown command).
    pub fn is_halted(&self) -> bool {
        self.halted
    }
    
    /// Get the halt code if the VM has halted.
    /// Code 0x5555 typically means successful shutdown (PASS).
    pub fn halt_code(&self) -> u64 {
        self.halt_code
    }

    /// Get a byte from the UART output buffer, if available.
    pub fn get_output(&mut self) -> Option<u8> {
        let byte = self.bus.uart.pop_output();
        // Uncomment for debugging UART output:
        // if let Some(b) = byte {
        //     web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
        //         &format!("[UART] Output: {:02x} '{}'", b, if b.is_ascii_graphic() { b as char } else { '.' })));
        // }
        byte
    }
    
    /// Check how many bytes are pending in the UART output buffer.
    /// Useful for debugging output issues.
    pub fn uart_output_pending(&self) -> usize {
        self.bus.uart.output.len()
    }
    
    /// Write a string to the UART output buffer (VM host message).
    /// This allows the VM to emit its own messages that the browser can display
    /// alongside kernel output.
    fn emit_to_uart(&mut self, s: &str) {
        for byte in s.bytes() {
            self.bus.uart.output.push_back(byte);
        }
    }
    
    /// Print the VM banner to UART output (visible in browser).
    /// Call this after creating the VM to show a boot banner.
    pub fn print_banner(&mut self) {
        const BANNER: &str = "\x1b[1;36m\
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│   ██████╗  █████╗ ██╗   ██╗██╗   ██╗    ██╗   ██╗███╗   ███╗            │
│   ██╔══██╗██╔══██╗██║   ██║╚██╗ ██╔╝    ██║   ██║████╗ ████║            │
│   ██████╔╝███████║██║   ██║ ╚████╔╝     ██║   ██║██╔████╔██║            │
│   ██╔══██╗██╔══██║╚██╗ ██╔╝  ╚██╔╝      ╚██╗ ██╔╝██║╚██╔╝██║            │
│   ██████╔╝██║  ██║ ╚████╔╝    ██║        ╚████╔╝ ██║ ╚═╝ ██║            │
│   ╚═════╝ ╚═╝  ╚═╝  ╚═══╝     ╚═╝         ╚═══╝  ╚═╝     ╚═╝            │
│                                                                         │
│   \x1b[1;97mBavy Virtual Machine v0.1.0\x1b[1;36m                                          │
│   \x1b[0;90m64-bit RISC-V Emulator with VirtIO Support\x1b[1;36m                           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
\x1b[0m\n";
        self.emit_to_uart(BANNER);
    }
    
    /// Print a status message to UART output (visible in browser).
    pub fn print_status(&mut self, message: &str) {
        self.emit_to_uart("\x1b[1;33m[VM]\x1b[0m ");
        self.emit_to_uart(message);
        self.emit_to_uart("\n");
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
