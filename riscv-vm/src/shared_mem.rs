//! Shared Memory Layout for WASM SMP
//!
//! This module defines the memory layout for SharedArrayBuffer-based
//! multi-hart execution in WASM environments.
//!
//! ## Memory Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ Control Region (4KB)         @ 0x0000                       │
//! │   - halt_requested (i32)     @ 0x0000                       │
//! │   - halted (i32)             @ 0x0004                       │
//! │   - halt_code (i64)          @ 0x0008                       │
//! │   - reserved                 @ 0x0010+                      │
//! ├─────────────────────────────────────────────────────────────┤
//! │ CLINT Region (64KB)          @ 0x1000                       │
//! │   - msip[MAX_HARTS]          @ 0x0000 (4B each)             │
//! │   - hart_count               @ 0x0F00 (4B)                  │
//! │   - mtimecmp[MAX_HARTS]      @ 0x4000 (8B each)             │
//! │   - mtime                    @ 0xBFF8 (8B)                  │
//! ├─────────────────────────────────────────────────────────────┤
//! │ DRAM Region                  @ 0x11000 (DRAM_BASE offset)   │
//! │   - Kernel, stack, heap, etc.                               │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! The CLINT layout mirrors the native CLINT for software compatibility.
//! Workers use JavaScript Atomics to access the shared state.

/// Size of the control region in bytes (4KB).
pub const CONTROL_REGION_SIZE: usize = 4096;

/// Size of the CLINT region in bytes (64KB, matches native CLINT_SIZE).
pub const CLINT_REGION_SIZE: usize = 0x10000;

/// Size of the shared UART output region in bytes (4KB).
pub const UART_OUTPUT_REGION_SIZE: usize = 4096;

/// Size of the shared UART input region in bytes (4KB).
pub const UART_INPUT_REGION_SIZE: usize = 4096;

/// Total header size before DRAM starts.
pub const HEADER_SIZE: usize =
    CONTROL_REGION_SIZE + CLINT_REGION_SIZE + UART_OUTPUT_REGION_SIZE + UART_INPUT_REGION_SIZE;

// ============================================================================
// Shared UART Output Region Offsets
// ============================================================================

/// Offset of the shared UART output region from start of SharedArrayBuffer.
pub const UART_OUTPUT_REGION_OFFSET: usize = CONTROL_REGION_SIZE + CLINT_REGION_SIZE;

/// UART output: write index (i32 index within UART region)
pub const UART_WRITE_IDX: u32 = 0;
/// UART output: read index (i32 index within UART region)
pub const UART_READ_IDX: u32 = 1;
/// UART output: buffer starts at byte 8 (after write_idx and read_idx)
pub const UART_BUFFER_OFFSET: usize = 8;
/// UART output: buffer capacity (region size minus header)
pub const UART_BUFFER_CAPACITY: usize = UART_OUTPUT_REGION_SIZE - UART_BUFFER_OFFSET;

// ============================================================================
// Shared UART Input Region Offsets
// ============================================================================

/// Offset of the shared UART input region from start of SharedArrayBuffer.
pub const UART_INPUT_REGION_OFFSET: usize =
    CONTROL_REGION_SIZE + CLINT_REGION_SIZE + UART_OUTPUT_REGION_SIZE;

/// UART input: write index (i32 index within UART input region)
pub const UART_INPUT_WRITE_IDX: u32 = 0;
/// UART input: read index (i32 index within UART input region)
pub const UART_INPUT_READ_IDX: u32 = 1;
/// UART input: buffer starts at byte 8 (after write_idx and read_idx)
pub const UART_INPUT_BUFFER_OFFSET: usize = 8;
/// UART input: buffer capacity (region size minus header)
pub const UART_INPUT_BUFFER_CAPACITY: usize = UART_INPUT_REGION_SIZE - UART_INPUT_BUFFER_OFFSET;

// ============================================================================
// Control Region Offsets (relative to start of SharedArrayBuffer)
// Using i32 indices for Atomics API compatibility
// ============================================================================

