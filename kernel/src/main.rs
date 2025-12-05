#![no_std]
#![no_main]

// Override riscv-rt's _max_hart_id to allow multiple harts to boot
// This MUST be defined before riscv-rt's startup code runs
// Set to 127 to support up to 128 harts (matching MAX_HARTS)
core::arch::global_asm!(".global _max_hart_id", "_max_hart_id = 127");

mod allocator;
mod cmd;
mod dns;
mod lock;

// Re-export Spinlock for convenience
pub use lock::Spinlock;
mod fs;
mod http;
mod net;
mod scripting;
mod tls;
mod tls12;
mod uart;
mod virtio_blk;
mod virtio_net;

// Process management modules
mod init;
mod ipc;
mod klog;
mod scheduler;
mod task;

pub use scheduler::SCHEDULER;

extern crate alloc;
use alloc::{format, string::String, vec::Vec};
use core::arch::asm;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};
use panic_halt as _;
use riscv_rt::entry;

/// Flag indicating primary boot is complete.
/// Secondary harts spin on this before proceeding.
static BOOT_READY: AtomicBool = AtomicBool::new(false);

/// Counter of harts that have completed initialization.
static HARTS_ONLINE: AtomicUsize = AtomicUsize::new(0);

/// CLINT MSIP register base address.
const CLINT_MSIP_BASE: usize = 0x0200_0000;

/// CLINT hart count register (set by emulator, read by kernel)
const CLINT_HART_COUNT: usize = 0x0200_0F00;

/// Read the hart count from the CLINT register (set by emulator)
fn get_expected_harts() -> usize {
    let count = unsafe { core::ptr::read_volatile(CLINT_HART_COUNT as *const u32) } as usize;
    // Clamp to valid range [1, MAX_HARTS]
    if count == 0 {
        1
    } else {
        count.min(MAX_HARTS)
    }
}

/// Maximum number of harts supported.
/// Set high enough to support modern multi-core systems.
pub const MAX_HARTS: usize = 128;

// ═══════════════════════════════════════════════════════════════════════════════
// BENCHMARK STATE (for multi-hart CPU testing)
// ═══════════════════════════════════════════════════════════════════════════════

/// Benchmark mode for parallel computation
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BenchmarkMode {
    Idle = 0,
    PrimeCount = 1,
}

/// Shared benchmark state for coordinating work across harts
struct BenchmarkState {
    /// Current benchmark mode (0 = idle, 1 = prime counting)
    mode: AtomicUsize,
    /// Start of range for prime counting
    range_start: AtomicU64,
    /// End of range for prime counting  
    range_end: AtomicU64,
    /// Number of harts that should participate
    num_harts: AtomicUsize,
    /// Counter for harts that have completed their work
    completed: AtomicUsize,
    /// Results from each hart (prime counts)
    results: [AtomicU64; MAX_HARTS],
}

impl BenchmarkState {
    const fn new() -> Self {
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            mode: AtomicUsize::new(0),
            range_start: AtomicU64::new(0),
            range_end: AtomicU64::new(0),
            num_harts: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            results: [ZERO; MAX_HARTS],
        }
    }

    /// Start a new benchmark
    fn start(&self, mode: BenchmarkMode, start: u64, end: u64, num_harts: usize) {
        // Reset results
        for i in 0..MAX_HARTS {
            self.results[i].store(0, Ordering::Relaxed);
        }
        self.completed.store(0, Ordering::Relaxed);
        self.range_start.store(start, Ordering::Relaxed);
        self.range_end.store(end, Ordering::Relaxed);
        self.num_harts.store(num_harts, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        // Set mode last to signal start
        self.mode.store(mode as usize, Ordering::Release);
    }

    /// Clear benchmark (return to idle)
    fn clear(&self) {
        self.mode
            .store(BenchmarkMode::Idle as usize, Ordering::Release);
    }

    /// Check if benchmark is active
    fn is_active(&self) -> bool {
        self.mode.load(Ordering::Acquire) != BenchmarkMode::Idle as usize
    }

    /// Get work range for a specific hart
    fn get_work_range(&self, hart_id: usize) -> (u64, u64) {
        let start = self.range_start.load(Ordering::Relaxed);
        let end = self.range_end.load(Ordering::Relaxed);
        let num_harts = self.num_harts.load(Ordering::Relaxed);

        if num_harts == 0 || hart_id >= num_harts {
            return (0, 0);
        }

        let total_range = end - start;
        let chunk_size = total_range / num_harts as u64;

        let my_start = start + (hart_id as u64 * chunk_size);
        let my_end = if hart_id == num_harts - 1 {
            end // Last hart takes remainder
        } else {
            my_start + chunk_size
        };

        (my_start, my_end)
    }

    /// Report result from a hart
    fn report_result(&self, hart_id: usize, count: u64) {
        if hart_id < MAX_HARTS {
            self.results[hart_id].store(count, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    /// Get total result from all harts
    fn total_result(&self) -> u64 {
        let mut total = 0u64;
        let num_harts = self.num_harts.load(Ordering::Relaxed);
        for i in 0..num_harts {
            total += self.results[i].load(Ordering::Relaxed);
        }
        total
    }

    /// Check if all harts have completed
    fn all_completed(&self) -> bool {
        let num_harts = self.num_harts.load(Ordering::Relaxed);
        self.completed.load(Ordering::Acquire) >= num_harts
    }
}

/// Global benchmark state
static BENCHMARK: BenchmarkState = BenchmarkState::new();

// ═══════════════════════════════════════════════════════════════════════════════
// PRIME NUMBER FUNCTIONS (for CPU benchmarking)
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if a number is prime using trial division
/// Optimized with early exits and only checking up to sqrt(n)
#[inline(never)] // Prevent inlining to ensure fair timing
fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n == 2 {
        return true;
    }
    if n % 2 == 0 {
        return false;
    }
    if n == 3 {
        return true;
    }
    if n % 3 == 0 {
        return false;
    }

    // Check divisors of form 6k±1 up to sqrt(n)
    let mut i = 5u64;
    while i * i <= n {
        if n % i == 0 || n % (i + 2) == 0 {
            return false;
        }
        i += 6;
    }
    true
}

/// Count primes in a range [start, end)
#[inline(never)]
fn count_primes_in_range(start: u64, end: u64) -> u64 {
    let mut count = 0u64;
    for n in start..end {
        if is_prime(n) {
            count += 1;
        }
    }
    count
}

/// Multi-processing hook called by riscv-rt before main().
///
/// - Hart 0: Returns true to continue to main()
/// - Other harts: Enter parking loop, call secondary_hart_entry when woken
///
/// # Safety
/// This is called very early in boot, before Rust runtime is fully initialized.
/// Only use assembly and no allocations.
#[export_name = "_mp_hook"]
#[inline(never)]
pub unsafe extern "C" fn mp_hook() -> bool {
    let hart_id: usize;
    asm!(
        "csrr {}, mhartid",
        out(reg) hart_id,
        options(nomem, nostack, preserves_flags)
    );

    if hart_id == 0 {
        // Primary hart: continue to main()
        true
    } else {
        // Secondary harts: park and wait for IPI
        secondary_hart_park(hart_id);
        // Never returns, but we need to satisfy the return type
        // This is unreachable
    }
}

/// Secondary hart parking loop.
///
/// Waits for IPI, then transfers to secondary_hart_entry.
///
/// # Safety
/// Called very early in boot, before Rust runtime is fully initialized.
#[inline(never)]
unsafe fn secondary_hart_park(hart_id: usize) -> ! {
    // Wait for IPI to wake us
    loop {
        asm!("wfi", options(nomem, nostack));

        // Check if this was our wake-up call
        if is_msip_pending(hart_id) {
            // Clear the interrupt
            clear_msip(hart_id);
            break;
        }
        // Spurious wakeup - go back to sleep
    }

    // Transfer to secondary entry point
    secondary_hart_entry(hart_id);
}

/// Get the current hart ID from mhartid CSR.
#[inline]
pub fn get_hart_id() -> usize {
    let id: usize;
    unsafe {
        asm!("csrr {}, mhartid", out(reg) id, options(nomem, nostack));
    }
    id
}

/// Entry point for secondary harts (called after waking from WFI).
///
/// This function is called after the secondary hart has:
/// 1. Been woken by an IPI from the primary hart
/// 2. Checked that BOOT_READY is true
///
/// # Arguments
/// * `hart_id` - This hart's ID (1, 2, 3, ...)
fn secondary_hart_entry(hart_id: usize) -> ! {
    // Wait for primary boot to complete (double-check after WFI wake)
    while !BOOT_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // Memory fence ensures we see all init writes from primary hart
    fence(Ordering::SeqCst);

    // Register this hart as online
    HARTS_ONLINE.fetch_add(1, Ordering::SeqCst);

    // Enter the secondary hart idle loop
    secondary_hart_idle(hart_id);
}

/// Secondary hart idle loop.
///
/// Secondary harts wait for work (IPI wakeup), then check for:
/// 1. Benchmark tasks (high priority, checked first)
/// 2. Scheduler tasks (including long-running daemons)
fn secondary_hart_idle(hart_id: usize) -> ! {
    loop {
        // Wait for work via IPI - this is the primary coordination mechanism
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack));
        }

        // Check if we were woken by an IPI
        if !is_my_msip_pending() {
            // Spurious wakeup - go back to sleep
            continue;
        }

        // Clear the IPI
        clear_my_msip();

        // Check for benchmark work first (high priority)
        if BENCHMARK.is_active() {
            let mode = BENCHMARK.mode.load(Ordering::Acquire);
            if mode == BenchmarkMode::PrimeCount as usize {
                // Get our work range
                let (start, end) = BENCHMARK.get_work_range(hart_id);
                if start < end {
                    // Count primes in our range
                    let count = count_primes_in_range(start, end);
                    // Report result
                    BENCHMARK.report_result(hart_id, count);
                } else {
                    // No work for this hart
                    BENCHMARK.report_result(hart_id, 0);
                }
                continue;
            }
        }

        // Check for scheduler tasks
        if SCHEDULER.is_running() {
            if let Some(task) = SCHEDULER.pick_next(hart_id) {
                // Mark task as running on this hart
                task.mark_running(hart_id);

                let start_time = get_time_ms() as u64;

                // Execute the task's entry point
                // Note: Daemon tasks have infinite loops and won't return
                (task.entry)();

                // If we get here, the task returned (non-daemon or daemon that exited)
                let elapsed = (get_time_ms() as u64).saturating_sub(start_time);
                task.add_cpu_time(elapsed);

                // Mark task as finished
                SCHEDULER.finish_task(task.pid, 0);
            }
        }
    }
}

