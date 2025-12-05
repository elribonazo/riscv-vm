//! Init system - PID 1 process
//!
//! The init process is responsible for:
//! - Spawning system services (daemons)
//! - Running startup scripts from /etc/init.d/
//! - Reaping zombie processes
//! - System shutdown coordination
//!
//! Similar to Linux's init/systemd but much simpler.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::klog::{klog_error, klog_info};
use crate::scheduler::SCHEDULER;
use crate::task::Priority;
use crate::Spinlock;

/// Init system state
static INIT_STATE: Spinlock<InitState> = Spinlock::new(InitState::new());

/// Whether init has completed startup
static INIT_COMPLETE: AtomicBool = AtomicBool::new(false);

/// Number of services started
static SERVICES_STARTED: AtomicUsize = AtomicUsize::new(0);

/// Service status
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    Stopped,
    Running,
    Failed,
}

impl ServiceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceStatus::Stopped => "stopped",
            ServiceStatus::Running => "running",
            ServiceStatus::Failed => "failed",
        }
    }
}

/// Service definition - describes a service that can be started/stopped
#[derive(Clone)]
pub struct ServiceDef {
    pub name: String,
    pub description: String,
    pub entry: crate::task::TaskEntry,
    pub priority: crate::task::Priority,
    pub preferred_hart: Option<usize>,
}

/// Service runtime info
#[derive(Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub pid: u32,
    pub status: ServiceStatus,
    pub started_at: u64,
    pub hart: Option<usize>,
}

/// Init state
struct InitState {
    /// Registered service definitions
    service_defs: Vec<ServiceDef>,
    /// Running services
    services: Vec<ServiceInfo>,
}

