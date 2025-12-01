use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x10000;

pub const MSIP_OFFSET: u64 = 0x0000;
pub const MTIME_OFFSET: u64 = 0xbff8;
pub const MTIMECMP_OFFSET: u64 = 0x4000;
/// Hart count register offset (read-only, set by emulator at init)
pub const HART_COUNT_OFFSET: u64 = 0x0F00;

/// Maximum number of harts supported by the CLINT.
/// Set high enough to support modern multi-core systems.
pub const MAX_HARTS: usize = 128;

/// Time increment per tick (in timer ticks).
/// This is called every 256 CPU steps (when CPU poll_counter wraps), so we
/// increment by 256 to maintain the same effective timer rate.
/// At 10MHz and ~1 instruction per cycle at ~10MHz CPU, this gives roughly real-time.
const MTIME_INCREMENT: u64 = 256;

/// Core Local Interruptor (CLINT) - Timer and Software Interrupts
///
/// All operations are lock-free using atomic operations.
/// This is safe because:
/// - Each hart primarily accesses its own msip/mtimecmp slots
/// - mtime is shared but only incremented by hart 0
/// - The weak memory ordering matches RISC-V's memory model
pub struct Clint {
    /// Machine timer counter - incremented by tick() every 256 CPU steps.
    mtime: AtomicU64,

    /// Per-hart Machine Software Interrupt Pending bits.
    /// Only bit 0 is meaningful for each entry.
    msip: [AtomicU32; MAX_HARTS],

    /// Per-hart Machine Timer Compare registers.
    /// Timer interrupt fires when mtime >= mtimecmp[hart].
    mtimecmp: [AtomicU64; MAX_HARTS],

    /// Number of harts in the system (set at initialization).
    num_harts: AtomicUsize,
}

impl Clint {
    pub fn new() -> Self {
        // Default to 1 hart, can be set with set_num_harts()
        Self::with_harts(1)
    }
    
    /// Create a new CLINT with a specific hart count.
    pub fn with_harts(num_harts: usize) -> Self {
        // Create arrays of atomics initialized to their default values.
        // Note: We can't use [AtomicXX::new(val); MAX_HARTS] because atomics
        // don't implement Copy. We use consts for array initialization.
        const ZERO_U32: AtomicU32 = AtomicU32::new(0);
        const MAX_U64: AtomicU64 = AtomicU64::new(u64::MAX);

        Self {
            mtime: AtomicU64::new(0),
            msip: [ZERO_U32; MAX_HARTS],
            mtimecmp: [MAX_U64; MAX_HARTS],
            num_harts: AtomicUsize::new(num_harts.min(MAX_HARTS)),
        }
    }
    
    /// Set the number of harts (called by emulator at init).
    pub fn set_num_harts(&self, num_harts: usize) {
        self.num_harts.store(num_harts.min(MAX_HARTS), Ordering::Release);
    }

    /// Get the number of harts (lock-free using atomics).
    #[inline]
    pub fn num_harts(&self) -> usize {
        self.num_harts.load(Ordering::Relaxed)
    }

    /// Returns the current mtime value.
    /// Lock-free for performance.
    #[inline]
    pub fn mtime(&self) -> u64 {
        self.mtime.load(Ordering::Relaxed)
    }

    /// Sets mtime to a specific value (used for snapshot restore).
    pub fn set_mtime(&self, val: u64) {
        self.mtime.store(val, Ordering::Relaxed);
    }

    /// Advance mtime by one tick. Called once per CPU step.
    /// Lock-free using atomic fetch_add.
    #[inline]
    pub fn tick(&self) {
        self.mtime.fetch_add(MTIME_INCREMENT, Ordering::Relaxed);
    }

    /// Backward compatibility: increment is now tick()
    pub fn increment(&self) {
        self.tick();
    }

    pub fn sync_time_micros(&self, _micros: u64) {
        // No-op for deterministic timer
    }

    /// Get msip value for a hart (lock-free using atomics)
    pub fn get_msip(&self, hart: usize) -> u32 {
        if hart < MAX_HARTS {
            self.msip[hart].load(Ordering::Relaxed)
        } else {
            0
        }
    }

