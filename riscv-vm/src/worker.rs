//! Web Worker entry point for WASM SMP.
//!
//! This module provides the Rust entry point that runs inside each Web Worker,
//! executing CPU instructions in parallel with other workers and the main thread.
//!
//! ## Architecture
//!
//! - Each worker runs one secondary hart (1, 2, 3, ...)
//! - Hart 0 runs on the main thread (handles I/O)
//! - Workers share DRAM and CLINT via SharedArrayBuffer
//! - Workers do NOT have access to VirtIO devices (disk, network)
//! - Inter-hart communication uses CLINT MSIP for IPIs
//!
//! ## Memory Layout
//!
//! The SharedArrayBuffer is laid out as:
//! - Control region (4KB): halt flags, hart count
//! - CLINT region (64KB): mtime, msip[], mtimecmp[]
//! - DRAM region: kernel memory

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use js_sys::SharedArrayBuffer;
#[cfg(target_arch = "wasm32")]
use crate::shared_mem::{self, wasm::{SharedClint, SharedControl}};
#[cfg(target_arch = "wasm32")]
use crate::bus::SystemBus;
#[cfg(target_arch = "wasm32")]
use crate::cpu::Cpu;
#[cfg(target_arch = "wasm32")]
use crate::Trap;

/// Result of executing a batch of instructions.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkerStepResult {
    /// Continue executing - call step_batch again
    Continue = 0,
    /// Halt requested via control region
    Halted = 1,
    /// Shutdown requested by guest (RequestedTrap)
    Shutdown = 2,
    /// Fatal error occurred
    Error = 3,
}