/// Send an Inter-Processor Interrupt to the specified hart.
///
/// This triggers a `MachineSoftwareInterrupt` on the target hart,
/// waking it from WFI if sleeping.
///
/// # Arguments
/// * `hart_id` - The target hart ID (0-7)
///
/// # Safety
/// This function writes to MMIO registers but is safe to call
/// from any context.
#[inline]
pub fn send_ipi(hart_id: usize) {
    if hart_id >= MAX_HARTS {
        return; // Invalid hart ID, silently ignore
    }

    let msip_addr = CLINT_MSIP_BASE + (hart_id * 4);

    // Write 1 to MSIP[hart_id] to trigger software interrupt
    unsafe {
        core::ptr::write_volatile(msip_addr as *mut u32, 1);
    }

    // Memory fence to ensure write is visible
    fence(Ordering::SeqCst);
}

/// Send IPI to all harts except the caller.
///
/// Useful for broadcast notifications.
#[allow(dead_code)]
pub fn send_ipi_all_others() {
    let my_hart = get_hart_id();
    let expected_harts = get_expected_harts();
    for hart in 0..expected_harts {
        if hart != my_hart {
            send_ipi(hart);
        }
    }
}

/// Clear the software interrupt for a hart.
///
/// Must be called by the target hart to acknowledge the IPI.
/// Typically called in the software interrupt handler.
#[inline]
pub fn clear_msip(hart_id: usize) {
    if hart_id >= MAX_HARTS {
        return;
    }
    let msip_addr = CLINT_MSIP_BASE + (hart_id * 4);
    unsafe {
        core::ptr::write_volatile(msip_addr as *mut u32, 0);
    }
}

/// Clear the software interrupt for the current hart.
#[inline]
#[allow(dead_code)]
pub fn clear_my_msip() {
    clear_msip(get_hart_id());
}

/// Check if software interrupt is pending for a hart.
#[inline]
#[allow(dead_code)]
pub fn is_msip_pending(hart_id: usize) -> bool {
    if hart_id >= MAX_HARTS {
        return false;
    }
    let msip_addr = CLINT_MSIP_BASE + (hart_id * 4);
    unsafe { core::ptr::read_volatile(msip_addr as *const u32) & 1 != 0 }
}

/// Check if software interrupt is pending for current hart.
#[inline]
#[allow(dead_code)]
pub fn is_my_msip_pending() -> bool {
    is_msip_pending(get_hart_id())
}

const CLINT_MTIME: usize = 0x0200_BFF8;
const TEST_FINISHER: usize = 0x0010_0000;

// ═══════════════════════════════════════════════════════════════════════════════
// SPINLOCK-PROTECTED GLOBAL STATE
// ═══════════════════════════════════════════════════════════════════════════════

/// Network state, protected by spinlock.
static NET_STATE: Spinlock<Option<net::NetState>> = Spinlock::new(None);

/// Filesystem state, protected by spinlock.
static FS_STATE: Spinlock<Option<fs::FileSystem>> = Spinlock::new(None);

/// Block device, protected by spinlock.
static BLK_DEV: Spinlock<Option<virtio_blk::VirtioBlock>> = Spinlock::new(None);

/// State for continuous ping (like Linux ping command)
struct PingState {
    target: smoltcp::wire::Ipv4Address,
    seq: u16,
    sent_time: i64,      // Time when current ping was sent
    last_send_time: i64, // Time when we last sent a ping (for 1s interval)
    waiting: bool,       // Waiting for reply to current ping
    continuous: bool,    // Whether running in continuous mode
    // Statistics
    packets_sent: u32,
    packets_received: u32,
    min_rtt: i64,
    max_rtt: i64,
    total_rtt: i64,
}

impl PingState {
    fn new(target: smoltcp::wire::Ipv4Address, timestamp: i64) -> Self {
        PingState {
            target,
            seq: 0,
            sent_time: timestamp,
            last_send_time: 0,
            waiting: false,
            continuous: true,
            packets_sent: 0,
            packets_received: 0,
            min_rtt: i64::MAX,
            max_rtt: 0,
            total_rtt: 0,
        }
    }

    fn record_reply(&mut self, rtt: i64) {
        self.packets_received += 1;
        self.total_rtt += rtt;
        if rtt < self.min_rtt {
            self.min_rtt = rtt;
        }
        if rtt > self.max_rtt {
            self.max_rtt = rtt;
        }
    }

    fn avg_rtt(&self) -> i64 {
        if self.packets_received > 0 {
            self.total_rtt / self.packets_received as i64
        } else {
            0
        }
    }

    fn packet_loss_percent(&self) -> u32 {
        if self.packets_sent > 0 {
            ((self.packets_sent - self.packets_received) * 100) / self.packets_sent
        } else {
            0
        }
    }
}

/// Ping state, protected by spinlock.
static PING_STATE: Spinlock<Option<PingState>> = Spinlock::new(None);

/// Command running flag, protected by spinlock.
static COMMAND_RUNNING: Spinlock<bool> = Spinlock::new(false);

