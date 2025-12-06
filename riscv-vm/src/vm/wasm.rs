use crate::Trap;
use crate::bus::{DRAM_BASE, SystemBus};
use crate::cpu;
use crate::loader::load_elf_wasm;
use crate::shared_mem;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

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

// ============================================================================
// Hart Count Detection
// ============================================================================

/// Detect the number of hardware threads available.
/// Limits to half the available CPU cores to leave resources for the host.
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)] // Used in WASM builds only
fn detect_hart_count() -> usize {
    // In browsers, navigator.hardwareConcurrency gives logical CPU count
    let count = web_sys::window()
        .and_then(|w| Some(w.navigator().hardware_concurrency() as usize))
        .unwrap_or(2);

    (count / 2).max(1) // Use half the CPUs, ensure at least 1
}

/// Check if SharedArrayBuffer is available for multi-threaded execution.
#[cfg(target_arch = "wasm32")]
fn check_shared_array_buffer_available() -> bool {
    // SharedArrayBuffer requires cross-origin isolation (COOP/COEP headers)

    // Check if we're in a browser context
    if let Some(window) = web_sys::window() {
        // Check crossOriginIsolated property
        let isolated: bool =
            js_sys::Reflect::get(&window, &JsValue::from_str("crossOriginIsolated"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

        if !isolated {
            web_sys::console::warn_1(&JsValue::from_str(
                "[VM] Not cross-origin isolated. Add COOP/COEP headers for SMP support.",
            ));
            return false;
        }

        web_sys::console::log_1(&JsValue::from_str(
            "[VM] Cross-origin isolated - SharedArrayBuffer should be available",
        ));
    }

    // If cross-origin isolated, SharedArrayBuffer should work
    // Note: catch_unwind doesn't work in WASM, so we trust the isolation check
    true
}

#[cfg(not(target_arch = "wasm32"))]
fn detect_hart_count() -> usize {
    let count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (count / 2).max(1) // Use half the CPUs, ensure at least 1
}

/// WASM-exposed VM wrapper for running RISC-V kernels in the browser.
///
/// ## Multi-Hart Architecture
///
/// When `SharedArrayBuffer` is available (requires COOP/COEP headers):
/// - Hart 0 runs on the main thread (handles I/O devices)
/// - Harts 1+ run on Web Workers (parallel execution)
/// - DRAM and CLINT are shared via SharedArrayBuffer
/// - Workers are managed automatically
///
/// When `SharedArrayBuffer` is NOT available:
/// - Falls back to single-threaded round-robin execution
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct WasmVm {
    bus: SystemBus,
    cpu: cpu::Cpu,    // Primary CPU (hart 0)
    num_harts: usize, // Total hart count
    net_status: NetworkStatus,
    poll_counter: u32,
    halted: bool,
    halt_code: u64,
    /// Shared memory buffer (for passing to workers)
    shared_buffer: Option<js_sys::SharedArrayBuffer>,
    /// Shared control region accessor
    shared_control: Option<shared_mem::wasm::SharedControl>,
    /// Shared CLINT accessor
    shared_clint: Option<shared_mem::wasm::SharedClint>,
    /// Shared UART output accessor (for reading worker output)
    shared_uart_output: Option<shared_mem::wasm::SharedUartOutput>,
    /// Shared UART input accessor (for sending keyboard input to workers)
    shared_uart_input: Option<shared_mem::wasm::SharedUartInput>,
    /// Worker handles
    workers: Vec<web_sys::Worker>,
    /// Worker ready flags
    workers_ready: Vec<bool>,
    /// Whether workers have been started
    workers_started: bool,
    /// Entry PC for workers
    entry_pc: u64,
    /// Boot step counter - used to delay worker start
    boot_steps: u64,
    /// Whether workers have been signaled to start
    workers_signaled: bool,
    /// External network backend for Node.js native addon bridging
    external_net: Option<Arc<crate::net::external::ExternalNetworkBackend>>,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmVm {
    /// Create a new VM instance and load a kernel (ELF or raw binary).
    ///
    /// If SharedArrayBuffer is available, the VM will use true parallel
    /// execution with Web Workers. Otherwise, falls back to single-threaded mode.
    ///
    /// Hart count is auto-detected as half of hardware_concurrency.
    /// Use `new_with_harts()` to specify a custom hart count.
    #[wasm_bindgen(constructor)]
    pub fn new(kernel: &[u8]) -> Result<WasmVm, JsValue> {
        Self::create_vm_internal(kernel, None)
    }

    /// Create a new VM instance with a specified number of harts.
    ///
    /// # Arguments
    /// * `kernel` - ELF kernel binary
    /// * `num_harts` - Number of harts (0 = auto-detect)
    pub fn new_with_harts(kernel: &[u8], num_harts: usize) -> Result<WasmVm, JsValue> {
        let harts = if num_harts == 0 {
            None
        } else {
            Some(num_harts)
        };
        Self::create_vm_internal(kernel, harts)
    }

    /// Internal constructor with optional hart count.
    fn create_vm_internal(kernel: &[u8], num_harts: Option<usize>) -> Result<WasmVm, JsValue> {
        // Set up panic hook for better error messages in the browser console
        console_error_panic_hook::set_once();

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
            "[VM] Creating new VM, kernel size: {} bytes",
            kernel.len()
        )));

        const DRAM_SIZE: usize = 512 * 1024 * 1024; // 512 MiB

        // Detect or use specified hart count
        let num_harts = num_harts.unwrap_or_else(detect_hart_count);

        // Check if SharedArrayBuffer is available for true parallelism
        let sab_available = check_shared_array_buffer_available();

        if sab_available {
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                "[VM] SharedArrayBuffer available - enabling SMP mode",
            ));
        } else {
            web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(
                "[VM] SharedArrayBuffer not available - running single-threaded",
            ));
        }

        // Create bus with shared memory if available
        let (
            bus,
            shared_buffer,
            shared_control,
            shared_clint,
            shared_uart_output,
            shared_uart_input,
        ) = if sab_available {
            // Create SharedArrayBuffer for shared memory
            let total_size = shared_mem::total_shared_size(DRAM_SIZE);
            let sab = js_sys::SharedArrayBuffer::new(total_size as u32);

            // Initialize shared memory regions
            shared_mem::wasm::init_shared_memory(&sab, num_harts);

            // Create bus with DRAM backed by shared buffer
            // IMPORTANT: Pass the full SharedArrayBuffer with the DRAM byte offset,
            // NOT a sliced copy (slice() creates a copy, breaking shared memory!)
            // Also pass SharedClint so CLINT MMIO accesses go through shared memory.
            let dram_offset = shared_mem::dram_offset();
            let shared_clint_for_bus = shared_mem::wasm::SharedClint::new(&sab);
            // Main thread (hart 0) reads from local UART, not shared input
            let bus = SystemBus::from_shared_buffer(
                sab.clone(),
                dram_offset,
                shared_clint_for_bus,
                false,
            );

            let control = shared_mem::wasm::SharedControl::new(&sab);
            let clint = shared_mem::wasm::SharedClint::new(&sab);
            let uart_output = shared_mem::wasm::SharedUartOutput::new(&sab);
            let uart_input = shared_mem::wasm::SharedUartInput::new(&sab);

            (
                bus,
                Some(sab),
                Some(control),
                Some(clint),
                Some(uart_output),
                Some(uart_input),
            )
        } else {
            // Standard bus without shared memory
            let bus = SystemBus::new(DRAM_BASE, DRAM_SIZE);
            (bus, None, None, None, None, None)
        };

        // Load kernel
        let entry_pc = if kernel.starts_with(b"\x7FELF") {
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("[VM] Detected ELF kernel"));
            let entry = load_elf_wasm(kernel, &bus)
                .map_err(|e| JsValue::from_str(&format!("Failed to load ELF kernel: {}", e)))?;
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
                "[VM] ELF loaded, entry point: 0x{:x}",
                entry
            )));
            entry
        } else {
            web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(&format!(
                "[VM] Warning: kernel does not appear to be an ELF file (magic: {:02x?}). Loading as raw binary.",
                &kernel[..kernel.len().min(4)]
            )));
            bus.dram
                .load(kernel, 0)
                .map_err(|e| JsValue::from_str(&format!("Failed to load kernel: {}", e)))?;
            DRAM_BASE
        };

        // Set hart count in CLINT (native CLINT in bus)
        bus.set_num_harts(num_harts);

        // Create primary CPU (hart 0)
        let cpu = cpu::Cpu::new(entry_pc, 0);

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
            "[VM] Created {} harts, entry PC=0x{:x}, SMP={}",
            num_harts, entry_pc, sab_available
        )));

        Ok(WasmVm {
            bus,
            cpu,
            num_harts,
            net_status: NetworkStatus::Disconnected,
            poll_counter: 0,
            halted: false,
            halt_code: 0,
            shared_buffer,
            shared_control,
            shared_clint,
            shared_uart_output,
            shared_uart_input,
            workers: Vec::new(),
            workers_ready: Vec::new(),
            workers_started: false,
            entry_pc,
            boot_steps: 0,
            workers_signaled: false,
            external_net: None,
        })
    }

    /// Load a disk image and attach it as a VirtIO block device.
    /// This should be called before starting execution if the kernel needs a filesystem.
    pub fn load_disk(&mut self, disk_image: &[u8]) {
        let vblk = crate::devices::virtio::VirtioBlock::new(disk_image.to_vec());
        self.bus.virtio_devices.push(Box::new(vblk));
    }

    /// Connect to a WebTransport relay server.
    /// Note: Connection is asynchronous. Check network_status() to monitor connection state.
    pub fn connect_webtransport(
        &mut self,
        url: &str,
        cert_hash: Option<String>,
    ) -> Result<(), JsValue> {
        use crate::devices::virtio::VirtioNet;
        use crate::net::webtransport::WebTransportBackend;

        // Status stays as Connecting until we can verify the connection is established
        // (when IP is assigned, the connection is confirmed)
        self.net_status = NetworkStatus::Connecting;

        let backend = WebTransportBackend::new(url, cert_hash);
        // Note: WebTransport connect is async, so backend.init() will start connection
        // but actual connection happens in background.
        let vnet = VirtioNet::new(Box::new(backend));
        // debug defaults to false in VirtioNet

        self.bus.virtio_devices.push(Box::new(vnet));
        // Don't set to Connected here - let network_status() check the actual state

        Ok(())
    }

    /// Disconnect from the network.
    pub fn disconnect_network(&mut self) {
        // Remove VirtioNet devices (device_id == 1)
        self.bus.virtio_devices.retain(|dev| dev.device_id() != 1);
        self.net_status = NetworkStatus::Disconnected;
        self.external_net = None;
    }

    // ========================================================================
    // External Network Backend (for Node.js native addon bridging)
    // ========================================================================

    /// Set up an external network backend for packet bridging.
    /// This is used by the Node.js CLI to bridge packets between the native
    /// WebTransport addon and the WASM VM.
    ///
    /// @param mac_bytes - MAC address as 6 bytes [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    pub fn setup_external_network(&mut self, mac_bytes: js_sys::Uint8Array) -> Result<(), JsValue> {
        use crate::devices::virtio::VirtioNet;
        use crate::net::external::{ExternalBackendWrapper, ExternalNetworkBackend};

        // Parse MAC address
        let mac_vec = mac_bytes.to_vec();
        if mac_vec.len() != 6 {
            return Err(JsValue::from_str("MAC address must be 6 bytes"));
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&mac_vec);

        // Create external backend
        let backend = Arc::new(ExternalNetworkBackend::new(mac));
        self.external_net = Some(backend.clone());

        // Create a wrapper that implements NetworkBackend
        let wrapper = ExternalBackendWrapper { inner: backend };

        // Create VirtIO network device
        let vnet = VirtioNet::new(Box::new(wrapper));
        self.bus.virtio_devices.push(Box::new(vnet));

        self.net_status = NetworkStatus::Connecting;

        // Log to console (works in both browser and Node.js with WASM)
        let msg = format!(
            "[VM] External network setup, MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&msg));

        Ok(())
    }

    /// Inject a network packet to be received by the guest.
    /// Called from JavaScript when the native WebTransport addon receives a packet.
    pub fn inject_network_packet(&self, packet: js_sys::Uint8Array) -> bool {
        if let Some(ref backend) = self.external_net {
            backend.inject_rx_packet(packet.to_vec());
            true
        } else {
            false
        }
    }

    /// Extract a network packet sent by the guest.
    /// Returns the packet data or null if no packet is pending.
    pub fn extract_network_packet(&self) -> Option<js_sys::Uint8Array> {
        if let Some(ref backend) = self.external_net {
            backend.extract_tx_packet().map(|p| {
                let arr = js_sys::Uint8Array::new_with_length(p.len() as u32);
                arr.copy_from(&p);
                arr
            })
        } else {
            None
        }
    }

    /// Extract all pending network packets sent by the guest.
    /// Returns an array of packet data.
    pub fn extract_all_network_packets(&self) -> js_sys::Array {
        let arr = js_sys::Array::new();
        if let Some(ref backend) = self.external_net {
            for p in backend.extract_all_tx_packets() {
                let uint8 = js_sys::Uint8Array::new_with_length(p.len() as u32);
                uint8.copy_from(&p);
                arr.push(&uint8);
            }
        }
        arr
    }

    /// Set the assigned IP address for the external network.
    /// Called when the native WebTransport addon receives an IP assignment.
    pub fn set_external_network_ip(&self, ip_bytes: js_sys::Uint8Array) -> bool {
        let ip_vec = ip_bytes.to_vec();
        if ip_vec.len() != 4 {
            return false;
        }
        if let Some(ref backend) = self.external_net {
            let mut ip = [0u8; 4];
            ip.copy_from_slice(&ip_vec);
            backend.set_assigned_ip(ip);
            backend.set_connected(true);
            true
        } else {
            false
        }
    }

    /// Check if external network is connected (has IP assigned).
    pub fn is_external_network_connected(&self) -> bool {
        if let Some(ref backend) = self.external_net {
            backend.is_connected()
        } else {
            false
        }
    }

    /// Get the number of pending RX packets.
    pub fn external_network_rx_pending(&self) -> usize {
        if let Some(ref backend) = self.external_net {
            backend.rx_queue_len()
        } else {
            0
        }
    }

    /// Get the number of pending TX packets.
    pub fn external_network_tx_pending(&self) -> usize {
        if let Some(ref backend) = self.external_net {
            backend.tx_queue_len()
        } else {
            0
        }
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
                        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
                            "[network_status] Device idx={} id={} read(0x108)={:#x}",
                            idx, dev_id, ip_val
                        )));
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

    /// Execute one instruction on hart 0 (primary hart).
    ///
    /// In SMP mode, secondary harts run in Web Workers and execute in parallel.
    /// This method only steps hart 0, which handles I/O coordination.
    ///
    /// Returns true if the VM is still running, false if halted.
    pub fn step(&mut self) -> bool {
        // If already halted, don't execute more instructions
        if self.halted {
            return false;
        }

        // Check if workers reported halt
        if let Some(ref control) = self.shared_control {
            if control.is_halted() {
                self.halted = true;
                self.halt_code = control.halt_code();
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "[VM] Worker signaled halt (code: {:#x})",
                    self.halt_code
                )));
                return false;
            }
        }

        // Track boot progress and signal workers after initial boot.
        // This gives hart 0 minimal time to set up critical state before
        // secondary harts start executing. Secondary harts should immediately
        // check mhartid and go to a parking loop, so they don't need much delay.
        // NOTE: The kernel is responsible for synchronizing harts - this is just
        // a small buffer to ensure workers aren't trying to execute before the
        // first few instructions of hart 0 have run.
        const BOOT_STEPS_THRESHOLD: u64 = 1_000; // ~1K instructions - minimal delay
        if !self.workers_signaled {
            self.boot_steps += 1;
            if self.boot_steps >= BOOT_STEPS_THRESHOLD {
                if let Some(ref control) = self.shared_control {
                    control.allow_workers_to_start();
                    self.workers_signaled = true;
                    web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
                        "[VM] Workers signaled to start after {} boot steps",
                        self.boot_steps
                    )));
                }
            }
        }

        // Poll VirtIO devices periodically for incoming network packets
        // Poll every 100 instructions for good network responsiveness
        self.poll_counter = self.poll_counter.wrapping_add(1);
        if self.poll_counter % 100 == 0 {
            self.bus.poll_virtio();

            // Update shared CLINT timer (if in SMP mode)
            if let Some(ref clint) = self.shared_clint {
                // Increment mtime in shared memory
                clint.tick(100); // 100 ticks per poll
            }
        }

        // Execute one instruction on hart 0 only
        // (Secondary harts run in workers)
        match self.cpu.step(&self.bus) {
            Ok(()) => {}
            Err(Trap::RequestedTrap(code)) => {
                self.halted = true;
                self.halt_code = code;
                // Signal halt to workers
                if let Some(ref control) = self.shared_control {
                    control.signal_halted(code);
                }
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "[VM] Hart 0 requested halt (code: {:#x})",
                    code
                )));
                return false;
            }
            Err(Trap::Fatal(msg)) => {
                web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "[VM] Fatal error: {} at PC=0x{:x}",
                    msg, self.cpu.pc
                )));
                self.halted = true;
                if let Some(ref control) = self.shared_control {
                    control.signal_halted(0xDEAD);
                }
                return false;
            }
            Err(_trap) => {
                // Architectural traps handled by CPU
            }
        }

        true
    }

    /// Start worker threads for secondary harts (1..num_harts).
    ///
    /// Workers run in parallel with the main thread, sharing DRAM and CLINT
    /// via SharedArrayBuffer.
    ///
    /// # Arguments
    /// * `worker_url` - URL to the worker script (e.g., "/worker.js")
    pub fn start_workers(&mut self, worker_url: &str) -> Result<(), JsValue> {
        // Only start workers if we have shared memory and more than 1 hart
        if self.shared_buffer.is_none() || self.num_harts <= 1 {
            web_sys::console::log_1(&JsValue::from_str(
                "[VM] Skipping worker creation (single-threaded mode or 1 hart)",
            ));
            return Ok(());
        }

        if self.workers_started {
            return Ok(());
        }

        let shared_buffer = self.shared_buffer.as_ref().unwrap();

        web_sys::console::log_1(&JsValue::from_str(&format!(
            "[VM] Starting {} workers at {}",
            self.num_harts - 1,
            worker_url
        )));

        for hart_id in 1..self.num_harts {
            // Create worker with ESM module type
            let mut opts = web_sys::WorkerOptions::new();
            opts.type_(web_sys::WorkerType::Module);

            let worker = web_sys::Worker::new_with_options(worker_url, &opts)
                .map_err(|e| JsValue::from_str(&format!("Failed to create worker: {:?}", e)))?;

            // Set up message handler for this worker
            let hart_id_copy = hart_id;
            let onmessage = wasm_bindgen::closure::Closure::wrap(Box::new(
                move |event: web_sys::MessageEvent| {
                    let data = event.data();
                    if let Some(type_str) = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
                        .ok()
                        .and_then(|v| v.as_string())
                    {
                        match type_str.as_str() {
                            "ready" => {
                                web_sys::console::log_1(&JsValue::from_str(&format!(
                                    "[VM] Worker {} ready",
                                    hart_id_copy
                                )));
                            }
                            "halted" => {
                                web_sys::console::log_1(&JsValue::from_str(&format!(
                                    "[VM] Worker {} halted",
                                    hart_id_copy
                                )));
                            }
                            "error" => {
                                if let Ok(error) =
                                    js_sys::Reflect::get(&data, &JsValue::from_str("error"))
                                {
                                    web_sys::console::error_1(&JsValue::from_str(&format!(
                                        "[VM] Worker {} error: {:?}",
                                        hart_id_copy, error
                                    )));
                                }
                            }
                            _ => {}
                        }
                    }
                },
            )
                as Box<dyn FnMut(_)>);

            worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
            onmessage.forget(); // Leak the closure (lives for program lifetime)

            // Send init message to worker
            let init_msg = js_sys::Object::new();
            js_sys::Reflect::set(
                &init_msg,
                &JsValue::from_str("hartId"),
                &JsValue::from(hart_id as u32),
            )
            .unwrap();
            js_sys::Reflect::set(&init_msg, &JsValue::from_str("sharedMem"), shared_buffer)
                .unwrap();
            js_sys::Reflect::set(
                &init_msg,
                &JsValue::from_str("entryPc"),
                &JsValue::from(self.entry_pc as f64),
            )
            .unwrap();

            worker
                .post_message(&init_msg)
                .map_err(|e| JsValue::from_str(&format!("Failed to send init message: {:?}", e)))?;

            self.workers.push(worker);
            self.workers_ready.push(false);
        }

        self.workers_started = true;
        web_sys::console::log_1(&JsValue::from_str(&format!(
            "[VM] Started {} workers",
            self.workers.len()
        )));

        Ok(())
    }

    /// Get the number of harts configured.
    pub fn num_harts(&self) -> usize {
        self.num_harts
    }

    /// Check if running in SMP mode (with workers).
    pub fn is_smp(&self) -> bool {
        self.shared_buffer.is_some() && self.num_harts > 1
    }

    /// Get the SharedArrayBuffer for external worker management.
    /// Returns None if not in SMP mode.
    pub fn get_shared_buffer(&self) -> Option<js_sys::SharedArrayBuffer> {
        self.shared_buffer.clone()
    }

    /// Get the entry PC address for workers.
    /// This is the address where secondary harts should start executing.
    pub fn entry_pc(&self) -> u64 {
        self.entry_pc
    }

    /// Signal that workers can start executing.
    /// Called by the main thread after hart 0 has finished initializing
    /// kernel data structures.
    pub fn allow_workers_to_start(&mut self) {
        if let Some(ref control) = self.shared_control {
            control.allow_workers_to_start();
            self.workers_signaled = true;
            web_sys::console::log_1(&JsValue::from_str("[VM] Workers signaled to start"));
        }
    }

    /// Terminate all workers.
    pub fn terminate_workers(&mut self) {
        // Signal halt to workers
        if let Some(ref control) = self.shared_control {
            control.request_halt();
        }

        // Terminate worker threads
        for worker in &self.workers {
            worker.terminate();
        }
        self.workers.clear();
        self.workers_ready.clear();
        self.workers_started = false;

        web_sys::console::log_1(&JsValue::from_str("[VM] All workers terminated"));
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
    ///
    /// In SMP mode, this checks both the shared UART output buffer (for worker output)
    /// and the local UART buffer (for hart 0 output).
    pub fn get_output(&mut self) -> Option<u8> {
        // First check shared UART output from workers
        if let Some(ref shared_uart) = self.shared_uart_output {
            if let Some(byte) = shared_uart.read_byte() {
                return Some(byte);
            }
        }

        // Then check local UART (hart 0 output)
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
        self.bus.uart.output_len()
    }

    /// Write a string to the UART output buffer (VM host message).
    /// This allows the VM to emit its own messages that the browser can display
    /// alongside kernel output.
    fn emit_to_uart(&mut self, s: &str) {
        self.bus.uart.push_output_str(s);
    }

    /// Log a message to both browser console and UART output.
    /// This ensures VM messages appear in both the developer console
    /// and the terminal UI visible to users.
    fn log_to_uart(&mut self, prefix: &str, message: &str) {
        // Log to browser console
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
            "{} {}",
            prefix, message
        )));
        // Also emit to UART so it appears in the terminal UI
        self.emit_to_uart(prefix);
        self.emit_to_uart(" ");
        self.emit_to_uart(message);
        self.emit_to_uart("\n");
    }

    /// Print the VM banner to UART output (visible in browser).
    /// Call this after creating the VM to show a boot banner.
    pub fn print_banner(&mut self) {
        let banner = format!(
            "\x1b[1;36m\
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│   ██████╗  █████╗ ██╗   ██╗██╗   ██╗    ██╗   ██╗███╗   ███╗            │
│   ██╔══██╗██╔══██╗██║   ██║╚██╗ ██╔╝    ██║   ██║████╗ ████║            │
│   ██████╔╝███████║██║   ██║ ╚████╔╝     ██║   ██║██╔████╔██║            │
│   ██╔══██╗██╔══██║╚██╗ ██╔╝  ╚██╔╝      ╚██╗ ██╔╝██║╚██╔╝██║            │
│   ██████╔╝██║  ██║ ╚████╔╝    ██║        ╚████╔╝ ██║ ╚═╝ ██║            │
│   ╚═════╝ ╚═╝  ╚═╝  ╚═══╝     ╚═╝         ╚═══╝  ╚═╝     ╚═╝            │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
\x1b[0m\n"
        );
        self.emit_to_uart(&banner);
    }

    /// Print a status message to UART output (visible in browser).
    pub fn print_status(&mut self, message: &str) {
        self.emit_to_uart("\x1b[1;33m[VM]\x1b[0m ");
        self.emit_to_uart(message);
        self.emit_to_uart("\n");
    }

    /// Push an input byte to the UART.
    /// In SMP mode, this also writes to the shared input buffer so workers can receive it.
    pub fn input(&mut self, byte: u8) {
        // Push to local UART for hart 0
        self.bus.uart.push_input(byte);

        // Also push to shared input buffer for workers to receive
        if let Some(ref shared_input) = self.shared_uart_input {
            let _ = shared_input.write_byte(byte);
        }
    }

    /// Get current memory usage (DRAM size) in bytes.
    pub fn get_memory_usage(&self) -> u64 {
        self.bus.dram_size() as u64
    }

    /// Get heap memory usage from the guest kernel.
    /// Returns (used_bytes, total_bytes).
    pub fn get_heap_usage(&self) -> js_sys::Array {
        let (used, total) = self.bus.sysinfo.heap_usage();
        let arr = js_sys::Array::new();
        arr.push(&JsValue::from(used as f64));
        arr.push(&JsValue::from(total as f64));
        arr
    }

    /// Get disk usage from the guest kernel.
    /// Returns (used_bytes, total_bytes).
    pub fn get_disk_usage(&self) -> js_sys::Array {
        let (used, total) = self.bus.sysinfo.disk_usage();
        let arr = js_sys::Array::new();
        arr.push(&JsValue::from(used as f64));
        arr.push(&JsValue::from(total as f64));
        arr
    }

    /// Get the total disk capacity from attached VirtIO block devices.
    /// Returns total bytes across all block devices.
    pub fn get_disk_capacity(&self) -> u64 {
        let mut total: u64 = 0;
        for device in &self.bus.virtio_devices {
            // VirtIO block device has device_id 2
            if device.device_id() == 2 {
                // Read capacity from config space (offset 0x100 and 0x104)
                if let Ok(cap_lo) = device.read(0x100) {
                    if let Ok(cap_hi) = device.read(0x104) {
                        let capacity_sectors = cap_lo | (cap_hi << 32);
                        total += capacity_sectors * 512; // Convert sectors to bytes
                    }
                }
            }
        }
        total
    }

    /// Get CPU count (from kernel-reported value).
    pub fn get_cpu_count(&self) -> u32 {
        self.bus.sysinfo.cpu_count()
    }

    /// Get system uptime in milliseconds (from kernel-reported value).
    pub fn get_uptime_ms(&self) -> u64 {
        self.bus.sysinfo.uptime_ms()
    }
}