/// Worker state stored in JS (passed back to Rust on each step_batch call).
/// This avoids recreating CPU/bus state on every call.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct WorkerState {
    cpu: Cpu,
    bus: SystemBus,
    control: SharedControl,
    clint: SharedClint,
    hart_id: usize,
    step_count: u64,
    /// Cached flag: have we received the "workers can start" signal?
    /// Once set to true, we never need to check again (reduces atomic ops).
    workers_started: bool,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WorkerState {
    /// Create a new worker state for a secondary hart.
    #[wasm_bindgen(constructor)]
    pub fn new(hart_id: usize, shared_mem: JsValue, entry_pc: u64) -> WorkerState {
        // Convert JsValue to SharedArrayBuffer
        let sab: SharedArrayBuffer = shared_mem.unchecked_into();
        
        // Create shared control and CLINT accessors
        let control = SharedControl::new(&sab);
        let clint = SharedClint::new(&sab);
        
        // Create bus view of shared DRAM
        let dram_offset = shared_mem::dram_offset();
        let shared_clint_for_bus = SharedClint::new(&sab);
        // Workers read from shared UART input (is_worker = true)
        let bus = SystemBus::from_shared_buffer(sab, dram_offset, shared_clint_for_bus, true);
        
        // Create CPU for this hart
        let cpu = Cpu::new(entry_pc, hart_id as u64);
        
        web_sys::console::log_1(&JsValue::from_str(
            &format!("[Worker {}] Initialized at PC=0x{:x}", hart_id, entry_pc)));
        
        WorkerState {
            cpu,
            bus,
            control,
            clint,
            hart_id,
            step_count: 0,
            workers_started: false, // Will be cached on first check
        }
    }
    
    /// Execute a batch of instructions and return.
    /// 
    /// This is designed to be called repeatedly from JavaScript, allowing
    /// the event loop to yield between batches. This prevents the worker
    /// from blocking indefinitely and allows it to respond to messages.
    ///
    /// Performance optimization: We reduce atomic operations by:
    /// - Only checking halt signals every HALT_CHECK_INTERVAL instructions
    /// - Only checking interrupts every INTERRUPT_CHECK_INTERVAL instructions
    /// - Doing a full interrupt check at the end of each batch
    ///
    /// Returns a WorkerStepResult indicating whether to continue, halt, etc.
    pub fn step_batch(&mut self, batch_size: u32) -> WorkerStepResult {
        // Check intervals - reduce atomic operations overhead
        // Higher values = better performance, but less responsive to signals
        const HALT_CHECK_INTERVAL: u32 = 10_000;
        const INTERRUPT_CHECK_INTERVAL: u32 = 5_000;
        
        // Check for halt request first (one atomic check at batch start)
        if self.control.should_stop() {
            web_sys::console::log_1(&JsValue::from_str(
                &format!("[Worker {}] Halt detected after {} steps", 
                    self.hart_id, self.step_count)));
            return WorkerStepResult::Halted;
        }
        
        // Wait for main thread to signal that workers can start.
        // This ensures hart 0 boots first and sets up memory before
        // secondary harts start executing kernel code.
        // 
        // Optimization: Cache the result once workers are allowed to start.
        // This eliminates an atomic operation per batch after startup.
        if !self.workers_started {
            if !self.control.can_workers_start() {
                // Still parked - return Continue to keep polling
                return WorkerStepResult::Continue;
            }
            // Workers can start - cache this permanently
            self.workers_started = true;
        }
        
        // Execute batch of instructions with reduced atomic operation frequency
        for i in 0..batch_size {
            // Periodic halt check (much less frequent than per-instruction)
            if i > 0 && i % HALT_CHECK_INTERVAL == 0 {
                if self.control.should_stop() {
                    web_sys::console::log_1(&JsValue::from_str(
                        &format!("[Worker {}] Halt detected during batch after {} steps", 
                            self.hart_id, self.step_count)));
                    return WorkerStepResult::Halted;
                }
            }
            
            // Periodic interrupt check (less frequent than halt check)
            if i > 0 && i % INTERRUPT_CHECK_INTERVAL == 0 {
                self.deliver_interrupts();
            }
            
            match self.cpu.step(&self.bus) {
                Ok(()) => {
                    self.step_count += 1;
                }
                Err(Trap::RequestedTrap(code)) => {
                    web_sys::console::log_1(&JsValue::from_str(
                        &format!("[Worker {}] Shutdown requested (code: {:#x})", 
                            self.hart_id, code)));
                    self.control.signal_halted(code);
                    return WorkerStepResult::Shutdown;
                }
                Err(Trap::Fatal(msg)) => {
                    web_sys::console::error_1(&JsValue::from_str(
                        &format!("[Worker {}] Fatal: {} at PC=0x{:x}", 
                            self.hart_id, msg, self.cpu.pc)));
                    self.control.signal_halted(0xDEAD);
                    return WorkerStepResult::Error;
                }
                Err(_trap) => {
                    // Architectural traps handled by CPU
                    self.step_count += 1;
                }
            }
        }
        
        // Full interrupt check at end of batch
        self.deliver_interrupts();
        
        WorkerStepResult::Continue
    }
    
    /// Check and deliver interrupts from shared CLINT.
    /// Separated into its own method to allow periodic calling during batch execution.
    #[inline]
    fn deliver_interrupts(&mut self) {
        let (msip_pending, timer_pending) = self.clint.check_interrupts(self.hart_id);
        if msip_pending || timer_pending {
            if let Ok(mut mip) = self.cpu.read_csr(0x344) { // MIP
                if msip_pending {
                    mip |= 1 << 3; // MSIP
                }
                if timer_pending {
                    mip |= 1 << 7; // MTIP
                }
                let _ = self.cpu.write_csr(0x344, mip);
            }
        }
    }
    
    /// Get the total step count.
    pub fn step_count(&self) -> u64 {
        self.step_count
    }
    
    /// Get the hart ID.
    pub fn hart_id(&self) -> usize {
        self.hart_id
    }
}

/// Legacy worker entry point - DEPRECATED.
/// 
/// This function runs a blocking infinite loop. Use WorkerState + step_batch instead
/// for cooperative scheduling that doesn't block the worker's event loop.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn worker_entry(hart_id: usize, shared_mem: JsValue, entry_pc: u64) {
    web_sys::console::warn_1(&JsValue::from_str(
        "[Worker] Using deprecated blocking worker_entry. Consider using WorkerState."));
    
    let mut state = WorkerState::new(hart_id, shared_mem, entry_pc);
    
    loop {
        match state.step_batch(256) {
            WorkerStepResult::Continue => continue,
            _ => break,
        }
    }
    
    web_sys::console::log_1(&JsValue::from_str(
        &format!("[Worker {}] Exited after {} steps", hart_id, state.step_count)));
}

/// Check interrupts for this hart using the shared CLINT.
///
/// This is called periodically by the worker to check for:
/// - Software interrupts (IPI via MSIP)
/// - Timer interrupts (MTIP)
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn worker_check_interrupts(hart_id: usize, shared_mem: JsValue) -> u64 {
    let sab: SharedArrayBuffer = shared_mem.unchecked_into();
    let clint = SharedClint::new(&sab);
    
    let mut mip: u64 = 0;
    let (msip, timer) = clint.check_interrupts(hart_id);
    
    if msip {
        mip |= 1 << 3; // MSIP
    }
    if timer {
        mip |= 1 << 7; // MTIP
    }
    
    mip
}

#[cfg(test)]
mod tests {
    // Worker tests require WASM environment
}