// ─── CURRENT WORKING DIRECTORY ────────────────────────────────────────────────
const CWD_MAX_LEN: usize = 128;

/// Current working directory state
struct CwdState {
    path: [u8; CWD_MAX_LEN],
    len: usize,
}

impl CwdState {
    const fn new() -> Self {
        let mut path = [0u8; CWD_MAX_LEN];
        path[0] = b'/';
        Self { path, len: 1 }
    }
}

/// Current working directory, protected by spinlock.
static CWD_STATE: Spinlock<CwdState> = Spinlock::new(CwdState::new());

/// Initialize CWD to root
fn cwd_init() {
    let mut cwd = CWD_STATE.lock();
    cwd.path[0] = b'/';
    cwd.len = 1;
}

/// Get current working directory as String
pub fn cwd_get() -> alloc::string::String {
    let cwd = CWD_STATE.lock();
    core::str::from_utf8(&cwd.path[..cwd.len])
        .unwrap_or("/")
        .into()
}

/// Set current working directory
fn cwd_set(path: &str) {
    let mut cwd = CWD_STATE.lock();
    let bytes = path.as_bytes();
    let len = core::cmp::min(bytes.len(), CWD_MAX_LEN);
    cwd.path[..len].copy_from_slice(&bytes[..len]);
    cwd.len = len;
}

// ─── OUTPUT CAPTURE FOR REDIRECTION ────────────────────────────────────────────
const OUTPUT_BUFFER_SIZE: usize = 4096;

/// Output capture state for redirection
struct OutputCapture {
    buffer: [u8; OUTPUT_BUFFER_SIZE],
    len: usize,
    capturing: bool,
}

impl OutputCapture {
    const fn new() -> Self {
        Self {
            buffer: [0u8; OUTPUT_BUFFER_SIZE],
            len: 0,
            capturing: false,
        }
    }
}

/// Output capture state, protected by spinlock.
static OUTPUT_CAPTURE: Spinlock<OutputCapture> = Spinlock::new(OutputCapture::new());

/// Start capturing output to the buffer
fn output_capture_start() {
    let mut cap = OUTPUT_CAPTURE.lock();
    cap.capturing = true;
    cap.len = 0;
}

/// Stop capturing and return the captured bytes as a Vec
fn output_capture_stop() -> Vec<u8> {
    let mut cap = OUTPUT_CAPTURE.lock();
    cap.capturing = false;
    Vec::from(&cap.buffer[..cap.len])
}

/// Write a string - respects capture mode
fn out_str(s: &str) {
    let mut cap = OUTPUT_CAPTURE.lock();
    if cap.capturing {
        for &b in s.as_bytes() {
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = b;
                cap.len += 1;
            }
        }
    } else {
        drop(cap); // Release lock before UART
        uart::write_str(s);
    }
}

/// Write a string with newline - respects capture mode
fn out_line(s: &str) {
    out_str(s);
    out_str("\n");
}

/// Write bytes - respects capture mode
fn out_bytes(bytes: &[u8]) {
    let mut cap = OUTPUT_CAPTURE.lock();
    if cap.capturing {
        for &b in bytes {
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = b;
                cap.len += 1;
            }
        }
    } else {
        drop(cap); // Release lock before UART
        uart::write_bytes(bytes);
    }
}

/// Write u64 - respects capture mode
fn out_u64(n: u64) {
    let mut cap = OUTPUT_CAPTURE.lock();
    if cap.capturing {
        if n == 0 {
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = b'0';
                cap.len += 1;
            }
            return;
        }
        let mut buf = [0u8; 20];
        let mut i = 0;
        let mut val = n;
        while val > 0 && i < buf.len() {
            buf[i] = b'0' + (val % 10) as u8;
            val /= 10;
            i += 1;
        }
        while i > 0 {
            i -= 1;
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = buf[i];
                cap.len += 1;
            }
        }
    } else {
        drop(cap); // Release lock before UART
        uart::write_u64(n);
    }
}

/// Write hex - respects capture mode  
fn out_hex(n: u64) {
    let mut cap = OUTPUT_CAPTURE.lock();
    if cap.capturing {
        let hex_digits = b"0123456789abcdef";
        if n == 0 {
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = b'0';
                cap.len += 1;
            }
            return;
        }
        let mut buf = [0u8; 16];
        let mut i = 0;
        let mut val = n;
        while val > 0 && i < buf.len() {
            buf[i] = hex_digits[(val & 0xf) as usize];
            val >>= 4;
            i += 1;
        }
        while i > 0 {
            i -= 1;
            let idx = cap.len;
            if idx < OUTPUT_BUFFER_SIZE {
                cap.buffer[idx] = buf[i];
                cap.len += 1;
            }
        }
    } else {
        drop(cap); // Release lock before UART
        uart::write_hex(n);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RedirectMode {
    None,
    Overwrite, // >
    Append,    // >>
}

/// Read current time in milliseconds from CLINT mtime register
pub fn get_time_ms() -> i64 {
    let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
    (mtime / 10_000) as i64
}

/// Run periodic daemon work on hart 0
///
/// Services like klogd and sysmond need VirtIO access for filesystem writes.
/// Since VirtIO is only accessible from hart 0 (which runs the shell loop),
/// we call their tick functions directly from the shell loop.
fn run_hart0_tasks() {
    // Run daemon tick functions (they check their own timing internally)
    init::klogd_tick();
    init::sysmond_tick();
}

/// Check for new content in a file being followed by tail -f
/// Returns the new file size if content was found, None otherwise
fn check_tail_follow(path: &str, last_size: usize) -> Option<usize> {
    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        if let Some(content) = fs.read_file(dev, path) {
            let new_size = content.len();

            if new_size > last_size {
                // Print new content with green highlighting
                let new_content = &content[last_size..];
                if let Ok(text) = core::str::from_utf8(new_content) {
                    for line in text.lines() {
                        uart::write_str("\x1b[1;32m");
                        uart::write_str(line);
                        uart::write_line("\x1b[0m");
                    }
                }
                return Some(new_size);
            } else if new_size < last_size {
                // File was truncated
                uart::write_line("\x1b[1;33mtail: file truncated\x1b[0m");
                return Some(new_size);
            }

            return Some(last_size); // No change
        }
    }
    None
}

/// Parse a command to see if it's a tail -f command
/// Returns Some((filepath, num_lines)) if it's a follow command, None otherwise
fn parse_tail_follow_command(cmd: &[u8]) -> Option<(String, usize)> {
    let cmd_str = core::str::from_utf8(cmd).ok()?;
    let cmd_str = cmd_str.trim();

    // Must start with "tail"
    if !cmd_str.starts_with("tail ") && cmd_str != "tail" {
        return None;
    }

    // Parse arguments
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    let mut has_follow = false;
    let mut num_lines: usize = 10;
    let mut filepath: Option<&str> = None;

    let mut i = 1;
    while i < parts.len() {
        let part = parts[i];

        if part == "-f" || part == "--follow" {
            has_follow = true;
        } else if part.starts_with("-f") && part.len() > 2 {
            // -f is in combined flags like -fn20 or just -f alone
            has_follow = true;
        } else if part.starts_with("--follow=") {
            has_follow = true;
        } else if part == "-n" {
            // Next arg is number of lines
            if i + 1 < parts.len() {
                i += 1;
                if let Ok(n) = parts[i].parse::<usize>() {
                    num_lines = n;
                }
            }
        } else if part.starts_with("-n") {
            // -nNUM format
            if let Ok(n) = part[2..].parse::<usize>() {
                num_lines = n;
            }
        } else if part.starts_with("--lines=") {
            if let Ok(n) = part[8..].parse::<usize>() {
                num_lines = n;
            }
        } else if !part.starts_with("-") {
            // It's the filepath
            filepath = Some(part);
        }

        i += 1;
    }

    // Must have -f flag and a file path
    if has_follow {
        if let Some(path) = filepath {
            return Some((String::from(path), num_lines));
        }
    }

    None
}