/// Control region: halt_requested flag (i32 index 0)
pub const CTRL_HALT_REQUESTED: u32 = 0;
/// Control region: halted flag (i32 index 1)
pub const CTRL_HALTED: u32 = 1;
/// Control region: halt_code low 32 bits (i32 index 2)
pub const CTRL_HALT_CODE_LO: u32 = 2;
/// Control region: halt_code high 32 bits (i32 index 3)
pub const CTRL_HALT_CODE_HI: u32 = 3;
/// Control region: number of active harts (i32 index 4)
pub const CTRL_NUM_HARTS: u32 = 4;
/// Control region: epoch counter for workers to detect new work (i32 index 5)
pub const CTRL_EPOCH: u32 = 5;
/// Control region: workers can start executing (i32 index 6)
/// Workers poll this flag; they park until main thread sets it.
pub const CTRL_WORKERS_CAN_START: u32 = 6;

// ============================================================================
// CLINT Region Offsets (relative to CLINT region start at CONTROL_REGION_SIZE)
// These match the native CLINT layout for software compatibility
// ============================================================================

/// CLINT: MSIP base offset (per-hart software interrupt pending)
pub const CLINT_MSIP_BASE: usize = 0x0000;
/// CLINT: MTIMECMP base offset (per-hart timer compare)
pub const CLINT_MTIMECMP_BASE: usize = 0x4000;
/// CLINT: MTIME offset (global timer)
pub const CLINT_MTIME_OFFSET: usize = 0xBFF8;
/// CLINT: HART_COUNT offset (read-only hart count)
pub const CLINT_HART_COUNT_OFFSET: usize = 0x0F00;

/// Maximum harts supported (matches native MAX_HARTS)
pub const MAX_HARTS: usize = 128;

/// Calculate the total SharedArrayBuffer size needed.
///
/// # Arguments
/// * `dram_size` - Size of DRAM region in bytes
pub const fn total_shared_size(dram_size: usize) -> usize {
    HEADER_SIZE + dram_size
}

/// Calculate the DRAM offset within the SharedArrayBuffer.
pub const fn dram_offset() -> usize {
    HEADER_SIZE
}

/// Calculate byte offset for MSIP of a specific hart.
pub const fn msip_offset(hart_id: usize) -> usize {
    CONTROL_REGION_SIZE + CLINT_MSIP_BASE + (hart_id * 4)
}

/// Calculate byte offset for MTIMECMP of a specific hart.
pub const fn mtimecmp_offset(hart_id: usize) -> usize {
    CONTROL_REGION_SIZE + CLINT_MTIMECMP_BASE + (hart_id * 8)
}

/// Calculate byte offset for MTIME.
pub const fn mtime_offset() -> usize {
    CONTROL_REGION_SIZE + CLINT_MTIME_OFFSET
}

/// Calculate byte offset for HART_COUNT.
pub const fn hart_count_offset() -> usize {
    CONTROL_REGION_SIZE + CLINT_HART_COUNT_OFFSET
}

// ============================================================================
// WASM-specific shared CLINT implementation
// ============================================================================

#[cfg(target_arch = "wasm32")]
pub mod wasm {
    use super::*;
    use js_sys::{Atomics, Int32Array, SharedArrayBuffer, Uint8Array};

    /// Shared CLINT accessor for WASM workers.
    ///
    /// This provides a CLINT-compatible interface backed by SharedArrayBuffer
    /// using JavaScript Atomics for thread-safe access.
    pub struct SharedClint {
        /// View of the entire SharedArrayBuffer as Int32Array for Atomics
        view: Int32Array,
        /// CLINT region byte offset
        clint_base: usize,
    }

    // SAFETY: SharedClint uses SharedArrayBuffer and JavaScript Atomics for
    // thread-safe access. In WASM, each worker has its own isolated memory space,
    // so the Int32Array view is not actually shared between Rust threads.
    // All cross-worker synchronization goes through SharedArrayBuffer + Atomics.
    unsafe impl Send for SharedClint {}
    unsafe impl Sync for SharedClint {}

