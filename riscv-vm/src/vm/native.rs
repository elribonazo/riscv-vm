use crate::Trap;
use crate::bus::{DRAM_BASE, SystemBus};
use crate::console::Console;
use crate::cpu::Cpu;
use crate::loader::load_elf_into_dram;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    const HALT_REQUESTED: u8 = 0x01;
    const HALTED: u8 = 0x02;

    pub fn new() -> Self {
        Self {
            flags: AtomicU8::new(0),
            halt_code: AtomicU64::new(0),
            _padding: [0; 64 - std::mem::size_of::<AtomicU8>() - std::mem::size_of::<AtomicU64>()],
        }
    }

    pub fn request_halt(&self) {
        self.flags.fetch_or(Self::HALT_REQUESTED, Ordering::Release);
    }

    pub fn is_halt_requested(&self) -> bool {
        (self.flags.load(Ordering::Relaxed) & Self::HALT_REQUESTED) != 0
    }

    pub fn signal_halted(&self, code: u64) {
        self.halt_code.store(code, Ordering::Relaxed);
        self.flags.fetch_or(Self::HALTED, Ordering::Release);
    }

    pub fn is_halted(&self) -> bool {
        (self.flags.load(Ordering::Relaxed) & Self::HALTED) != 0
    }

    pub fn halt_code(&self) -> u64 {
        self.halt_code.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn should_stop(&self) -> bool {
        self.flags.load(Ordering::Relaxed) != 0
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

enum HaltReason {
    Shutdown(u64),
    Fatal(String, u64),
}

/// Native multi-threaded VM.
///
/// Manages one thread per hart, with hart 0 running on the main thread
/// for I/O coordination.
pub struct NativeVm {
    bus: Arc<SystemBus>,
    handles: Vec<JoinHandle<()>>,
    primary_cpu: Option<Cpu>,
    pub shared: Arc<SharedState>,
    num_harts: usize,
    entry_pc: u64,
}

impl NativeVm {
    /// Create a new VM with the given kernel.
    ///
    /// # Arguments
    /// * `kernel` - Kernel binary (ELF or raw)
    /// * `num_harts` - Number of harts (CPUs) to create
    pub fn new(kernel: &[u8], num_harts: usize) -> Result<Self, String> {
        const DRAM_SIZE: usize = 512 * 1024 * 1024;
        let bus = SystemBus::new(DRAM_BASE, DRAM_SIZE);

        bus.set_num_harts(num_harts);

        let entry_pc = if kernel.starts_with(b"\x7FELF") {
            load_elf_into_dram(kernel, &bus)?
        } else {
            bus.dram
                .load(kernel, 0)
                .map_err(|e| format!("Failed to load kernel: {:?}", e))?;
            DRAM_BASE
        };

        let bus = Arc::new(bus);
        let shared = Arc::new(SharedState::new());
        let primary_cpu = Some(Cpu::new(entry_pc, 0));

        println!(
            "[VM] Created with {} harts, entry=0x{:x}",
            num_harts, entry_pc
        );

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
        let num_harts = (cpus / 2).max(1);
        Self::new(kernel, num_harts)
    }

    /// Load a disk image and attach as VirtIO block device.
    pub fn load_disk(&mut self, disk: Vec<u8>) {
        use crate::devices::virtio::VirtioBlock;

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
    pub fn connect_webtransport(&mut self, url: &str, cert_hash: Option<String>) {
        use crate::devices::virtio::VirtioNet;
        use crate::net::async_backend::AsyncNetworkBackend;
        use crate::net::webtransport::WebTransportBackend;

        if let Some(bus) = Arc::get_mut(&mut self.bus) {
            let backend = WebTransportBackend::new(url, cert_hash);
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
    pub fn run(&mut self) {
        if !self.workers_started() {
            self.start_workers();
        }

        let mut cpu = self.primary_cpu.take().expect("CPU already taken");
        let mut step_count: u64 = 0;
        let start_time = Instant::now();

        let console = Console::new();
        let mut escaped = false;

        let mut last_report_time = Instant::now();
        let mut last_report_steps: u64 = 0;
        let report_interval = Duration::from_secs(5);

        println!("[VM] Running hart 0 on main thread...");

        const BATCH_SIZE: u64 = 256;
        const VIRTIO_POLL_INTERVAL: u64 = 4096;
        const CONSOLE_POLL_INTERVAL: u64 = 16384;

        loop {
            if self.shared.should_stop() {
                break;
            }

            let (batch_steps, halt_reason) = self.execute_batch(&mut cpu, BATCH_SIZE);
            step_count += batch_steps;

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

            if step_count % VIRTIO_POLL_INTERVAL == 0 {
                self.bus.poll_virtio();
            }

            if step_count % CONSOLE_POLL_INTERVAL == 0 {
                self.pump_console(&console, &mut escaped);

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

        self.shutdown();

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
                Err(_) => {
                    count += 1;
                }
            }
        }

        (count, None)
    }

    fn pump_console(&self, console: &Console, escaped: &mut bool) {
        let output = self.bus.uart.drain_output();
        if !output.is_empty() {
            for byte in output {
                if byte == b'\n' {
                    print!("\r\n");
                } else {
                    print!("{}", byte as char);
                }
            }
            io::stdout().flush().ok();
        }

        for byte in console.read_available() {
            if *escaped {
                if byte == b'x' {
                    println!("\r\n[VM] Terminated by user (Ctrl-A x)");
                    self.shared.request_halt();
                    return;
                } else if byte == 1 {
                    self.bus.uart.push_input(1);
                } else {
                    self.bus.uart.push_input(byte);
                }
                *escaped = false;
            } else if byte == 1 {
                *escaped = true;
            } else {
                self.bus.uart.push_input(byte);
            }
        }
    }

    fn shutdown(&mut self) {
        println!("[VM] Shutting down...");

        self.shared.request_halt();

        for handle in self.handles.drain(..) {
            if let Err(e) = handle.join() {
                eprintln!("[VM] Worker thread panicked: {:?}", e);
            }
        }

        println!("[VM] All threads stopped");
    }
}

impl Drop for NativeVm {
    fn drop(&mut self) {
        self.shared.request_halt();
        for handle in self.handles.drain(..) {
            handle.join().ok();
        }
    }
}

fn hart_thread(hart_id: usize, entry_pc: u64, bus: Arc<SystemBus>, shared: Arc<SharedState>) {
    let mut cpu = Cpu::new(entry_pc, hart_id as u64);
    let mut step_count: u64 = 0;
    let start_time = Instant::now();

    let mut last_report_time = Instant::now();
    let mut last_report_steps: u64 = 0;
    let report_interval = Duration::from_secs(5);

    println!("[Hart {}] Started at PC=0x{:x}", hart_id, entry_pc);

    const BATCH_SIZE: u64 = 256;
    const YIELD_INTERVAL: u64 = 4_000_000;

    loop {
        if shared.should_stop() {
            break;
        }

        let (batch_steps, halt_reason) = execute_batch_worker(&mut cpu, &bus, BATCH_SIZE);
        step_count += batch_steps;

        if let Some(reason) = halt_reason {
            match reason {
                HaltReason::Shutdown(code) => {
                    println!("[Hart {}] Shutdown requested (code: {:#x})", hart_id, code);
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

        if step_count % YIELD_INTERVAL == 0 {
            thread::yield_now();

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

fn execute_batch_worker(
    cpu: &mut Cpu,
    bus: &SystemBus,
    max_steps: u64,
) -> (u64, Option<HaltReason>) {
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
            Err(_) => {
                count += 1;
            }
        }
    }

    (count, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SystemBus;
    use crate::cpu::Cpu;
    use crate::devices::clint::Clint;
    use crate::devices::plic::Plic;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn measure_shared_state_size() {
        println!("Cpu size: {} bytes", std::mem::size_of::<Cpu>());
        println!("Cpu align: {} bytes", std::mem::align_of::<Cpu>());
        println!(
            "SharedState size: {} bytes",
            std::mem::size_of::<SharedState>()
        );
        println!(
            "SharedState align: {} bytes",
            std::mem::align_of::<SharedState>()
        );
        println!("SystemBus size: {} bytes", std::mem::size_of::<SystemBus>());
        println!("Clint size: {} bytes", std::mem::size_of::<Clint>());
        println!("Plic size: {} bytes", std::mem::size_of::<Plic>());
    }

    #[test]
    fn test_shared_state_alignment() {
        assert_eq!(std::mem::align_of::<SharedState>(), 64);
        assert_eq!(std::mem::size_of::<SharedState>(), 64);
    }

    #[test]
    fn test_shared_state_should_stop() {
        let state = SharedState::new();

        assert!(!state.should_stop());
        assert!(!state.is_halt_requested());
        assert!(!state.is_halted());

        state.request_halt();
        assert!(state.should_stop());
        assert!(state.is_halt_requested());
        assert!(!state.is_halted());

        let state2 = SharedState::new();
        assert!(!state2.should_stop());

        state2.signal_halted(42);
        assert!(state2.should_stop());
        assert!(!state2.is_halt_requested());
        assert!(state2.is_halted());
        assert_eq!(state2.halt_code(), 42);
    }

    #[test]
    fn test_shared_state_concurrent() {
        let state = Arc::new(SharedState::new());
        let mut handles = vec![];

        for _ in 0..4 {
            let state_clone = Arc::clone(&state);
            let handle = thread::spawn(move || {
                for _ in 0..100_000 {
                    let _ = state_clone.should_stop();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