    /// Set MSIP value for a specific hart (only bit 0 is meaningful).
    /// Uses Release ordering to ensure any prior writes (e.g., data being
    /// passed to the target hart) are visible before the target sees the interrupt.
    pub fn set_msip(&self, hart: usize, value: u32) {
        if hart < MAX_HARTS {
            // Only bit 0 matters for MSIP
            self.msip[hart].store(value & 1, Ordering::Release);
        }
    }

    /// Get mtimecmp value for a hart (lock-free using atomics)
    pub fn get_mtimecmp(&self, hart: usize) -> u64 {
        if hart < MAX_HARTS {
            self.mtimecmp[hart].load(Ordering::Relaxed)
        } else {
            u64::MAX
        }
    }

    /// Set mtimecmp value for a specific hart (lock-free using atomics)
    pub fn set_mtimecmp(&self, hart: usize, value: u64) {
        if hart < MAX_HARTS {
            self.mtimecmp[hart].store(value, Ordering::Release);
        }
    }

    /// Get the low 32 bits of MTIMECMP for a hart
    pub fn get_mtimecmp_low(&self, hart: usize) -> u32 {
        if hart < MAX_HARTS {
            (self.mtimecmp[hart].load(Ordering::Relaxed) & 0xFFFF_FFFF) as u32
        } else {
            u32::MAX
        }
    }

    /// Get the high 32 bits of MTIMECMP for a hart
    pub fn get_mtimecmp_high(&self, hart: usize) -> u32 {
        if hart < MAX_HARTS {
            (self.mtimecmp[hart].load(Ordering::Relaxed) >> 32) as u32
        } else {
            u32::MAX
        }
    }

    /// Set the low 32 bits of MTIMECMP for a hart.
    /// Uses compare-and-swap to atomically update only the low bits.
    pub fn set_mtimecmp_low(&self, hart: usize, value: u32) {
        if hart >= MAX_HARTS {
            return;
        }

        // Atomic read-modify-write using compare_exchange loop
        loop {
            let current = self.mtimecmp[hart].load(Ordering::Relaxed);
            let new = (current & 0xFFFF_FFFF_0000_0000) | (value as u64);

            match self.mtimecmp[hart].compare_exchange_weak(
                current,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue, // Retry on contention
            }
        }
    }