    impl SharedClint {
        /// Create a new SharedClint from a SharedArrayBuffer.
        pub fn new(buffer: &SharedArrayBuffer) -> Self {
            Self {
                view: Int32Array::new(buffer),
                clint_base: CONTROL_REGION_SIZE,
            }
        }

        /// Get the i32 index for a byte offset.
        #[inline]
        fn i32_index(&self, byte_offset: usize) -> u32 {
            (byte_offset / 4) as u32
        }

        /// Load mtime (64-bit).
        pub fn mtime(&self) -> u64 {
            let offset = mtime_offset();
            let lo = self.load_i32(offset) as u32 as u64;
            let hi = self.load_i32(offset + 4) as u32 as u64;
            lo | (hi << 32)
        }

        /// Increment mtime (called by hart 0 only).
        pub fn tick(&self, increment: u64) {
            let offset = mtime_offset();
            // Read-modify-write using atomics
            // Note: In WASM, we use Atomics.add for 32-bit values
            // For 64-bit, we need two operations (potential race, but mtime only updated by hart 0)
            let current = self.mtime();
            let new_val = current.wrapping_add(increment);
            self.store_i32(offset, new_val as i32);
            self.store_i32(offset + 4, (new_val >> 32) as i32);
        }

        /// Get MSIP for a hart.
        pub fn get_msip(&self, hart_id: usize) -> u32 {
            if hart_id >= MAX_HARTS {
                return 0;
            }
            let offset = msip_offset(hart_id);
            (self.load_i32(offset) & 1) as u32
        }

        /// Set MSIP for a hart (IPI).
        pub fn set_msip(&self, hart_id: usize, value: u32) {
            if hart_id >= MAX_HARTS {
                return;
            }
            let offset = msip_offset(hart_id);
            self.store_i32(offset, (value & 1) as i32);
        }

        /// Get MTIMECMP for a hart.
        pub fn get_mtimecmp(&self, hart_id: usize) -> u64 {
            if hart_id >= MAX_HARTS {
                return u64::MAX;
            }
            let offset = mtimecmp_offset(hart_id);
            let lo = self.load_i32(offset) as u32 as u64;
            let hi = self.load_i32(offset + 4) as u32 as u64;
            lo | (hi << 32)
        }

        /// Set MTIMECMP for a hart.
        pub fn set_mtimecmp(&self, hart_id: usize, value: u64) {
            if hart_id >= MAX_HARTS {
                return;
            }
            let offset = mtimecmp_offset(hart_id);
            self.store_i32(offset, value as i32);
            self.store_i32(offset + 4, (value >> 32) as i32);
        }

        /// Check interrupts for a hart.
        /// Returns (msip_pending, timer_pending).
        pub fn check_interrupts(&self, hart_id: usize) -> (bool, bool) {
            if hart_id >= MAX_HARTS {
                return (false, false);
            }
            let msip = self.get_msip(hart_id) != 0;
            let mtime = self.mtime();
            let mtimecmp = self.get_mtimecmp(hart_id);
            let timer = mtime >= mtimecmp;
            (msip, timer)
        }

        /// Get number of harts.
        pub fn num_harts(&self) -> usize {
            let offset = hart_count_offset();
            self.load_i32(offset) as usize
        }

        /// Set number of harts (called during init).
        pub fn set_num_harts(&self, num_harts: usize) {
            let offset = hart_count_offset();
            self.store_i32(offset, num_harts.min(MAX_HARTS) as i32);
        }

        /// Load a CLINT register (MMIO-style).
        pub fn load(&self, offset: u64, size: u64) -> u64 {
            let byte_offset = self.clint_base + offset as usize;
            match size {
                4 => self.load_i32(byte_offset) as u32 as u64,
                8 => {
                    let lo = self.load_i32(byte_offset) as u32 as u64;
                    let hi = self.load_i32(byte_offset + 4) as u32 as u64;
                    lo | (hi << 32)
                }
                _ => 0,
            }
        }