/// Start tail follow mode for a file
/// Returns (success, initial_size)
fn start_tail_follow(path: &str, num_lines: usize) -> (bool, usize) {
    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        if let Some(content) = fs.read_file(dev, path) {
            // Show last N lines
            if let Ok(text) = core::str::from_utf8(&content) {
                let lines: Vec<&str> = text.lines().collect();
                let start = if lines.len() > num_lines {
                    lines.len() - num_lines
                } else {
                    0
                };

                for i in start..lines.len() {
                    uart::write_line(lines[i]);
                }
            }

            uart::write_line("");
            uart::write_line("\x1b[2m--- Following (Ctrl+C or 'q' to stop) ---\x1b[0m");

            return (true, content.len());
        } else {
            uart::write_str("\x1b[1;31mtail: cannot open '");
            uart::write_str(path);
            uart::write_line("': No such file\x1b[0m");
        }
    } else {
        uart::write_line("\x1b[1;31mtail: filesystem not available\x1b[0m");
    }

    (false, 0)
}

/// Print a section header
fn print_section(title: &str) {
    uart::write_line("");
    uart::write_line(
        "\x1b[1;33m────────────────────────────────────────────────────────────────────────\x1b[0m",
    );
    uart::write_str("\x1b[1;33m  ◆ ");
    uart::write_str(title);
    uart::write_line("\x1b[0m");
    uart::write_line(
        "\x1b[1;33m────────────────────────────────────────────────────────────────────────\x1b[0m",
    );
}

/// Print a boot status line
fn print_boot_status(component: &str, ok: bool) {
    if ok {
        uart::write_str("    \x1b[1;32m[✓]\x1b[0m ");
    } else {
        uart::write_str("    \x1b[1;31m[✗]\x1b[0m ");
    }
    uart::write_line(component);
}

/// Print a boot info line
fn print_boot_info(key: &str, value: &str) {
    uart::write_str("    \x1b[0;90m├─\x1b[0m ");
    uart::write_str(key);
    uart::write_str(": \x1b[1;97m");
    uart::write_str(value);
    uart::write_line("\x1b[0m");
}