    /// Set the high 32 bits of MTIMECMP for a hart.
    /// Uses compare-and-swap to atomically update only the high bits.
    pub fn set_mtimecmp_high(&self, hart: usize, value: u32) {
        if hart >= MAX_HARTS {
            return;
        }

        loop {
            let current = self.mtimecmp[hart].load(Ordering::Relaxed);
            let new = (current & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);

            match self.mtimecmp[hart].compare_exchange_weak(
                current,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
    }

    /// Check if timer interrupt is pending for a specific hart.
    /// Completely lock-free using atomic reads.
    #[inline]
    pub fn is_timer_pending(&self, hart_id: usize) -> bool {
        if hart_id >= MAX_HARTS {
            return false;
        }
        let mtime = self.mtime.load(Ordering::Relaxed);
        let mtimecmp = self.mtimecmp[hart_id].load(Ordering::Relaxed);
        mtime >= mtimecmp
    }

    /// Check if software interrupt is pending for a specific hart.
    /// Lock-free using atomics.
    #[inline]
    pub fn is_msip_pending(&self, hart_id: usize) -> bool {
        if hart_id >= MAX_HARTS {
            return false;
        }
        (self.msip[hart_id].load(Ordering::Relaxed) & 1) != 0
    }

    /// Check all interrupt conditions for a hart.
    /// Returns (msip_pending, timer_pending).
    /// Completely lock-free using atomic reads.
    #[inline]
    pub fn check_interrupts_for_hart(&self, hart_id: usize) -> (bool, bool) {
        if hart_id >= MAX_HARTS {
            return (false, false);
        }
        let mtime = self.mtime.load(Ordering::Relaxed);
        let msip = (self.msip[hart_id].load(Ordering::Relaxed) & 1) != 0;
        let mtimecmp = self.mtimecmp[hart_id].load(Ordering::Relaxed);
        let timer = mtime >= mtimecmp;
        (msip, timer)
    }

    /// Load from the CLINT register space.
    ///
    /// Offsets are relative to `CLINT_BASE`. Only naturally aligned 4- and
    /// 8-byte accesses are architecturally meaningful; other sizes return 0.
    ///
    /// This method is completely lock-free using atomic operations.
    pub fn load(&self, offset: u64, size: u64) -> u64 {
        match (offset, size) {
            // ============================================================
            // MTIME: 64-bit timer register
            // ============================================================
            (MTIME_OFFSET, 8) => self.mtime.load(Ordering::Relaxed),
            (MTIME_OFFSET, 4) => {
                // Low 32 bits
                self.mtime.load(Ordering::Relaxed) & 0xFFFF_FFFF
            }
            (o, 4) if o == MTIME_OFFSET + 4 => {
                // High 32 bits
                self.mtime.load(Ordering::Relaxed) >> 32
            }

            // ============================================================
            // MSIP: Per-hart software interrupt pending (32-bit each)
            // ============================================================
            (o, 4) if o >= MSIP_OFFSET && o < MSIP_OFFSET + (MAX_HARTS as u64 * 4) => {
                let hart_idx = ((o - MSIP_OFFSET) / 4) as usize;
                if hart_idx < MAX_HARTS {
                    self.msip[hart_idx].load(Ordering::Relaxed) as u64
                } else {
                    0
                }
            }

            // ============================================================
            // MTIMECMP: Per-hart timer compare register (64-bit each)
            // ============================================================
            (o, 8) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                if hart_idx < MAX_HARTS {
                    self.mtimecmp[hart_idx].load(Ordering::Relaxed)
                } else {
                    u64::MAX
                }
            }
            (o, 4) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                if hart_idx >= MAX_HARTS {
                    return 0;
                }
                let sub_offset = (o - MTIMECMP_OFFSET) % 8;
                let val = self.mtimecmp[hart_idx].load(Ordering::Relaxed);
                match sub_offset {
                    0 => val & 0xFFFF_FFFF,  // Low 32 bits
                    4 => val >> 32,          // High 32 bits
                    _ => 0,                  // Misaligned (shouldn't happen)
                }
            }

            // ============================================================
            // HART_COUNT: Number of harts (read-only, set at init)
            // ============================================================
            (HART_COUNT_OFFSET, 4) | (HART_COUNT_OFFSET, 8) => {
                self.num_harts.load(Ordering::Relaxed) as u64
            }

            // ============================================================
            // Reserved/unmapped: return zero
            // ============================================================
            _ => 0,
        }
    }

    // Snapshot support methods

    /// Get a copy of all MSIP values for snapshot.
    ///
    /// Note: This should only be called when the VM is paused for consistent state.
    pub fn get_msip_array(&self) -> [u32; MAX_HARTS] {
        let mut result = [0u32; MAX_HARTS];
        for i in 0..MAX_HARTS {
            result[i] = self.msip[i].load(Ordering::Relaxed);
        }
        result
    }

    /// Get a copy of all MTIMECMP values for snapshot.
    ///
    /// Note: This should only be called when the VM is paused for consistent state.
    pub fn get_mtimecmp_array(&self) -> [u64; MAX_HARTS] {
        let mut result = [u64::MAX; MAX_HARTS];
        for i in 0..MAX_HARTS {
            result[i] = self.mtimecmp[i].load(Ordering::Relaxed);
        }
        result
    }

    /// Restore MSIP values from snapshot.
    ///
    /// Note: This should only be called when the VM is paused for consistent state.
    pub fn set_msip_array(&self, values: &[u32]) {
        let len = values.len().min(MAX_HARTS);
        for i in 0..len {
            self.msip[i].store(values[i], Ordering::Relaxed);
        }
        // Clear any remaining slots
        for i in len..MAX_HARTS {
            self.msip[i].store(0, Ordering::Relaxed);
        }
    }