impl InitState {
    const fn new() -> Self {
        Self {
            service_defs: Vec::new(),
            services: Vec::new(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INIT PROCESS
// ═══════════════════════════════════════════════════════════════════════════════

/// Init process entry point - PID 1
///
/// This runs on the primary hart and is responsible for bringing up the system.
pub fn init_main() {
    klog_info("init", "Starting init system (PID 1)");

    // Phase 1: Create required directories
    klog_info("init", "Phase 1: Creating system directories");
    ensure_directories();

    // Phase 2: Start system services
    klog_info("init", "Phase 2: Starting system services");
    start_system_services();

    // Phase 3: Run init scripts
    klog_info("init", "Phase 3: Running init scripts");
    run_init_scripts();

    // Mark init complete
    INIT_COMPLETE.store(true, Ordering::Release);

    let services = SERVICES_STARTED.load(Ordering::Relaxed);
    klog_info(
        "init",
        &format!("Init complete. {} services started.", services),
    );

    // Write initial boot message to kernel.log
    write_boot_log();

    // Init process is done - it doesn't need to loop
    // The scheduler will continue running other tasks
}

/// Ensure required system directories exist
fn ensure_directories() {
    let dirs = ["/var", "/var/log", "/var/run", "/etc", "/tmp"];

    for dir in &dirs {
        // For our simple FS, we just ensure we can write a marker file
        // A real FS would have proper directory support
        // Directory ensured: dir (no-op in our simple FS)
        let _ = dir;
    }
}

/// Start core system services
fn start_system_services() {
    let num_harts = crate::HARTS_ONLINE.load(Ordering::Relaxed);
    klog_info(
        "init",
        &format!("{} harts available for parallel tasks", num_harts),
    );

    // Register service definitions (available services)
    //
    // NOTE: All daemons are pinned to hart 0 (Some(0)) because in WASM multi-hart
    // mode, only hart 0 (main thread) has VirtIO access. Secondary harts (workers)
    // only share DRAM via SharedArrayBuffer and cannot access disk/network.
    //
    // In native mode, all harts share Arc<SystemBus> with VirtIO, but pinning to
    // hart 0 is still safe and keeps behavior consistent across platforms.
    register_service_def(
        "klogd",
        "Kernel logger daemon - logs system memory stats",
        klogd_service,
        Priority::Normal,
        Some(0), // Pin to hart 0 - has VirtIO access in both native and WASM
    );

    register_service_def(
        "sysmond",
        "System monitor daemon - monitors system health",
        sysmond_service,
        Priority::Normal,
        Some(0), // Pin to hart 0 - has VirtIO access in both native and WASM
    );

    // Auto-start daemons (they're pinned to hart 0, safe in all modes)
    if let Ok(()) = start_service("klogd") {
        klog_info("init", "Auto-started klogd on hart 0");
    }
    if let Ok(()) = start_service("sysmond") {
        klog_info("init", "Auto-started sysmond on hart 0");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PUBLIC SERVICE CONTROL API
// ═══════════════════════════════════════════════════════════════════════════════

/// Start a service by name
/// Returns Ok(()) on success, Err(message) on failure
pub fn start_service(name: &str) -> Result<(), &'static str> {
    let state = INIT_STATE.lock();

    // Check if already running
    if let Some(svc) = state.services.iter().find(|s| s.name == name) {
        if svc.status == ServiceStatus::Running {
            return Err("Service is already running");
        }
    }

    // Find service definition
    let def = state
        .service_defs
        .iter()
        .find(|d| d.name == name)
        .ok_or("Service not found")?;

    let entry = def.entry;
    let priority = def.priority;
    let preferred_hart = def.preferred_hart;
    let name_owned = def.name.clone();

    drop(state); // Release lock before spawning

    // Spawn the service
    let pid = SCHEDULER.spawn_daemon_on_hart(&name_owned, entry, priority, preferred_hart);

    // Register as running
    register_service(&name_owned, pid, preferred_hart);

    // Wake the target hart
    if let Some(hart) = preferred_hart {
        crate::send_ipi(hart);
    }

    Ok(())
}

/// Stop a service by name
/// Returns Ok(()) on success, Err(message) on failure
pub fn stop_service(name: &str) -> Result<(), &'static str> {
    let state = INIT_STATE.lock();

    // Find the running service
    let svc = state
        .services
        .iter()
        .find(|s| s.name == name)
        .ok_or("Service not found")?;

    if svc.status != ServiceStatus::Running {
        return Err("Service is not running");
    }

    let pid = svc.pid;
    drop(state); // Release lock before killing

    // Kill the service task
    if pid > 0 {
        SCHEDULER.kill(pid);
    }

    // Mark as stopped
    mark_service_stopped(name);

    Ok(())
}

/// Restart a service by name
/// Returns Ok(()) on success, Err(message) on failure
pub fn restart_service(name: &str) -> Result<(), &'static str> {
    // Stop if running (ignore error if not running)
    let _ = stop_service(name);

    // Small delay to let things settle
    for _ in 0..10000 {
        core::hint::spin_loop();
    }

    // Start the service
    start_service(name)
}

/// Get status of a service
pub fn service_status(name: &str) -> Option<ServiceStatus> {
    let state = INIT_STATE.lock();
    state
        .services
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.status)
}

/// Get detailed info about a service
pub fn get_service_info(name: &str) -> Option<ServiceInfo> {
    let state = INIT_STATE.lock();
    state.services.iter().find(|s| s.name == name).cloned()
}

/// List all registered services (definitions)
pub fn list_service_defs() -> Vec<(String, String)> {
    let state = INIT_STATE.lock();
    state
        .service_defs
        .iter()
        .map(|d| (d.name.clone(), d.description.clone()))
        .collect()
}

/// Register a service definition (what the service is and how to start it)
fn register_service_def(
    name: &str,
    description: &str,
    entry: crate::task::TaskEntry,
    priority: crate::task::Priority,
    preferred_hart: Option<usize>,
) {
    let mut state = INIT_STATE.lock();
    state.service_defs.push(ServiceDef {
        name: String::from(name),
        description: String::from(description),
        entry,
        priority,
        preferred_hart,
    });
}

/// Register a running service instance
fn register_service(name: &str, pid: u32, hart: Option<usize>) {
    let mut state = INIT_STATE.lock();

    // Update existing or add new
    if let Some(svc) = state.services.iter_mut().find(|s| s.name == name) {
        svc.pid = pid;
        svc.status = ServiceStatus::Running;
        svc.started_at = crate::get_time_ms() as u64;
        svc.hart = hart;
    } else {
        state.services.push(ServiceInfo {
            name: String::from(name),
            pid,
            status: ServiceStatus::Running,
            started_at: crate::get_time_ms() as u64,
            hart,
        });
    }
    SERVICES_STARTED.fetch_add(1, Ordering::Relaxed);
}

/// Mark a service as stopped
fn mark_service_stopped(name: &str) {
    let mut state = INIT_STATE.lock();
    if let Some(svc) = state.services.iter_mut().find(|s| s.name == name) {
        svc.status = ServiceStatus::Stopped;
        svc.pid = 0;
        svc.hart = None;
    }
}

/// Run init scripts from /etc/init.d/
fn run_init_scripts() {
    let mut fs_guard = crate::FS_STATE.lock();
    let mut blk_guard = crate::BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        // Look for init scripts
        let files = fs.list_dir(dev, "/");
        for file in files {
            if file.name.starts_with("/etc/init.d/") {
                let script_name = &file.name[12..]; // Strip "/etc/init.d/"
                klog_info("init", &format!("Running init script: {}", script_name));

                // Read and execute the script
                if let Some(content) = fs.read_file(dev, &file.name) {
                    if let Ok(script) = core::str::from_utf8(&content) {
                        drop(blk_guard);
                        drop(fs_guard);

                        // Execute via scripting engine
                        match crate::scripting::execute_script(script, "") {
                            Ok(output) => {
                                if !output.is_empty() {
                                    klog_info("init", &format!("Script output: {}", output.trim()));
                                }
                            }
                            Err(e) => {
                                klog_error("init", &format!("Script error: {}", e));
                            }
                        }
                        return; // Re-acquire locks would be complex, just return
                    }
                }
            }
        }
    }
}

/// Write boot information to kernel.log
fn write_boot_log() {
    let timestamp = crate::get_time_ms();
    let num_harts = crate::HARTS_ONLINE.load(Ordering::Relaxed);
    let services = SERVICES_STARTED.load(Ordering::Relaxed);

    let boot_msg = format!(
        "=== BAVY OS Boot Log ===\n\
         Boot time: {}ms\n\
         Harts online: {}\n\
         Services started: {}\n\
         ========================\n",
        timestamp, num_harts, services
    );

    // Write to kernel.log
    let mut fs_guard = crate::FS_STATE.lock();
    let mut blk_guard = crate::BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        if let Err(e) = fs.write_file(dev, "/var/log/kernel.log", boot_msg.as_bytes()) {
            klog_error("init", &format!("Failed to write boot log: {}", e));
        } else {
            // Sync to ensure data is written to disk
            let _ = fs.sync(dev);
            klog_info("init", "Boot log written to /var/log/kernel.log");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SYSTEM SERVICES (long-running daemons on secondary harts)
// ═══════════════════════════════════════════════════════════════════════════════

/// Spin-delay for approximately the given milliseconds
/// Uses busy-waiting since secondary harts don't have timer interrupts
#[inline(never)]
fn spin_delay_ms(ms: u64) {
    let start = crate::get_time_ms() as u64;
    let target = start + ms;
    while (crate::get_time_ms() as u64) < target {
        // Yield CPU hints to save power
        for _ in 0..100 {
            core::hint::spin_loop();
        }
    }
}

/// Append a line to the kernel log file
/// Returns true on success
fn append_to_log(line: &str) -> bool {
    let mut fs_guard = crate::FS_STATE.lock();
    let mut blk_guard = crate::BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        // Read existing content
        let existing = fs
            .read_file(dev, "/var/log/kernel.log")
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();

        // Truncate if too large (keep last 16KB)
        let trimmed = if existing.len() > 16384 {
            String::from(&existing[existing.len() - 16384..])
        } else {
            existing
        };

        let new_content = format!("{}{}\n", trimmed, line);

        if fs
            .write_file(dev, "/var/log/kernel.log", new_content.as_bytes())
            .is_ok()
        {
            // Sync to ensure data is written to disk
            let _ = fs.sync(dev);
            return true;
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════════════
// COOPERATIVE DAEMON TICKS
// These functions do one unit of work and return immediately.
// Called from the shell loop on hart 0.
// ═══════════════════════════════════════════════════════════════════════════════

use core::sync::atomic::AtomicI64;

/// State for klogd daemon
static KLOGD_LAST_RUN: AtomicI64 = AtomicI64::new(0);
static KLOGD_TICK: AtomicUsize = AtomicUsize::new(0);
static KLOGD_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// State for sysmond daemon  
static SYSMOND_LAST_RUN: AtomicI64 = AtomicI64::new(0);
static SYSMOND_TICK: AtomicUsize = AtomicUsize::new(0);
static SYSMOND_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Run klogd work if 5 seconds have passed since last run
pub fn klogd_tick() {
    let now = crate::get_time_ms();
    let last = KLOGD_LAST_RUN.load(Ordering::Relaxed);

    // First run: initialize
    if !KLOGD_INITIALIZED.load(Ordering::Relaxed) {
        KLOGD_INITIALIZED.store(true, Ordering::Relaxed);
        KLOGD_LAST_RUN.store(now, Ordering::Relaxed);

        let startup_msg = format!(
            "══════════════════════════════════════════════════════════════\n\
             BAVY OS - Kernel Logger Started\n\
             ══════════════════════════════════════════════════════════════\n\
             Time: {}ms | Hart: 0 | klogd daemon initialized\n\
             ──────────────────────────────────────────────────────────────",
            now
        );
        append_to_log(&startup_msg);
        return;
    }

    // Check if 5 seconds have passed
    if now - last < 5000 {
        return;
    }

    KLOGD_LAST_RUN.store(now, Ordering::Relaxed);
    let tick = KLOGD_TICK.fetch_add(1, Ordering::Relaxed) + 1;

    let (heap_used, _heap_free) = crate::allocator::heap_stats();
    let heap_total = crate::allocator::heap_size();
    let usage_pct = (heap_used * 100) / heap_total.max(1);

    let log_entry = format!(
        "[{:>10}ms] klogd #{}: mem={}%({}/{}KB)",
        now,
        tick,
        usage_pct,
        heap_used / 1024,
        heap_total / 1024,
    );

    append_to_log(&log_entry);
}

/// Run sysmond work if 10 seconds have passed since last run
pub fn sysmond_tick() {
    let now = crate::get_time_ms();
    let last = SYSMOND_LAST_RUN.load(Ordering::Relaxed);

    // First run: initialize (with 2 second delay after klogd)
    if !SYSMOND_INITIALIZED.load(Ordering::Relaxed) {
        if now < 2000 {
            return; // Wait for initial delay
        }
        SYSMOND_INITIALIZED.store(true, Ordering::Relaxed);
        SYSMOND_LAST_RUN.store(now, Ordering::Relaxed);

        let startup_msg = format!("[{:>10}ms] sysmond started on hart 0", now);
        append_to_log(&startup_msg);
        return;
    }

    // Check if 10 seconds have passed
    if now - last < 10000 {
        return;
    }

    SYSMOND_LAST_RUN.store(now, Ordering::Relaxed);
    let tick = SYSMOND_TICK.fetch_add(1, Ordering::Relaxed) + 1;

    // Collect system stats
    let task_count = SCHEDULER.task_count();
    let queued = SCHEDULER.queued_count();
    let num_harts = crate::HARTS_ONLINE.load(Ordering::Relaxed);

    let net_ok = crate::NET_STATE.lock().is_some();
    let fs_ok = crate::FS_STATE.lock().is_some();

    let log_entry = format!(
        "[{:>10}ms] sysmond #{}: harts={} tasks={} queued={} net={} fs={}",
        now,
        tick,
        num_harts,
        task_count,
        queued,
        if net_ok { "UP" } else { "DOWN" },
        if fs_ok { "OK" } else { "ERR" },
    );

    append_to_log(&log_entry);

    // Reap zombie processes
    let reaped = SCHEDULER.reap_zombies();
    if reaped > 0 {
        let reap_msg = format!(
            "[{:>10}ms] sysmond: reaped {} zombie process(es)",
            crate::get_time_ms(),
            reaped
        );
        append_to_log(&reap_msg);
    }
}

/// Legacy service entry points (for task scheduler compatibility)
/// These are no longer used directly but kept for API compatibility
pub fn klogd_service() {
    // Single tick - for scheduler-based execution
    klogd_tick();
}

pub fn sysmond_service() {
    // Single tick - for scheduler-based execution
    sysmond_tick();
}

// ═══════════════════════════════════════════════════════════════════════════════
// UTILITY FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if init has completed
pub fn is_init_complete() -> bool {
    INIT_COMPLETE.load(Ordering::Acquire)
}

/// Get list of all services (running and stopped)
pub fn list_services() -> Vec<ServiceInfo> {
    let state = INIT_STATE.lock();

    // Return all services, adding stopped ones from definitions
    let mut result = state.services.clone();

    // Add any defined services that aren't in the running list
    for def in &state.service_defs {
        if !result.iter().any(|s| s.name == def.name) {
            result.push(ServiceInfo {
                name: def.name.clone(),
                pid: 0,
                status: ServiceStatus::Stopped,
                started_at: 0,
                hart: None,
            });
        }
    }

    result
}

/// Get number of services started
pub fn service_count() -> usize {
    SERVICES_STARTED.load(Ordering::Relaxed)
}