#[entry]
fn main() -> ! {
    // ═══════════════════════════════════════════════════════════════════
    // VERIFY WE'RE THE PRIMARY HART
    // ═══════════════════════════════════════════════════════════════════

    let hart_id = get_hart_id();
    if hart_id != 0 {
        // Should never happen if _mp_hook works correctly
        loop {
            unsafe {
                asm!("wfi");
            }
        }
    }

    // ─── CPU & ARCHITECTURE INFO ──────────────────────────────────────────────
    print_section("CPU & ARCHITECTURE");
    print_boot_info("Primary Hart", "0");
    print_boot_info("Architecture", "RISC-V 64-bit (RV64GC)");
    print_boot_info("Mode", "Machine Mode (M-Mode)");
    print_boot_info("Timer Source", "CLINT @ 0x02000000");
    print_boot_status("CPU initialized", true);

    // ─── MEMORY SUBSYSTEM ─────────────────────────────────────────────────────
    print_section("MEMORY SUBSYSTEM");
    allocator::init();
    let total_heap = allocator::heap_size();
    uart::write_str("    \x1b[0;90m├─\x1b[0m Heap Base: \x1b[1;97m0x");
    uart::write_hex(0x8080_0000u64); // Approximate heap start
    uart::write_line("\x1b[0m");
    uart::write_str("    \x1b[0;90m├─\x1b[0m Heap Size: \x1b[1;97m");
    uart::write_u64(total_heap as u64 / 1024);
    uart::write_line(" KiB\x1b[0m");
    print_boot_status("Heap allocator ready", true);

    // ─── STORAGE SUBSYSTEM ────────────────────────────────────────────────────
    init_storage();

    // ─── SCRIPTING ENGINE ──────────────────────────────────────────────────────
    // Preload scripts from /usr/bin/ into AST cache for faster first execution
    scripting::preload_scripts();

    // ─── NETWORK SUBSYSTEM ────────────────────────────────────────────────────
    print_section("NETWORK SUBSYSTEM");
    init_network();

    // ═══════════════════════════════════════════════════════════════════
    // SMP INITIALIZATION
    // ═══════════════════════════════════════════════════════════════════

    print_section("SMP INITIALIZATION");

    // Read expected hart count from emulator (CLINT register)
    let expected_harts = get_expected_harts();
    print_boot_info("Expected harts", &format!("{}", expected_harts));

    // Memory fence ensures all init writes are visible to other harts
    fence(Ordering::SeqCst);

    // Signal that boot is complete
    BOOT_READY.store(true, Ordering::Release);

    // Register primary hart as online
    HARTS_ONLINE.fetch_add(1, Ordering::SeqCst);
    print_boot_info("Primary hart", "online");

    // Wake secondary harts via IPI
    for hart in 1..expected_harts {
        uart::write_str("    Sending IPI to hart ");
        uart::write_u64(hart as u64);
        uart::write_line("");
        send_ipi(hart);
    }

    // Wait for all harts to come online (with timeout)
    let timeout = get_time_ms() + 1000; // 1 second timeout
    while HARTS_ONLINE.load(Ordering::Acquire) < expected_harts {
        if get_time_ms() > timeout {
            uart::write_str("    \x1b[1;33m[!]\x1b[0m Warning: Only ");
            uart::write_u64(HARTS_ONLINE.load(Ordering::Relaxed) as u64);
            uart::write_str("/");
            uart::write_u64(expected_harts as u64);
            uart::write_line(" harts online after timeout");
            break;
        }
        core::hint::spin_loop();
    }

    let online = HARTS_ONLINE.load(Ordering::Relaxed);
    uart::write_str("    \x1b[1;32m[✓]\x1b[0m Harts online: ");
    uart::write_u64(online as u64);
    uart::write_str("/");
    uart::write_u64(expected_harts as u64);
    uart::write_line("");

    // ═══════════════════════════════════════════════════════════════════
    // PROCESS MANAGER INITIALIZATION
    // ═══════════════════════════════════════════════════════════════════

    print_section("PROCESS MANAGER");

    // Initialize scheduler with number of online harts
    SCHEDULER.init(online);
    print_boot_status("Scheduler initialized", true);
    print_boot_info("Run queues", &format!("{} (one per hart)", online));

    // Run init directly on primary hart (spawns daemons to secondary harts)
    // Note: We don't spawn init as a task - it runs synchronously during boot
    print_boot_info("Init process", "running");
    init::init_main();

    // Report services started
    let services = init::service_count();
    print_boot_status(
        &format!("System services started ({})", services),
        services > 0,
    );

    // ─── BOOT COMPLETE ────────────────────────────────────────────────────────
    print_section(&format!("\x1b[1;97mBAVY OS BOOT COMPLETE!\x1b[0m"));
    uart::write_line("");
    uart::write_line("");

    cwd_init();
    print_prompt();

    let console = uart::Console::new();
    let mut buffer = [0u8; 128];
    let mut len = 0usize;
    let mut count: usize = 0;
    let mut last_newline: u8 = 0; // Track last newline char to handle \r\n sequences

    // Command history
    const HISTORY_SIZE: usize = 16;
    let mut history: [[u8; 128]; HISTORY_SIZE] = [[0u8; 128]; HISTORY_SIZE];
    let mut history_lens: [usize; HISTORY_SIZE] = [0; HISTORY_SIZE];
    let mut history_count: usize = 0; // Total commands stored
    let mut history_pos: usize = 0; // Current position when navigating (0 = newest)
    let mut browsing_history: bool = false;

    // Escape sequence state
    let mut esc_state: u8 = 0; // 0 = normal, 1 = got ESC, 2 = got ESC[

    // Track last time we ran scheduled tasks
    let mut last_task_run: i64 = get_time_ms();

    // Tail follow mode state
    let mut tail_follow_mode = false;
    let mut tail_follow_path: [u8; 128] = [0u8; 128];
    let mut tail_follow_path_len: usize = 0;
    let mut tail_follow_last_size: usize = 0;
    let mut tail_follow_last_check: i64 = 0;

    loop {
        // Poll network stack
        poll_network();

        let byte = console.read_byte();

        // 0 means "no input" in our UART model
        if byte == 0 {
            // While idle, periodically run scheduled tasks on hart 0
            // Services like klogd/sysmond are pinned to hart 0 for VirtIO access
            let now = get_time_ms();
            if now - last_task_run >= 100 {
                // Every 100ms
                last_task_run = now;
                run_hart0_tasks();
            }

            // If in tail follow mode, check for new content
            if tail_follow_mode && now - tail_follow_last_check >= 200 {
                tail_follow_last_check = now;

                let path_str =
                    core::str::from_utf8(&tail_follow_path[..tail_follow_path_len]).unwrap_or("");
                if let Some(new_size) = check_tail_follow(path_str, tail_follow_last_size) {
                    tail_follow_last_size = new_size;
                }
            }

            continue;
        }

        // Check for Ctrl+C (0x03) to cancel running commands or exit follow mode
        if byte == 0x03 {
            if tail_follow_mode {
                // Exit tail follow mode
                tail_follow_mode = false;
                uart::write_line("");
                uart::write_line("\x1b[2m--- tail -f stopped ---\x1b[0m");
                print_prompt();
                len = 0;
                continue;
            }
            if cancel_running_command() {
                // Command was cancelled, print new prompt
                print_prompt();
                len = 0;
                browsing_history = false;
                history_pos = 0;
            }
            continue;
        }

        // In follow mode, 'q' also exits
        if tail_follow_mode && (byte == b'q' || byte == b'Q') {
            tail_follow_mode = false;
            uart::write_line("");
            uart::write_line("\x1b[2m--- tail -f stopped ---\x1b[0m");
            print_prompt();
            len = 0;
            continue;
        }

        // Ignore other input while in follow mode
        if tail_follow_mode {
            continue;
        }

        // Handle escape sequences for arrow keys
        if esc_state == 1 {
            if byte == b'[' {
                esc_state = 2;
                continue;
            } else {
                esc_state = 0;
                // Fall through to handle the byte normally
            }
        } else if esc_state == 2 {
            esc_state = 0;
            match byte {
                b'A' => {
                    // Up arrow - go to older command
                    if history_count > 0 {
                        let max_pos = if history_count < HISTORY_SIZE {
                            history_count
                        } else {
                            HISTORY_SIZE
                        };
                        if history_pos < max_pos {
                            if !browsing_history {
                                browsing_history = true;
                                history_pos = 0;
                            }
                            if history_pos < max_pos {
                                // Clear current line
                                clear_input_line(len);

                                // Get command from history (0 = most recent)
                                let idx =
                                    ((history_count - 1 - history_pos) % HISTORY_SIZE) as usize;
                                len = history_lens[idx];
                                buffer[..len].copy_from_slice(&history[idx][..len]);

                                // Display the command
                                uart::write_bytes(&buffer[..len]);

                                if history_pos + 1 < max_pos {
                                    history_pos += 1;
                                }
                            }
                        }
                    }
                    continue;
                }
                b'B' => {
                    // Down arrow - go to newer command
                    if browsing_history && history_pos > 0 {
                        history_pos -= 1;

                        // Clear current line
                        clear_input_line(len);

                        if history_pos == 0 {
                            // Back to empty line (current input)
                            browsing_history = false;
                            len = 0;
                        } else {
                            // Get command from history
                            let idx = ((history_count - history_pos) % HISTORY_SIZE) as usize;
                            len = history_lens[idx];
                            buffer[..len].copy_from_slice(&history[idx][..len]);

                            // Display the command
                            uart::write_bytes(&buffer[..len]);
                        }
                    } else if browsing_history {
                        // At position 0, clear and go back to empty
                        clear_input_line(len);
                        browsing_history = false;
                        len = 0;
                    }
                    continue;
                }
                b'C' | b'D' => {
                    // Right/Left arrow - ignore for now
                    continue;
                }
                _ => {
                    // Unknown escape sequence, ignore
                    continue;
                }
            }
        }

        match byte {
            0x1b => {
                // ESC - start of escape sequence
                esc_state = 1;
            }
            b'\r' | b'\n' => {
                // Skip second char of \r\n or \n\r sequence
                if (last_newline == b'\r' && byte == b'\n')
                    || (last_newline == b'\n' && byte == b'\r')
                {
                    last_newline = 0;
                    continue;
                }
                last_newline = byte;
                uart::write_line(""); // Echo the newline

                // Save to history if non-empty
                if len > 0 {
                    let idx = history_count % HISTORY_SIZE;
                    history[idx][..len].copy_from_slice(&buffer[..len]);
                    history_lens[idx] = len;
                    history_count += 1;
                }

                // Check for tail -f command (handle specially for real-time following)
                if let Some((path, num_lines)) = parse_tail_follow_command(&buffer[..len]) {
                    // Resolve the path
                    let resolved = resolve_path(&path);
                    let resolved_bytes = resolved.as_bytes();

                    // Start follow mode
                    let (success, initial_size) = start_tail_follow(&resolved, num_lines);
                    if success {
                        tail_follow_mode = true;
                        tail_follow_path_len = resolved_bytes.len().min(128);
                        tail_follow_path[..tail_follow_path_len]
                            .copy_from_slice(&resolved_bytes[..tail_follow_path_len]);
                        tail_follow_last_size = initial_size;
                        tail_follow_last_check = get_time_ms();
                        // Don't print prompt - we're in follow mode
                    } else {
                        print_prompt();
                    }
                } else {
                    handle_line(&buffer, len, &mut count);
                    print_prompt();
                }
                len = 0;
                browsing_history = false;
                history_pos = 0;
            }
            // Backspace / Delete
            8 | 0x7f => {
                if len > 0 {
                    len -= 1;
                    // Move cursor back, erase char, move back again.
                    // (Simple TTY-style backspace handling.)
                    uart::write_str("\u{8} \u{8}");
                }
            }
            // Tab - autocomplete
            b'\t' => {
                last_newline = 0;
                let new_len = handle_tab_completion(&mut buffer, len);
                len = new_len;
            }
            _ => {
                last_newline = 0; // Reset newline tracking on regular input
                if len < buffer.len() {
                    buffer[len] = byte;
                    len += 1;
                    uart::Console::new().write_byte(byte);
                }
            }
        }
    }
}

/// Clear the current input line on the terminal
fn clear_input_line(len: usize) {
    // Move cursor back and clear each character
    for _ in 0..len {
        uart::write_str("\u{8} \u{8}");
    }
}