        /// Store to a CLINT register (MMIO-style).
        pub fn store(&self, offset: u64, size: u64, value: u64) {
            let byte_offset = self.clint_base + offset as usize;
            match size {
                4 => self.store_i32(byte_offset, value as i32),
                8 => {
                    self.store_i32(byte_offset, value as i32);
                    self.store_i32(byte_offset + 4, (value >> 32) as i32);
                }
                _ => {}
            }
        }

        // Low-level atomic operations

        #[inline]
        fn load_i32(&self, byte_offset: usize) -> i32 {
            let idx = self.i32_index(byte_offset);
            Atomics::load(&self.view, idx).unwrap_or(0)
        }

        #[inline]
        fn store_i32(&self, byte_offset: usize, value: i32) {
            let idx = self.i32_index(byte_offset);
            let _ = Atomics::store(&self.view, idx, value);
        }
    }

    /// Shared control region accessor.
    pub struct SharedControl {
        view: Int32Array,
    }

    impl SharedControl {
        /// Create from SharedArrayBuffer.
        pub fn new(buffer: &SharedArrayBuffer) -> Self {
            Self {
                view: Int32Array::new(buffer),
            }
        }

        /// Check if halt has been requested.
        pub fn is_halt_requested(&self) -> bool {
            Atomics::load(&self.view, CTRL_HALT_REQUESTED).unwrap_or(0) != 0
        }

        /// Request halt (called by any hart).
        pub fn request_halt(&self) {
            let _ = Atomics::store(&self.view, CTRL_HALT_REQUESTED, 1);
            // Wake any workers waiting on this flag
            let _ = Atomics::notify(&self.view, CTRL_HALT_REQUESTED);
        }

        /// Check if VM has halted.
        pub fn is_halted(&self) -> bool {
            Atomics::load(&self.view, CTRL_HALTED).unwrap_or(0) != 0
        }

        /// Signal that VM has halted with a code.
        pub fn signal_halted(&self, code: u64) {
            let _ = Atomics::store(&self.view, CTRL_HALT_CODE_LO, (code & 0xFFFFFFFF) as i32);
            let _ = Atomics::store(&self.view, CTRL_HALT_CODE_HI, (code >> 32) as i32);
            let _ = Atomics::store(&self.view, CTRL_HALTED, 1);
            // Wake all waiting threads
            let _ = Atomics::notify(&self.view, CTRL_HALTED);
        }

        /// Get the halt code.
        pub fn halt_code(&self) -> u64 {
            let lo = Atomics::load(&self.view, CTRL_HALT_CODE_LO).unwrap_or(0) as u32 as u64;
            let hi = Atomics::load(&self.view, CTRL_HALT_CODE_HI).unwrap_or(0) as u32 as u64;
            lo | (hi << 32)
        }

        /// Check if we should stop (halt requested or halted).
        #[inline]
        pub fn should_stop(&self) -> bool {
            self.is_halt_requested() || self.is_halted()
        }

        /// Get the number of active harts.
        pub fn num_harts(&self) -> usize {
            Atomics::load(&self.view, CTRL_NUM_HARTS).unwrap_or(1) as usize
        }

        /// Set the number of active harts.
        pub fn set_num_harts(&self, n: usize) {
            let _ = Atomics::store(&self.view, CTRL_NUM_HARTS, n as i32);
        }

        /// Increment epoch (signal new work available).
        pub fn increment_epoch(&self) {
            let _ = Atomics::add(&self.view, CTRL_EPOCH, 1);
            let _ = Atomics::notify(&self.view, CTRL_EPOCH);
        }

        /// Wait for epoch change (used by workers to wait for work).
        pub fn wait_for_epoch(&self, expected: i32) {
            // Atomics.wait returns immediately if value != expected
            let _ = Atomics::wait(&self.view, CTRL_EPOCH, expected);
        }

        /// Get current epoch.
        pub fn epoch(&self) -> i32 {
            Atomics::load(&self.view, CTRL_EPOCH).unwrap_or(0)
        }

        /// Check if workers can start executing.
        /// Workers poll this flag; they remain parked until it's set.
        pub fn can_workers_start(&self) -> bool {
            Atomics::load(&self.view, CTRL_WORKERS_CAN_START).unwrap_or(0) != 0
        }