    /// Restore MTIMECMP values from snapshot.
    ///
    /// Note: This should only be called when the VM is paused for consistent state.
    pub fn set_mtimecmp_array(&self, values: &[u64]) {
        let len = values.len().min(MAX_HARTS);
        for i in 0..len {
            self.mtimecmp[i].store(values[i], Ordering::Relaxed);
        }
        // Set remaining slots to MAX (no timer interrupt)
        for i in len..MAX_HARTS {
            self.mtimecmp[i].store(u64::MAX, Ordering::Relaxed);
        }
    }

    /// Store into the CLINT register space.
    ///
    /// Offsets are relative to `CLINT_BASE`. Mis-sized or strange offsets are
    /// ignored to keep the device side-effect free for unsupported accesses.
    ///
    /// This method is completely lock-free using atomic operations.
    pub fn store(&self, offset: u64, size: u64, value: u64) {
        match (offset, size) {
            // ============================================================
            // MSIP: Per-hart software interrupt pending (32-bit write)
            // Only bit 0 is meaningful
            // ============================================================
            (o, 4) if o >= MSIP_OFFSET && o < MSIP_OFFSET + (MAX_HARTS as u64 * 4) => {
                let hart_idx = ((o - MSIP_OFFSET) / 4) as usize;
                if hart_idx < MAX_HARTS {
                    // Only bit 0 matters for MSIP (Machine Software Interrupt Pending)
                    self.msip[hart_idx].store((value & 1) as u32, Ordering::Release);
                }
            }

            // ============================================================
            // MTIME: Read-only in this implementation
            // (Timer is driven by tick() calls from the emulator)
            // ============================================================
            (MTIME_OFFSET, _) => {
                // Ignore writes to MTIME
            }
            (o, 4) if o == MTIME_OFFSET + 4 => {
                // Ignore writes to MTIME high bits
            }

            // ============================================================
            // MTIMECMP: Per-hart timer compare (64-bit or split 32-bit)
            // ============================================================
            (o, 8) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                // Full 64-bit write
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                if hart_idx < MAX_HARTS {
                    self.mtimecmp[hart_idx].store(value, Ordering::Release);
                }
            }
            (o, 4) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                // Split 32-bit write
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                if hart_idx >= MAX_HARTS {
                    return;
                }

                let sub_offset = (o - MTIMECMP_OFFSET) % 8;
                match sub_offset {
                    0 => {
                        // Write to low 32 bits - use atomic RMW
                        self.set_mtimecmp_low(hart_idx, value as u32);
                    }
                    4 => {
                        // Write to high 32 bits - use atomic RMW
                        self.set_mtimecmp_high(hart_idx, value as u32);
                    }
                    _ => {
                        // Misaligned access - ignore
                    }
                }
            }

            // ============================================================
            // HART_COUNT: Read-only (set at initialization)
            // ============================================================
            (HART_COUNT_OFFSET, _) => {
                // Ignore writes to HART_COUNT
            }

            // ============================================================
            // Reserved/unmapped: ignore
            // ============================================================
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    #[test]
    fn clint_is_thread_safe() {
        assert_send::<Clint>();
        assert_sync::<Clint>();
    }

    #[test]
    fn test_mtimecmp_split_access() {
        let clint = Clint::new();

        // Set via 64-bit write
        clint.set_mtimecmp(0, 0x1234_5678_9ABC_DEF0);

        // Read via 32-bit methods
        assert_eq!(clint.get_mtimecmp_low(0), 0x9ABC_DEF0);
        assert_eq!(clint.get_mtimecmp_high(0), 0x1234_5678);

        // Write via 32-bit methods
        clint.set_mtimecmp_low(0, 0x1111_1111);
        assert_eq!(clint.get_mtimecmp(0), 0x1234_5678_1111_1111);

        clint.set_mtimecmp_high(0, 0x2222_2222);
        assert_eq!(clint.get_mtimecmp(0), 0x2222_2222_1111_1111);
    }

    #[test]
    fn test_num_harts_atomic() {
        let clint = Clint::with_harts(4);
        assert_eq!(clint.num_harts(), 4);

        clint.set_num_harts(8);
        assert_eq!(clint.num_harts(), 8);

        // Verify clamping
        clint.set_num_harts(MAX_HARTS + 100);
        assert_eq!(clint.num_harts(), MAX_HARTS);
    }