/// Handle tab completion
/// Returns the new buffer length after completion
fn handle_tab_completion(buffer: &mut [u8], len: usize) -> usize {
    use alloc::string::String;
    use alloc::vec::Vec;

    if len == 0 {
        return 0;
    }

    let input = match core::str::from_utf8(&buffer[..len]) {
        Ok(s) => s,
        Err(_) => return len,
    };

    // Find the word being completed (last space-separated token)
    let last_space = input.rfind(' ');
    let (prefix, word_to_complete) = match last_space {
        Some(pos) => (&input[..=pos], &input[pos + 1..]),
        None => ("", input),
    };

    let is_command = prefix.is_empty();

    let mut matches: Vec<String> = Vec::new();

    if is_command {
        // Complete commands - check built-ins first
        let builtins = [
            "clear", "shutdown", "cd", "pwd", "ping", "nslookup", "node", "help", "ls", "cat",
            "echo", "cowsay", "sysinfo", "ip", "netstat", "memstats", "uptime", "write", "wget",
        ];

        for cmd in builtins.iter() {
            if cmd.starts_with(word_to_complete) {
                matches.push(String::from(*cmd));
            }
        }

        // Also check /usr/bin/ for scripts
        {
            let mut fs_guard = FS_STATE.lock();
            let mut blk_guard = BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let files = fs.list_dir(dev, "/");
                for f in files {
                    if f.name.starts_with("/usr/bin/") {
                        let script_name = &f.name[9..]; // Strip "/usr/bin/"
                        if script_name.starts_with(word_to_complete) {
                            // Avoid duplicates with builtins
                            if !matches.iter().any(|m| m == script_name) {
                                matches.push(String::from(script_name));
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Complete file/directory paths
        let path_to_complete = if word_to_complete.starts_with('/') {
            String::from(word_to_complete)
        } else {
            resolve_path(word_to_complete)
        };

        // Find the directory part and file prefix
        let (dir_path, file_prefix) = if let Some(last_slash) = path_to_complete.rfind('/') {
            if last_slash == 0 {
                ("/", &path_to_complete[1..])
            } else {
                (
                    &path_to_complete[..last_slash],
                    &path_to_complete[last_slash + 1..],
                )
            }
        } else {
            ("/", path_to_complete.as_str())
        };

        {
            let mut fs_guard = FS_STATE.lock();
            let mut blk_guard = BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let files = fs.list_dir(dev, "/");
                let mut seen_dirs: Vec<String> = Vec::new();

                for f in files {
                    // Check if file is in the target directory
                    let check_prefix = if dir_path == "/" { "/" } else { dir_path };

                    if !f.name.starts_with(check_prefix) {
                        continue;
                    }

                    // Get the part after the directory
                    let relative = if dir_path == "/" {
                        &f.name[1..]
                    } else if f.name.len() > check_prefix.len() + 1 {
                        &f.name[check_prefix.len() + 1..]
                    } else {
                        continue;
                    };

                    // Get just the immediate child (first path component)
                    let child_name = if let Some(slash_pos) = relative.find('/') {
                        &relative[..slash_pos]
                    } else {
                        relative
                    };

                    if child_name.is_empty() {
                        continue;
                    }

                    // Check if it matches the prefix
                    if !child_name.starts_with(file_prefix) {
                        continue;
                    }

                    // Check if this is a directory (has more path after)
                    let is_dir = relative.len() > child_name.len();

                    let completion = if is_dir {
                        let dir_name = String::from(child_name) + "/";
                        if seen_dirs.contains(&dir_name) {
                            continue;
                        }
                        seen_dirs.push(dir_name.clone());
                        dir_name
                    } else {
                        String::from(child_name)
                    };

                    if !matches.iter().any(|m| m == &completion) {
                        matches.push(completion);
                    }
                }
            }
        }
    }

    matches.sort();

    if matches.is_empty() {
        // No matches - beep or do nothing
        return len;
    }

    if matches.len() == 1 {
        // Single match - complete it
        let completion = &matches[0];
        let to_add = &completion[word_to_complete.len()..];

        // Add completion to buffer
        let new_len = len + to_add.len();
        if new_len <= buffer.len() {
            for (i, b) in to_add.bytes().enumerate() {
                buffer[len + i] = b;
            }
            uart::write_str(to_add);

            // Add space after command completion (not for paths ending in /)
            if is_command && new_len + 1 <= buffer.len() {
                buffer[new_len] = b' ';
                uart::write_str(" ");
                return new_len + 1;
            }

            return new_len;
        }
        return len;
    }

    // Multiple matches - find common prefix and show options
    let common = find_common_prefix(&matches);

    if common.len() > word_to_complete.len() {
        // Complete up to common prefix
        let to_add = &common[word_to_complete.len()..];
        let new_len = len + to_add.len();
        if new_len <= buffer.len() {
            for (i, b) in to_add.bytes().enumerate() {
                buffer[len + i] = b;
            }
            uart::write_str(to_add);
            return new_len;
        }
        return len;
    }

    // Show all matches
    uart::write_line("");
    let mut col = 0;
    let col_width = 16;
    let num_cols = 4;

    for m in &matches {
        let display_len = m.len();
        uart::write_str(m);

        col += 1;
        if col >= num_cols {
            uart::write_line("");
            col = 0;
        } else {
            // Pad to column width
            for _ in display_len..col_width {
                uart::write_str(" ");
            }
        }
    }
    if col > 0 {
        uart::write_line("");
    }

    // Redraw prompt and current input
    print_prompt();
    uart::write_bytes(&buffer[..len]);

    len
}

/// Find common prefix among strings
fn find_common_prefix(strings: &[alloc::string::String]) -> alloc::string::String {
    use alloc::string::String;

    if strings.is_empty() {
        return String::new();
    }

    let first = &strings[0];
    let mut prefix_len = first.len();

    for s in strings.iter().skip(1) {
        let mut common = 0;
        for (a, b) in first.chars().zip(s.chars()) {
            if a == b && common < prefix_len {
                common += 1;
            } else {
                break;
            }
        }
        prefix_len = common;
    }

    String::from(&first[..prefix_len])
}

fn init_storage() {
    print_section("STORAGE SUBSYSTEM");
    if let Some(blk) = virtio_blk::VirtioBlock::probe() {
        uart::write_str("    \x1b[0;90m├─\x1b[0m Block Device: \x1b[1;97m");
        uart::write_u64(blk.capacity() * 512 / 1024 / 1024);
        uart::write_line(" MiB\x1b[0m");
        *BLK_DEV.lock() = Some(blk);
        print_boot_status("VirtIO-Block driver loaded", true);
    } else {
        print_boot_status("No storage device found", false);
    }

    let mut blk_guard = BLK_DEV.lock();
    if let Some(ref mut blk) = *blk_guard {
        if let Some(fs) = fs::FileSystem::init(blk) {
            uart::write_line("    \x1b[1;32m[✓]\x1b[0m SFS Mounted (R/W)");
            *FS_STATE.lock() = Some(fs);
        }
    }
}

fn init_fs() {
    if let Some(blk) = virtio_blk::VirtioBlock::probe() {
        uart::write_line("    \x1b[1;32m[✓]\x1b[0m VirtIO Block found");
        *BLK_DEV.lock() = Some(blk);

        let mut blk_guard = BLK_DEV.lock();
        if let Some(ref mut dev) = *blk_guard {
            if let Some(fs) = fs::FileSystem::init(dev) {
                *FS_STATE.lock() = Some(fs);
                uart::write_line("    \x1b[1;32m[✓]\x1b[0m FileSystem Mounted");
            }
        }
    }
}

/// Initialize the network stack
fn init_network() {
    uart::write_line("    \x1b[0;90m├─\x1b[0m Probing for VirtIO devices...");

    // Probe for VirtIO network device
    match virtio_net::VirtioNet::probe() {
        Some(device) => {
            uart::write_str("    \x1b[0;90m├─\x1b[0m VirtIO-Net found at: \x1b[1;97m0x");
            uart::write_hex(device.base_addr() as u64);
            uart::write_line("\x1b[0m");

            match net::NetState::new(device) {
                Ok(state) => {
                    // Store in static FIRST, then finalize
                    {
                        let mut net_guard = NET_STATE.lock();
                        *net_guard = Some(state);
                        if let Some(ref mut s) = *net_guard {
                            s.finalize();

                            // Print network configuration
                            uart::write_line("");
                            uart::write_str("    \x1b[0m  MAC Address:   \x1b[1;97m");
                            uart::write_bytes(&s.mac_str());
                            uart::write_line("\x1b[0m                    \x1b[0m");

                            let mut ip_buf = [0u8; 16];
                            let my_ip = net::get_my_ip();
                            let ip_len = net::format_ipv4(my_ip, &mut ip_buf);
                            uart::write_str("    \x1b[0m  IPv4 Address:  \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_str("/");
                            uart::write_u64(net::PREFIX_LEN as u64);
                            uart::write_line("\x1b[0m                   \x1b[0m");

                            let gw_len = net::format_ipv4(net::GATEWAY, &mut ip_buf);
                            uart::write_str("    \x1b[0m  Gateway:       \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..gw_len]);
                            uart::write_line("\x1b[0m                       \x1b[0m");

                            let dns_len = net::format_ipv4(net::DNS_SERVER, &mut ip_buf);
                            uart::write_str("    \x1b[0m  DNS Server:    \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..dns_len]);
                            uart::write_line("\x1b[0m                       \x1b[0m");
                            uart::write_line("");
                        }
                    }
                    print_boot_status("Network stack initialized (smoltcp)", true);
                    print_boot_status("VirtIO-Net driver loaded", true);
                }
                Err(_e) => {
                    // Network initialization failed - no IP assigned
                    // Networking is disabled, NET_STATE remains None
                    uart::write_line(
                        "    \x1b[0;90m    └─ Network features will be unavailable\x1b[0m",
                    );
                }
            }
        }
        None => {
            uart::write_line("    \x1b[1;33m[!]\x1b[0m No VirtIO network device detected");
            uart::write_line("    \x1b[0;90m    └─ Network features will be unavailable\x1b[0m");
        }
    }
}

/// Cancel any running command (called when Ctrl+C is pressed)
fn cancel_running_command() -> bool {
    let running = *COMMAND_RUNNING.lock();
    if !running {
        return false;
    }

    // Check if ping is running
    let should_print_stats = {
        let ping_guard = PING_STATE.lock();
        if let Some(ref ping) = *ping_guard {
            ping.continuous
        } else {
            false
        }
    };

    if should_print_stats {
        uart::write_line("^C");
        print_ping_statistics();
        *PING_STATE.lock() = None;
        *COMMAND_RUNNING.lock() = false;
        return true;
    }

    // Generic command cancellation
    *COMMAND_RUNNING.lock() = false;
    uart::write_line("^C");
    true
}

/// Print ping statistics summary (like Linux ping)
fn print_ping_statistics() {
    let ping_guard = PING_STATE.lock();
    if let Some(ref ping) = *ping_guard {
        let mut ip_buf = [0u8; 16];
        let ip_len = net::format_ipv4(ping.target, &mut ip_buf);

        uart::write_line("");
        uart::write_str("--- ");
        uart::write_bytes(&ip_buf[..ip_len]);
        uart::write_line(" ping statistics ---");

        uart::write_u64(ping.packets_sent as u64);
        uart::write_str(" packets transmitted, ");
        uart::write_u64(ping.packets_received as u64);
        uart::write_str(" received, ");
        uart::write_u64(ping.packet_loss_percent() as u64);
        uart::write_line("% packet loss");

        if ping.packets_received > 0 {
            uart::write_str("rtt min/avg/max = ");
            uart::write_u64(ping.min_rtt as u64);
            uart::write_str("/");
            uart::write_u64(ping.avg_rtt() as u64);
            uart::write_str("/");
            uart::write_u64(ping.max_rtt as u64);
            uart::write_line(" ms");
        }
        uart::write_line("");
    }
}

/// Poll the network stack
fn poll_network() {
    let timestamp = get_time_ms();

    // First, poll the network state
    {
        let mut net_guard = NET_STATE.lock();
        if let Some(ref mut state) = *net_guard {
            state.poll(timestamp);
        }
    }

    // Then handle ping state separately to avoid holding both locks
    let mut ping_guard = PING_STATE.lock();
    if let Some(ref mut ping) = *ping_guard {
        // Check for ping reply
        if ping.waiting {
            let reply = {
                let mut net_guard = NET_STATE.lock();
                if let Some(ref mut state) = *net_guard {
                    state.check_ping_reply()
                } else {
                    None
                }
            };

            if let Some((from, _ident, seq)) = reply {
                if seq == ping.seq {
                    let rtt = timestamp - ping.sent_time;
                    ping.record_reply(rtt);

                    let mut ip_buf = [0u8; 16];
                    let ip_len = net::format_ipv4(from, &mut ip_buf);
                    uart::write_str("64 bytes from ");
                    uart::write_bytes(&ip_buf[..ip_len]);
                    uart::write_str(": icmp_seq=");
                    uart::write_u64(seq as u64);
                    uart::write_str(" time=");
                    uart::write_u64(rtt as u64);
                    uart::write_line(" ms");
                    ping.waiting = false;
                }
            }

            // Timeout after 5 seconds for current ping
            if timestamp - ping.sent_time > 5000 {
                uart::write_str("Request timeout for icmp_seq ");
                uart::write_u64(ping.seq as u64);
                uart::write_line("");
                ping.waiting = false;
            }
        }

        // In continuous mode, send next ping after 1 second interval
        if ping.continuous && !ping.waiting {
            if timestamp - ping.last_send_time >= 1000 {
                ping.seq = ping.seq.wrapping_add(1);
                ping.sent_time = timestamp;
                ping.last_send_time = timestamp;
                ping.packets_sent += 1;

                let send_result = {
                    let mut net_guard = NET_STATE.lock();
                    if let Some(ref mut state) = *net_guard {
                        state.send_ping(ping.target, ping.seq, timestamp)
                    } else {
                        Err("Network not available")
                    }
                };

                match send_result {
                    Ok(()) => {
                        ping.waiting = true;
                    }
                    Err(_e) => {
                        // Failed to send, will retry next interval
                    }
                }
            }
        }
    }
}

fn print_prompt() {
    let cwd = cwd_get();
    let prompt_path = if cwd == "/" {
        String::new()
    } else {
        format!(" {}", cwd)
    };

    uart::write_str(&format!(
        "\x1b[1;35mBavy\x1b[0m\x1b[1;34m{}\x1b[0m # ",
        prompt_path
    ));
}

/// Parse a command line for redirection operators
/// Returns: (command_part, redirect_mode, filename)
fn parse_redirection(line: &[u8]) -> (&[u8], RedirectMode, &[u8]) {
    // Look for >> first (must check before >)
    for i in 0..line.len().saturating_sub(1) {
        if line[i] == b'>' && line[i + 1] == b'>' {
            let cmd_part = trim_bytes(&line[..i]);
            let file_part = trim_bytes(&line[i + 2..]);
            return (cmd_part, RedirectMode::Append, file_part);
        }
    }

    // Look for single >
    for i in 0..line.len() {
        if line[i] == b'>' {
            let cmd_part = trim_bytes(&line[..i]);
            let file_part = trim_bytes(&line[i + 1..]);
            return (cmd_part, RedirectMode::Overwrite, file_part);
        }
    }

    (line, RedirectMode::None, &[])
}

/// Trim whitespace from byte slice
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();

    while start < end && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    while end > start && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }

    &bytes[start..end]
}

fn handle_line(buffer: &[u8], len: usize, _count: &mut usize) {
    // Trim leading/trailing whitespace (spaces and tabs only)
    let mut start = 0;
    let mut end = len;

    while start < end && (buffer[start] == b' ' || buffer[start] == b'\t') {
        start += 1;
    }
    while end > start && (buffer[end - 1] == b' ' || buffer[end - 1] == b'\t') {
        end -= 1;
    }

    if start >= end {
        // Empty line -> do nothing
        return;
    }

    let full_line = &buffer[start..end];

    // Parse for redirection
    let (line, redirect_mode, redirect_file) = parse_redirection(full_line);

    // Validate redirection target
    if redirect_mode != RedirectMode::None && redirect_file.is_empty() {
        uart::write_line("");
        uart::write_line("\x1b[1;31mError:\x1b[0m Missing filename for redirection");
        return;
    }

    // Split into command and arguments (first whitespace)
    let mut i = 0;
    while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
        i += 1;
    }
    let cmd = &line[..i];

    let mut arg_start = i;
    while arg_start < line.len() && (line[arg_start] == b' ' || line[arg_start] == b'\t') {
        arg_start += 1;
    }
    let args = &line[arg_start..];

    // Start capturing if redirecting
    if redirect_mode != RedirectMode::None {
        output_capture_start();
    }

    // Execute the command
    execute_command(cmd, args);

    // Handle redirection output
    if redirect_mode != RedirectMode::None {
        let output = output_capture_stop();

        if let Ok(filename) = core::str::from_utf8(redirect_file) {
            let filename = filename.trim();
            // Resolve path relative to CWD
            let resolved_path = resolve_path(filename);

            let mut fs_guard = FS_STATE.lock();
            let mut blk_guard = BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let final_data = if redirect_mode == RedirectMode::Append {
                    // Read existing file content and append
                    let mut combined = match fs.read_file(dev, &resolved_path) {
                        Some(existing) => existing,
                        None => Vec::new(),
                    };
                    combined.extend_from_slice(&output);
                    combined
                } else {
                    // Overwrite mode - just use new output
                    output
                };

                match fs.write_file(dev, &resolved_path, &final_data) {
                    Ok(()) => {
                        // Sync to ensure data is written to disk
                        let _ = fs.sync(dev);
                        uart::write_line("");
                        uart::write_str("\x1b[1;32m✓\x1b[0m Output written to ");
                        uart::write_line(&resolved_path);
                    }
                    Err(e) => {
                        uart::write_line("");
                        uart::write_str("\x1b[1;31mError:\x1b[0m Failed to write to file: ");
                        uart::write_line(e);
                    }
                }
            } else {
                uart::write_line("");
                uart::write_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
            }
        } else {
            uart::write_line("");
            uart::write_line("\x1b[1;31mError:\x1b[0m Invalid filename");
        }
    }
}

/// Execute a command (separated for cleaner redirection handling)
///
/// Commands are resolved in this order:
/// 1. Essential built-in commands (that require direct kernel access)
/// 2. Native commands (fast Rust implementations of common utilities)
/// 3. Scripts: searched in root, then /usr/bin/ directory (PATH-like)
fn execute_command(cmd: &[u8], args: &[u8]) {
    let cmd_str = core::str::from_utf8(cmd).unwrap_or("");
    let args_str = core::str::from_utf8(args).unwrap_or("");

    // ═══════════════════════════════════════════════════════════════════════════
    // ESSENTIAL BUILT-IN COMMANDS
    // These require direct kernel access or cannot be implemented in scripts
    // ═══════════════════════════════════════════════════════════════════════════

    match cmd_str {
        // System control - requires direct hardware access
        "shutdown" | "poweroff" => {
            cmd::shutdown();
            return;
        }
        "clear" => {
            for _ in 0..50 {
                out_line("");
            }
            return;
        }

        // Directory navigation - requires shell state
        "cd" => {
            cmd::cd(args_str);
            return;
        }
        "pwd" => {
            out_line(&cwd_get());
            return;
        }

        // Scripting engine control
        "node" => {
            cmd::node(args);
            return;
        }

        // Async network commands - require event loop integration
        "ping" => {
            cmd::ping(args);
            return;
        }
        "nslookup" => {
            cmd::nslookup(args);
            return;
        }

        // Low-level debugging commands
        "readsec" => {
            cmd::readsec(args);
            return;
        }
        "alloc" => {
            cmd::alloc(args);
            return;
        }
        "memtest" => {
            cmd::memtest(args);
            return;
        }

        // CPU benchmark
        "cpuTest" | "cputest" => {
            cmd::cputest(args);
            return;
        }

        // Help - try script first, fall back to built-in
        "help" => {
            // First try to run help script
            if let Some(script_bytes) = scripting::find_script("help") {
                run_script_bytes(&script_bytes, args_str);
                return;
            }
            // Fallback to built-in help
            cmd::help();
            return;
        }

        _ => {}
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // NATIVE COMMANDS (Fast implementations)
    // These are common utilities implemented in Rust for maximum performance.
    // Scripts in /usr/bin/ are still available for customization.
    // ═══════════════════════════════════════════════════════════════════════════

    if cmd::try_native(cmd_str, args_str) {
        return;
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // SCRIPT RESOLUTION (PATH-like)
    // Fallback to script-based commands for flexibility/customization
    // ═══════════════════════════════════════════════════════════════════════════

    if let Some(script_bytes) = scripting::find_script(cmd_str) {
        run_script_bytes(&script_bytes, args_str);
        return;
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // COMMAND NOT FOUND
    // ═══════════════════════════════════════════════════════════════════════════

    out_str("\x1b[1;31mCommand not found:\x1b[0m ");
    out_line(cmd_str);
    out_line("\x1b[0;90mTry 'help' for available commands, or check /usr/bin/ for scripts\x1b[0m");
}

/// Run a script from its bytes
fn run_script_bytes(bytes: &[u8], args: &str) {
    let script = unsafe { core::str::from_utf8_unchecked(bytes) };
    match scripting::execute_script(script, args) {
        Ok(output) => {
            if !output.is_empty() {
                out_str(&output);
            }
        }
        Err(e) => {
            out_str("\x1b[1;31mScript error:\x1b[0m ");
            out_line(&e);
        }
    }
}

/// Resolve a path relative to CWD
pub fn resolve_path(path: &str) -> alloc::string::String {
    use alloc::string::String;
    use alloc::vec::Vec;

    let mut result = String::new();

    // Start from root or CWD
    let cwd = cwd_get();
    let base: &str = if path.starts_with('/') { "/" } else { &cwd };

    // Combine base and path, then normalize
    let full = if path.starts_with('/') {
        String::from(path)
    } else if base == "/" {
        let mut s = String::from("/");
        s.push_str(path);
        s
    } else {
        let mut s = String::from(base);
        s.push('/');
        s.push_str(path);
        s
    };

    // Split and normalize (handle . and ..)
    let mut parts: Vec<&str> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }

    // Rebuild path
    result.push('/');
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            result.push('/');
        }
        result.push_str(part);
    }

    if result.is_empty() {
        result.push('/');
    }

    result
}

/// Check if a path exists (has files under it or is a file)
fn path_exists(path: &str) -> bool {
    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();
    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        // Root always exists
        if path == "/" {
            return true;
        }

        let files = fs.list_dir(dev, "/");
        let path_with_slash = if path.ends_with('/') {
            alloc::string::String::from(path)
        } else {
            let mut s = alloc::string::String::from(path);
            s.push('/');
            s
        };

        for file in files {
            // Check if any file starts with this path (it's a directory)
            if file.name.starts_with(&path_with_slash) {
                return true;
            }
            // Or if it exactly matches (it's a file)
            if file.name == path {
                return true;
            }
        }
    }
    false
}

fn cmd_shutdown() {
    uart::write_line("");
    uart::write_line(
        "\x1b[1;31m╔═══════════════════════════════════════════════════════════════════╗\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                    \x1b[1;97mSystem Shutdown Initiated\x1b[0m                       \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m╚═══════════════════════════════════════════════════════════════════╝\x1b[0m",
    );
    uart::write_line("");
    uart::write_line("    \x1b[0;90m[1/3]\x1b[0m Syncing filesystems...");
    uart::write_line("    \x1b[0;90m[2/3]\x1b[0m Stopping network services...");
    uart::write_line("    \x1b[0;90m[3/3]\x1b[0m Powering off CPU...");
    uart::write_line("");
    uart::write_line("    \x1b[1;32m✓ Goodbye!\x1b[0m");
    uart::write_line("");

    // Write to the test finisher address to signal the VM to stop
    // Value 0x5555 indicates successful exit (PASS)
    unsafe {
        core::ptr::write_volatile(TEST_FINISHER as *mut u32, 0x5555);
    }
    // Should not reach here, but loop just in case
    loop {}
}

fn eq_cmd(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