        /// Signal that workers can start executing.
        /// Called by hart 0 after initial boot is complete.
        pub fn allow_workers_to_start(&self) {
            let _ = Atomics::store(&self.view, CTRL_WORKERS_CAN_START, 1);
            // Wake any workers waiting on this flag
            let _ = Atomics::notify(&self.view, CTRL_WORKERS_CAN_START);
        }

        /// Wait for workers_can_start signal.
        /// Workers call this to park until main thread allows execution.
        pub fn wait_for_start_signal(&self) {
            // Poll-wait with yield - Atomics.wait may not work in workers
            // depending on browser, so we use a simple poll loop
            while !self.can_workers_start() {
                // Yield to prevent busy-spin from starving other threads
                // This is a hint to the JS runtime
                std::hint::spin_loop();
            }
        }
    }

    /// Shared UART output ring buffer for workers to send output to hart 0.
    ///
    /// This implements a lock-free single-producer-single-consumer ring buffer
    /// using atomics. Workers write to it, and hart 0 reads from it.
    pub struct SharedUartOutput {
        /// View of the entire SharedArrayBuffer as Int32Array for Atomics
        view: Int32Array,
        /// View of the UART buffer region as Uint8Array
        byte_view: Uint8Array,
    }

    // SAFETY: SharedUartOutput uses SharedArrayBuffer and JavaScript Atomics
    unsafe impl Send for SharedUartOutput {}
    unsafe impl Sync for SharedUartOutput {}

    impl SharedUartOutput {
        /// Create a new SharedUartOutput from a SharedArrayBuffer.
        pub fn new(buffer: &SharedArrayBuffer) -> Self {
            Self {
                view: Int32Array::new(buffer),
                byte_view: Uint8Array::new(buffer),
            }
        }

        /// Get the i32 index for an offset within the UART output region.
        #[inline]
        fn uart_i32_index(&self, offset: u32) -> u32 {
            ((UART_OUTPUT_REGION_OFFSET / 4) as u32) + offset
        }