    #[test]
    fn test_load_msip() {
        let clint = Clint::with_harts(4);

        // Initially all MSIP should be 0
        assert_eq!(clint.load(MSIP_OFFSET, 4), 0);
        assert_eq!(clint.load(MSIP_OFFSET + 4, 4), 0);

        // Set MSIP for hart 1
        clint.set_msip(1, 1);
        assert_eq!(clint.load(MSIP_OFFSET, 4), 0);     // Hart 0: still 0
        assert_eq!(clint.load(MSIP_OFFSET + 4, 4), 1); // Hart 1: now 1
    }

    #[test]
    fn test_load_mtimecmp() {
        let clint = Clint::with_harts(2);

        // Set MTIMECMP for hart 0
        clint.set_mtimecmp(0, 0x1234_5678_9ABC_DEF0);

        // 64-bit read
        assert_eq!(clint.load(MTIMECMP_OFFSET, 8), 0x1234_5678_9ABC_DEF0);

        // 32-bit split reads
        assert_eq!(clint.load(MTIMECMP_OFFSET, 4), 0x9ABC_DEF0);     // Low
        assert_eq!(clint.load(MTIMECMP_OFFSET + 4, 4), 0x1234_5678); // High
    }

    #[test]
    fn test_load_hart_count() {
        let clint = Clint::with_harts(16);
        assert_eq!(clint.load(HART_COUNT_OFFSET, 4), 16);
    }

    #[test]
    fn test_store_msip() {
        let clint = Clint::with_harts(4);

        // Write MSIP for hart 0 via MMIO
        clint.store(MSIP_OFFSET, 4, 1);
        assert_eq!(clint.get_msip(0), 1);

        // Only bit 0 matters
        clint.store(MSIP_OFFSET, 4, 0xFF);
        assert_eq!(clint.get_msip(0), 1); // Stored as 1

        // Clear
        clint.store(MSIP_OFFSET, 4, 0);
        assert_eq!(clint.get_msip(0), 0);
    }

    #[test]
    fn test_store_mtimecmp_64bit() {
        let clint = Clint::with_harts(2);

        // 64-bit write
        clint.store(MTIMECMP_OFFSET, 8, 0x1234_5678_9ABC_DEF0);
        assert_eq!(clint.get_mtimecmp(0), 0x1234_5678_9ABC_DEF0);
    }

    #[test]
    fn test_store_mtimecmp_32bit_split() {
        let clint = Clint::with_harts(2);

        // Start with a known value
        clint.set_mtimecmp(0, 0);

        // Write low 32 bits via MMIO
        clint.store(MTIMECMP_OFFSET, 4, 0xDEAD_BEEF);
        assert_eq!(clint.get_mtimecmp(0), 0x0000_0000_DEAD_BEEF);

        // Write high 32 bits via MMIO
        clint.store(MTIMECMP_OFFSET + 4, 4, 0xCAFE_BABE);
        assert_eq!(clint.get_mtimecmp(0), 0xCAFE_BABE_DEAD_BEEF);
    }

    #[test]
    fn test_store_mtime_readonly() {
        let clint = Clint::with_harts(1);

        let before = clint.mtime();
        clint.store(MTIME_OFFSET, 8, 0xFFFF_FFFF_FFFF_FFFF);
        let after = clint.mtime();

        // MTIME should not change (read-only via MMIO)
        assert_eq!(before, after);
    }

    #[test]
    fn test_check_interrupts_msip() {
        let clint = Clint::with_harts(4);

        // Initially no interrupts
        let (msip, timer) = clint.check_interrupts_for_hart(0);
        assert!(!msip);
        assert!(!timer); // mtimecmp defaults to MAX

        // Set MSIP for hart 0
        clint.set_msip(0, 1);
        let (msip, timer) = clint.check_interrupts_for_hart(0);
        assert!(msip);
        assert!(!timer);

        // Hart 1 should still have no MSIP
        let (msip, _) = clint.check_interrupts_for_hart(1);
        assert!(!msip);
    }