        /// Write a byte to the shared UART output buffer.
        /// Returns true if the byte was written, false if buffer is full.
        pub fn write_byte(&self, byte: u8) -> bool {
            let write_idx_slot = self.uart_i32_index(UART_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            let capacity = UART_BUFFER_CAPACITY as u32;
            let next_write = (write_idx + 1) % capacity;

            // Check if buffer is full
            if next_write == read_idx {
                return false;
            }

            // Write the byte
            let byte_offset = UART_OUTPUT_REGION_OFFSET + UART_BUFFER_OFFSET + (write_idx as usize);
            self.byte_view.set_index(byte_offset as u32, byte);

            // Update write index atomically
            let _ = Atomics::store(&self.view, write_idx_slot, next_write as i32);

            true
        }

        /// Read a byte from the shared UART output buffer.
        /// Returns None if buffer is empty.
        pub fn read_byte(&self) -> Option<u8> {
            let write_idx_slot = self.uart_i32_index(UART_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            // Check if buffer is empty
            if write_idx == read_idx {
                return None;
            }

            // Read the byte
            let byte_offset = UART_OUTPUT_REGION_OFFSET + UART_BUFFER_OFFSET + (read_idx as usize);
            let byte = self.byte_view.get_index(byte_offset as u32);

            // Update read index atomically
            let capacity = UART_BUFFER_CAPACITY as u32;
            let next_read = (read_idx + 1) % capacity;
            let _ = Atomics::store(&self.view, read_idx_slot, next_read as i32);

            Some(byte)
        }

        /// Check if there are bytes available to read.
        pub fn has_data(&self) -> bool {
            let write_idx_slot = self.uart_i32_index(UART_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0);
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0);

            write_idx != read_idx
        }

        /// Write multiple bytes to the shared UART output buffer.
        /// This is more efficient than calling write_byte repeatedly as it
        /// reduces atomic operations (only one index read/write per batch).
        /// Returns the number of bytes successfully written.
        pub fn write_bytes(&self, bytes: &[u8]) -> usize {
            if bytes.is_empty() {
                return 0;
            }

            let write_idx_slot = self.uart_i32_index(UART_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_READ_IDX);

            // Single atomic read of indices at start
            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            let capacity = UART_BUFFER_CAPACITY as u32;

            // Calculate available space (ring buffer)
            let available = if write_idx >= read_idx {
                capacity - (write_idx - read_idx) - 1
            } else {
                read_idx - write_idx - 1
            };

            // Write as many bytes as we can
            let to_write = (bytes.len() as u32).min(available) as usize;
            if to_write == 0 {
                return 0;
            }

            let mut current_write = write_idx;
            for &byte in &bytes[..to_write] {
                let byte_offset =
                    UART_OUTPUT_REGION_OFFSET + UART_BUFFER_OFFSET + (current_write as usize);
                self.byte_view.set_index(byte_offset as u32, byte);
                current_write = (current_write + 1) % capacity;
            }

            // Single atomic write to update the write index
            let _ = Atomics::store(&self.view, write_idx_slot, current_write as i32);

            to_write
        }

        /// Read multiple bytes from the shared UART output buffer.
        /// This is more efficient than calling read_byte repeatedly.
        /// Returns a vector of bytes read.
        pub fn read_bytes(&self, max_count: usize) -> Vec<u8> {
            let write_idx_slot = self.uart_i32_index(UART_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_READ_IDX);

            // Single atomic read of indices at start
            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            // Check if buffer is empty
            if write_idx == read_idx {
                return Vec::new();
            }

            let capacity = UART_BUFFER_CAPACITY as u32;

            // Calculate available bytes
            let available = if write_idx >= read_idx {
                write_idx - read_idx
            } else {
                capacity - read_idx + write_idx
            } as usize;

            let to_read = available.min(max_count);
            let mut bytes = Vec::with_capacity(to_read);

            let mut current_read = read_idx;
            for _ in 0..to_read {
                let byte_offset =
                    UART_OUTPUT_REGION_OFFSET + UART_BUFFER_OFFSET + (current_read as usize);
                bytes.push(self.byte_view.get_index(byte_offset as u32));
                current_read = (current_read + 1) % capacity;
            }

            // Single atomic write to update the read index
            let _ = Atomics::store(&self.view, read_idx_slot, current_read as i32);

            bytes
        }
    }

    /// Shared UART input ring buffer for main thread to send input to workers.
    ///
    /// This implements a lock-free single-producer-single-consumer ring buffer
    /// using atomics. Main thread (hart 0) writes to it, workers read from it.
    ///
    /// This allows workers to receive keyboard input that the browser sends
    /// to the main thread.
    pub struct SharedUartInput {
        /// View of the entire SharedArrayBuffer as Int32Array for Atomics
        view: Int32Array,
        /// View of the UART input buffer region as Uint8Array
        byte_view: Uint8Array,
    }

    // SAFETY: SharedUartInput uses SharedArrayBuffer and JavaScript Atomics
    unsafe impl Send for SharedUartInput {}
    unsafe impl Sync for SharedUartInput {}

    impl SharedUartInput {
        /// Create a new SharedUartInput from a SharedArrayBuffer.
        pub fn new(buffer: &SharedArrayBuffer) -> Self {
            Self {
                view: Int32Array::new(buffer),
                byte_view: Uint8Array::new(buffer),
            }
        }

        /// Get the i32 index for an offset within the UART input region.
        #[inline]
        fn uart_i32_index(&self, offset: u32) -> u32 {
            ((UART_INPUT_REGION_OFFSET / 4) as u32) + offset
        }

        /// Write a byte to the shared UART input buffer (called by main thread).
        /// Returns true if the byte was written, false if buffer is full.
        pub fn write_byte(&self, byte: u8) -> bool {
            let write_idx_slot = self.uart_i32_index(UART_INPUT_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_INPUT_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            let capacity = UART_INPUT_BUFFER_CAPACITY as u32;
            let next_write = (write_idx + 1) % capacity;

            // Check if buffer is full
            if next_write == read_idx {
                return false;
            }

            // Write the byte
            let byte_offset =
                UART_INPUT_REGION_OFFSET + UART_INPUT_BUFFER_OFFSET + (write_idx as usize);
            self.byte_view.set_index(byte_offset as u32, byte);

            // Update write index atomically
            let _ = Atomics::store(&self.view, write_idx_slot, next_write as i32);

            true
        }

        /// Read a byte from the shared UART input buffer (called by workers).
        /// Returns None if buffer is empty.
        pub fn read_byte(&self) -> Option<u8> {
            let write_idx_slot = self.uart_i32_index(UART_INPUT_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_INPUT_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            // Check if buffer is empty
            if write_idx == read_idx {
                return None;
            }

            // Read the byte
            let byte_offset =
                UART_INPUT_REGION_OFFSET + UART_INPUT_BUFFER_OFFSET + (read_idx as usize);
            let byte = self.byte_view.get_index(byte_offset as u32);

            // Update read index atomically
            let capacity = UART_INPUT_BUFFER_CAPACITY as u32;
            let next_read = (read_idx + 1) % capacity;
            let _ = Atomics::store(&self.view, read_idx_slot, next_read as i32);

            Some(byte)
        }

        /// Check if there are bytes available to read.
        pub fn has_data(&self) -> bool {
            let write_idx_slot = self.uart_i32_index(UART_INPUT_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_INPUT_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0);
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0);

            write_idx != read_idx
        }

        /// Write multiple bytes to the shared UART input buffer.
        /// This is more efficient than calling write_byte repeatedly.
        /// Returns the number of bytes successfully written.
        pub fn write_bytes(&self, bytes: &[u8]) -> usize {
            if bytes.is_empty() {
                return 0;
            }

            let write_idx_slot = self.uart_i32_index(UART_INPUT_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_INPUT_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            let capacity = UART_INPUT_BUFFER_CAPACITY as u32;

            let available = if write_idx >= read_idx {
                capacity - (write_idx - read_idx) - 1
            } else {
                read_idx - write_idx - 1
            };

            let to_write = (bytes.len() as u32).min(available) as usize;
            if to_write == 0 {
                return 0;
            }

            let mut current_write = write_idx;
            for &byte in &bytes[..to_write] {
                let byte_offset =
                    UART_INPUT_REGION_OFFSET + UART_INPUT_BUFFER_OFFSET + (current_write as usize);
                self.byte_view.set_index(byte_offset as u32, byte);
                current_write = (current_write + 1) % capacity;
            }

            let _ = Atomics::store(&self.view, write_idx_slot, current_write as i32);

            to_write
        }

        /// Read multiple bytes from the shared UART input buffer.
        /// This is more efficient than calling read_byte repeatedly.
        pub fn read_bytes(&self, max_count: usize) -> Vec<u8> {
            let write_idx_slot = self.uart_i32_index(UART_INPUT_WRITE_IDX);
            let read_idx_slot = self.uart_i32_index(UART_INPUT_READ_IDX);

            let write_idx = Atomics::load(&self.view, write_idx_slot).unwrap_or(0) as u32;
            let read_idx = Atomics::load(&self.view, read_idx_slot).unwrap_or(0) as u32;

            if write_idx == read_idx {
                return Vec::new();
            }

            let capacity = UART_INPUT_BUFFER_CAPACITY as u32;

            let available = if write_idx >= read_idx {
                write_idx - read_idx
            } else {
                capacity - read_idx + write_idx
            } as usize;

            let to_read = available.min(max_count);
            let mut bytes = Vec::with_capacity(to_read);

            let mut current_read = read_idx;
            for _ in 0..to_read {
                let byte_offset =
                    UART_INPUT_REGION_OFFSET + UART_INPUT_BUFFER_OFFSET + (current_read as usize);
                bytes.push(self.byte_view.get_index(byte_offset as u32));
                current_read = (current_read + 1) % capacity;
            }

            let _ = Atomics::store(&self.view, read_idx_slot, current_read as i32);

            bytes
        }
    }

    /// Initialize the shared memory region.
    ///
    /// Sets up the control region, CLINT, and shared UART output with default values.
    pub fn init_shared_memory(buffer: &SharedArrayBuffer, num_harts: usize) {
        let view = Int32Array::new(buffer);

        // Initialize control region
        let _ = Atomics::store(&view, CTRL_HALT_REQUESTED, 0);
        let _ = Atomics::store(&view, CTRL_HALTED, 0);
        let _ = Atomics::store(&view, CTRL_HALT_CODE_LO, 0);
        let _ = Atomics::store(&view, CTRL_HALT_CODE_HI, 0);
        let _ = Atomics::store(&view, CTRL_NUM_HARTS, num_harts as i32);
        let _ = Atomics::store(&view, CTRL_EPOCH, 0);
        // Workers start parked - main thread will set this after boot
        let _ = Atomics::store(&view, CTRL_WORKERS_CAN_START, 0);

        // Initialize CLINT region
        let clint = SharedClint::new(buffer);

        // Initialize MTIME to 0
        let mtime_off = mtime_offset();
        let _ = Atomics::store(&view, (mtime_off / 4) as u32, 0);
        let _ = Atomics::store(&view, ((mtime_off + 4) / 4) as u32, 0);

        // Initialize MSIP to 0 for all harts
        for hart in 0..MAX_HARTS {
            clint.set_msip(hart, 0);
        }

        // Initialize MTIMECMP to MAX for all harts (no timer interrupt)
        for hart in 0..MAX_HARTS {
            clint.set_mtimecmp(hart, u64::MAX);
        }

        // Set hart count
        clint.set_num_harts(num_harts);

        // Initialize shared UART output region
        // write_idx and read_idx both start at 0 (empty buffer)
        let uart_out_base_i32 = (UART_OUTPUT_REGION_OFFSET / 4) as u32;
        let _ = Atomics::store(&view, uart_out_base_i32 + UART_WRITE_IDX, 0);
        let _ = Atomics::store(&view, uart_out_base_i32 + UART_READ_IDX, 0);

        // Initialize shared UART input region
        // write_idx and read_idx both start at 0 (empty buffer)
        let uart_in_base_i32 = (UART_INPUT_REGION_OFFSET / 4) as u32;
        let _ = Atomics::store(&view, uart_in_base_i32 + UART_INPUT_WRITE_IDX, 0);
        let _ = Atomics::store(&view, uart_in_base_i32 + UART_INPUT_READ_IDX, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offsets() {
        // Control region is 4KB
        assert_eq!(CONTROL_REGION_SIZE, 4096);

        // CLINT region is 64KB
        assert_eq!(CLINT_REGION_SIZE, 0x10000);

        // UART output region is 4KB
        assert_eq!(UART_OUTPUT_REGION_SIZE, 4096);

        // UART input region is 4KB
        assert_eq!(UART_INPUT_REGION_SIZE, 4096);

        // Header is control + CLINT + UART output + UART input
        assert_eq!(HEADER_SIZE, 4096 + 0x10000 + 4096 + 4096);

        // DRAM starts after header
        assert_eq!(dram_offset(), HEADER_SIZE);
    }

    #[test]
    fn test_clint_offsets() {
        // MSIP for hart 0 is at CLINT base
        assert_eq!(msip_offset(0), CONTROL_REGION_SIZE + 0);

        // MSIP for hart 1 is 4 bytes later
        assert_eq!(msip_offset(1), CONTROL_REGION_SIZE + 4);

        // MTIMECMP for hart 0
        assert_eq!(mtimecmp_offset(0), CONTROL_REGION_SIZE + 0x4000);

        // MTIME
        assert_eq!(mtime_offset(), CONTROL_REGION_SIZE + 0xBFF8);
    }

    #[test]
    fn test_total_size() {
        let dram_size = 512 * 1024 * 1024; // 512 MiB
        let total = total_shared_size(dram_size);
        assert_eq!(total, HEADER_SIZE + dram_size);
    }
}