    #[test]
    fn test_check_interrupts_timer() {
        let clint = Clint::with_harts(2);

        // Set timer for hart 0 to trigger at time 1000
        clint.set_mtimecmp(0, 1000);

        // Before deadline
        clint.set_mtime(999);
        let (_, timer) = clint.check_interrupts_for_hart(0);
        assert!(!timer);

        // At deadline
        clint.set_mtime(1000);
        let (_, timer) = clint.check_interrupts_for_hart(0);
        assert!(timer);

        // After deadline
        clint.set_mtime(1001);
        let (_, timer) = clint.check_interrupts_for_hart(0);
        assert!(timer);
    }

    #[test]
    fn test_check_interrupts_out_of_bounds() {
        let clint = Clint::with_harts(4);

        // Out of bounds hart should return (false, false)
        let (msip, timer) = clint.check_interrupts_for_hart(MAX_HARTS);
        assert!(!msip);
        assert!(!timer);

        let (msip, timer) = clint.check_interrupts_for_hart(MAX_HARTS + 100);
        assert!(!msip);
        assert!(!timer);
    }

    #[test]
    fn test_check_interrupts_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let clint = Arc::new(Clint::with_harts(4));
        let mut handles = vec![];

        // Spawn 4 threads simulating 4 harts
        for hart_id in 0..4 {
            let clint_clone = Arc::clone(&clint);
            let handle = thread::spawn(move || {
                for _ in 0..1_000_000 {
                    let _ = clint_clone.check_interrupts_for_hart(hart_id);
                }
            });
            handles.push(handle);
        }

        // All threads should complete without deadlock or panic
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_clint_snapshot_roundtrip() {
        let clint1 = Clint::with_harts(4);

        // Set some state
        clint1.set_mtime(12345);
        clint1.set_msip(0, 1);
        clint1.set_msip(2, 1);
        clint1.set_mtimecmp(0, 1000);
        clint1.set_mtimecmp(1, 2000);
        clint1.set_mtimecmp(3, 3000);

        // Take snapshot
        let mtime = clint1.mtime();
        let msip_array = clint1.get_msip_array();
        let mtimecmp_array = clint1.get_mtimecmp_array();
        let num_harts = clint1.num_harts();

        // Create new CLINT and restore
        let clint2 = Clint::with_harts(num_harts);
        clint2.set_mtime(mtime);
        clint2.set_msip_array(&msip_array);
        clint2.set_mtimecmp_array(&mtimecmp_array);

        // Verify state matches
        assert_eq!(clint2.mtime(), 12345);
        assert_eq!(clint2.get_msip(0), 1);
        assert_eq!(clint2.get_msip(1), 0);
        assert_eq!(clint2.get_msip(2), 1);
        assert_eq!(clint2.get_mtimecmp(0), 1000);
        assert_eq!(clint2.get_mtimecmp(1), 2000);
        assert_eq!(clint2.get_mtimecmp(3), 3000);
    }

    #[test]
    fn test_is_timer_pending() {
        let clint = Clint::with_harts(2);

        // Set timer compare for hart 0
        clint.set_mtimecmp(0, 500);

        // Before the timer fires
        clint.set_mtime(100);
        assert!(!clint.is_timer_pending(0));

        // At the trigger point
        clint.set_mtime(500);
        assert!(clint.is_timer_pending(0));

        // After the trigger point
        clint.set_mtime(600);
        assert!(clint.is_timer_pending(0));

        // Hart 1 should still be pending (default MAX)
        assert!(!clint.is_timer_pending(1));
    }

    #[test]
    fn test_is_msip_pending() {
        let clint = Clint::with_harts(2);

        // Initially not pending
        assert!(!clint.is_msip_pending(0));
        assert!(!clint.is_msip_pending(1));

        // Set MSIP for hart 0
        clint.set_msip(0, 1);
        assert!(clint.is_msip_pending(0));
        assert!(!clint.is_msip_pending(1));

        // Clear MSIP for hart 0
        clint.set_msip(0, 0);
        assert!(!clint.is_msip_pending(0));

        // Out of bounds
        assert!(!clint.is_msip_pending(MAX_HARTS));
    }
}
