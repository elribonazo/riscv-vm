use crate::clint::MAX_HARTS;
use crate::dram::MemoryError;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

pub const PLIC_BASE: u64 = 0x0C00_0000;
pub const PLIC_SIZE: u64 = 0x400_0000;

pub const UART_IRQ: u32 = 10;
pub const VIRTIO0_IRQ: u32 = 1;

const NUM_SOURCES: usize = 32;
/// Number of interrupt contexts.
/// Each hart has 2 contexts: M-mode (2*N) and S-mode (2*N+1).
const NUM_CONTEXTS: usize = 2 * MAX_HARTS;  // 2 contexts per hart (M-mode and S-mode)

/// Internal mutable state for PLIC, protected by Mutex
struct PlicState {
    priority: [u32; NUM_SOURCES],
    pending: u32, // Level-triggered mirror of device IRQ lines (bit per source)
    enable: [u32; NUM_CONTEXTS],
    threshold: [u32; NUM_CONTEXTS],
    active: [u32; NUM_CONTEXTS], // Per-context in-flight IRQs (claimed but not completed)
    debug: bool,
}

pub struct Plic {
    /// Authoritative state protected by Mutex
    state: Mutex<PlicState>,
    
    // ============================================================
    // Atomic Caches - Updated on writes, used for fast polling
    // ============================================================
    
    /// Cache of pending interrupt bits (mirrors state.pending)
    pending_cache: AtomicU32,
    
    /// Cache of enable bits per context (mirrors state.enable)
    enable_cache: [AtomicU32; NUM_CONTEXTS],
    
    /// Cache of threshold per context (mirrors state.threshold)
    threshold_cache: [AtomicU32; NUM_CONTEXTS],
    
    /// Cache of priority per source (mirrors state.priority)
    /// Note: Only need cache for sources 0-31
    priority_cache: [AtomicU32; NUM_SOURCES],
}

impl Plic {
    /// Get the M-mode context ID for a given hart.
    #[inline]
    pub fn m_context(hart_id: usize) -> usize {
        hart_id * 2
    }
    
    /// Get the S-mode context ID for a given hart.
    #[inline]
    pub fn s_context(hart_id: usize) -> usize {
        hart_id * 2 + 1
    }

    pub fn new() -> Self {
        // Create atomic arrays using const initialization
        const ZERO: AtomicU32 = AtomicU32::new(0);
        
        Self {
            state: Mutex::new(PlicState {
                priority: [0; NUM_SOURCES],
                pending: 0,
                enable: [0; NUM_CONTEXTS],
                threshold: [0; NUM_CONTEXTS],
                active: [0; NUM_CONTEXTS],
                debug: false,
            }),
            
            // Initialize caches to match initial state
            pending_cache: AtomicU32::new(0),
            enable_cache: [ZERO; NUM_CONTEXTS],
            threshold_cache: [ZERO; NUM_CONTEXTS],
            priority_cache: [ZERO; NUM_SOURCES],
        }
    }

    // ============================================================
    // Cache Sync Methods
    // ============================================================

    /// Sync all caches from authoritative state.
    /// 
    /// This should be called while holding the PlicState lock.
    /// Takes a reference to the state to avoid double-locking.
    fn sync_caches_from(&self, state: &PlicState) {
        // Sync pending
        self.pending_cache.store(state.pending, Ordering::Release);
        
        // Sync enable for all contexts
        for (i, &val) in state.enable.iter().enumerate() {
            self.enable_cache[i].store(val, Ordering::Release);
        }
        
        // Sync threshold for all contexts
        for (i, &val) in state.threshold.iter().enumerate() {
            self.threshold_cache[i].store(val, Ordering::Release);
        }
        
        // Sync priority for all sources
        for (i, &val) in state.priority.iter().enumerate() {
            self.priority_cache[i].store(val, Ordering::Release);
        }
    }

    /// Sync only the pending cache.
    #[inline]
    fn sync_pending_cache(&self, state: &PlicState) {
        self.pending_cache.store(state.pending, Ordering::Release);
    }
    
    /// Sync only the enable cache for a specific context.
    #[inline]
    fn sync_enable_cache(&self, state: &PlicState, ctx: usize) {
        if ctx < NUM_CONTEXTS {
            self.enable_cache[ctx].store(state.enable[ctx], Ordering::Release);
        }
    }
    
    /// Sync only the threshold cache for a specific context.
    #[inline]
    fn sync_threshold_cache(&self, state: &PlicState, ctx: usize) {
        if ctx < NUM_CONTEXTS {
            self.threshold_cache[ctx].store(state.threshold[ctx], Ordering::Release);
        }
    }
    
    /// Sync only the priority cache for a specific source.
    #[inline]
    fn sync_priority_cache(&self, state: &PlicState, source: usize) {
        if source < NUM_SOURCES {
            self.priority_cache[source].store(state.priority[source], Ordering::Release);
        }
    }

    /// Force sync all caches from Mutex state.
    /// 
    /// This acquires the lock and syncs all caches.
    /// Mainly useful for testing and initialization.
    pub fn sync_caches(&self) {
        let state = self.state.lock().unwrap();
        self.sync_caches_from(&state);
    }

    // ============================================================
    // Cache Accessor Methods (lock-free)
    // ============================================================

    /// Get pending bits from cache (lock-free)
    #[inline]
    pub fn pending_cached(&self) -> u32 {
        self.pending_cache.load(Ordering::Relaxed)
    }
    
    /// Get enable bits for a context from cache (lock-free)
    #[inline]
    pub fn enable_cached(&self, ctx: usize) -> u32 {
        if ctx < NUM_CONTEXTS {
            self.enable_cache[ctx].load(Ordering::Relaxed)
        } else {
            0
        }
    }
    
    /// Get threshold for a context from cache (lock-free)
    #[inline]
    pub fn threshold_cached(&self, ctx: usize) -> u32 {
        if ctx < NUM_CONTEXTS {
            self.threshold_cache[ctx].load(Ordering::Relaxed)
        } else {
            0
        }
    }
    
    /// Get priority for a source from cache (lock-free)
    #[inline]
    pub fn priority_cached(&self, source: usize) -> u32 {
        if source < NUM_SOURCES {
            self.priority_cache[source].load(Ordering::Relaxed)
        } else {
            0
        }
    }

    // ============================================================
    // Fast Interrupt Check (lock-free)
    // ============================================================

    /// Fast lock-free check for pending interrupts.
    /// 
    /// This uses atomic caches and is suitable for polling.
    /// Returns true if ANY interrupt might be pending; the caller should
    /// use claim_interrupt_for() to get the actual interrupt ID.
    /// 
    /// Note: May return false positives (cache slightly stale) but not false negatives
    /// when used correctly (caches synced on writes).
    #[inline]
    pub fn is_interrupt_pending_for_fast(&self, ctx: usize) -> bool {
        if ctx >= NUM_CONTEXTS {
            return false;
        }
        
        // Load cached values (lock-free)
        let pending = self.pending_cache.load(Ordering::Relaxed);
        let enable = self.enable_cache[ctx].load(Ordering::Relaxed);
        let threshold = self.threshold_cache[ctx].load(Ordering::Relaxed);
        
        // Quick check: any enabled source pending?
        let candidates = pending & enable;
        if candidates == 0 {
            return false;
        }
        
        // Check if any candidate has priority > threshold
        // This is a bit more expensive but still lock-free
        for source in 1..NUM_SOURCES {
            if (candidates & (1 << source)) != 0 {
                let priority = self.priority_cache[source].load(Ordering::Relaxed);
                if priority > threshold {
                    return true;
                }
            }
        }
        
        false
    }

    /// Ultra-fast pending check - only checks pending & enable.
    /// 
    /// This may return true when priority <= threshold, but is faster.
    /// Use when you want to minimize polling overhead and can tolerate
    /// occasional unnecessary claim attempts.
    #[inline]
    pub fn has_pending_candidate(&self, ctx: usize) -> bool {
        if ctx >= NUM_CONTEXTS {
            return false;
        }
        let pending = self.pending_cache.load(Ordering::Relaxed);
        let enable = self.enable_cache[ctx].load(Ordering::Relaxed);
        (pending & enable) != 0
    }

    // ============================================================
    // Source Level Control
    // ============================================================

    pub fn update_pending(&self, source: u32) {
        let mut state = self.state.lock().unwrap();
        // Backward compatibility helper: set as pending (edge → level).
        // Bus.refresh_irqs() may later clear this if device line is low.
        if source < 32 {
            if state.debug {
                 eprintln!("[PLIC] Update Pending source={}", source);
            }
            state.pending |= 1 << source;
            // Sync pending cache
            self.sync_pending_cache(&state);
        }
    }

    // New: level-triggered source line setter
    pub fn set_source_level(&self, source: u32, level: bool) {
        let mut state = self.state.lock().unwrap();
        if source >= 32 {
            return;
        }
        let was_pending = (state.pending & (1 << source)) != 0;
        if level {
            if state.debug && !was_pending {
                 eprintln!("[PLIC] IRQ Line High: source={} enable[0]=0x{:x} enable[1]=0x{:x} prio={}", 
                          source, state.enable[0], state.enable[1], state.priority[source as usize]);
            }
            state.pending |= 1 << source;
        } else {
            state.pending &= !(1 << source);
        }
        // Sync pending cache
        self.sync_pending_cache(&state);
    }

    // ============================================================
    // Snapshot support methods
    // ============================================================

    /// Get priority array for snapshot
    pub fn get_priority(&self) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state.priority.to_vec()
    }

    /// Get pending bits for snapshot
    pub fn get_pending(&self) -> u32 {
        let state = self.state.lock().unwrap();
        state.pending
    }

    /// Get enable array for snapshot
    pub fn get_enable(&self) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state.enable.to_vec()
    }

    /// Get threshold array for snapshot
    pub fn get_threshold(&self) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state.threshold.to_vec()
    }

    /// Get active array for snapshot
    pub fn get_active(&self) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state.active.to_vec()
    }

    /// Restore priority from snapshot
    pub fn set_priority(&self, values: &[u32]) {
        let mut state = self.state.lock().unwrap();
        for (i, &val) in values.iter().enumerate() {
            if i < state.priority.len() {
                state.priority[i] = val;
            }
        }
        // Sync all priority caches
        for i in 0..NUM_SOURCES.min(values.len()) {
            self.priority_cache[i].store(state.priority[i], Ordering::Release);
        }
    }

    /// Restore pending bits from snapshot  
    pub fn set_pending(&self, value: u32) {
        let mut state = self.state.lock().unwrap();
        state.pending = value;
        self.sync_pending_cache(&state);
    }

    /// Restore enable from snapshot
    pub fn set_enable(&self, values: &[u32]) {
        let mut state = self.state.lock().unwrap();
        for (i, &val) in values.iter().enumerate() {
            if i < state.enable.len() {
                state.enable[i] = val;
            }
        }
        // Sync all enable caches
        for i in 0..NUM_CONTEXTS.min(values.len()) {
            self.enable_cache[i].store(state.enable[i], Ordering::Release);
        }
    }

    /// Restore threshold from snapshot
    pub fn set_threshold(&self, values: &[u32]) {
        let mut state = self.state.lock().unwrap();
        for (i, &val) in values.iter().enumerate() {
            if i < state.threshold.len() {
                state.threshold[i] = val;
            }
        }
        // Sync all threshold caches
        for i in 0..NUM_CONTEXTS.min(values.len()) {
            self.threshold_cache[i].store(state.threshold[i], Ordering::Release);
        }
    }

    /// Restore active from snapshot
    pub fn set_active(&self, values: &[u32]) {
        let mut state = self.state.lock().unwrap();
        for (i, &val) in values.iter().enumerate() {
            if i < state.active.len() {
                state.active[i] = val;
            }
        }
        // Note: active is not cached, so no sync needed
    }

    // ============================================================
    // MMIO Load/Store
    // ============================================================

    pub fn load(&self, offset: u64, size: u64) -> Result<u64, MemoryError> {
        let mut state = self.state.lock().unwrap();
        if size != 4 {
            return Ok(0); 
        }

        // Priority registers: 0x000000 .. 0x0000FC (4 bytes each)
        if offset < 0x001000 {
            let idx = (offset >> 2) as usize;
            if idx < NUM_SOURCES {
                return Ok(state.priority[idx] as u64);
            }
        }
        // Pending bits: 0x001000
        if offset == 0x001000 {
            return Ok(state.pending as u64);
        }
        // Enable per context: 0x002000 + 0x80 * context
        if offset >= 0x002000 && offset < 0x002000 + 0x80 * (NUM_CONTEXTS as u64) {
            let ctx = ((offset - 0x002000) / 0x80) as usize;
            let inner = (offset - 0x002000) % 0x80;
            if ctx < NUM_CONTEXTS && inner == 0 {
                return Ok(state.enable[ctx] as u64);
            }
        }
        // Context registers: threshold @ 0x200000 + 0x1000 * ctx, claim @ +4
        if offset >= 0x200000 {
            let ctx = ((offset - 0x200000) / 0x1000) as usize;
            if ctx < NUM_CONTEXTS {
                let base = 0x200000 + (0x1000 * ctx as u64);
                if offset == base {
                    return Ok(state.threshold[ctx] as u64);
                }
                if offset == base + 4 {
                    let claim = Self::claim_interrupt_for_internal(&mut state, ctx);
                    if Self::debug_trace() {
                        eprintln!("[PLIC] SCLAIM ctx={} -> {}", ctx, claim);
                    }
                    return Ok(claim as u64);
                }
            }
        }

        Ok(0)
    }

    fn debug_trace() -> bool {
        // Helper to check if trace logging is enabled without importing log everywhere if not needed
        // or just use std::env
        std::env::var("RUST_LOG").map(|s| s.contains("trace")).unwrap_or(false)
    }

    pub fn store(&self, offset: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        let mut state = self.state.lock().unwrap();
        if size != 4 {
            return Ok(());
        }
        let val = value as u32;

        // ============================================================
        // Priority registers: 0x000000 .. 0x0000FC (4 bytes each)
        // ============================================================
        if offset < 0x001000 {
            let idx = (offset >> 2) as usize;
            if idx < NUM_SOURCES {
                state.priority[idx] = val;
                // Sync priority cache for this source
                self.sync_priority_cache(&state, idx);
            }
            return Ok(());
        }

        // ============================================================
        // Pending bits: 0x001000 (read-only to software)
        // ============================================================
        if offset == 0x001000 {
            // Pending is read-only - ignore writes
            return Ok(());
        }

        // ============================================================
        // Enable per context: 0x002000 + 0x80 * context
        // ============================================================
        if offset >= 0x002000 && offset < 0x002000 + 0x80 * (NUM_CONTEXTS as u64) {
            let ctx = ((offset - 0x002000) / 0x80) as usize;
            let inner = (offset - 0x002000) % 0x80;
            if ctx < NUM_CONTEXTS && inner == 0 {
                state.enable[ctx] = val;
                // Sync enable cache for this context
                self.sync_enable_cache(&state, ctx);
            }
            return Ok(());
        }

        // ============================================================
        // Context registers: threshold @ 0x200000 + 0x1000 * ctx
        //                    claim/complete @ 0x200000 + 0x1000 * ctx + 4
        // ============================================================
        if offset >= 0x200000 {
            let ctx = ((offset - 0x200000) / 0x1000) as usize;
            if ctx < NUM_CONTEXTS {
                let base = 0x200000 + (0x1000 * ctx as u64);
                
                if offset == base {
                    // Threshold write
                    state.threshold[ctx] = val;
                    // Sync threshold cache for this context
                    self.sync_threshold_cache(&state, ctx);
                    return Ok(());
                }
                
                if offset == base + 4 {
                    // Completion write: value is the source ID to complete
                    let id = (val & 0xffff) as u32;
                    if id > 0 && (id as usize) < NUM_SOURCES {
                        state.active[ctx] &= !(1 << id);
                        // No cache sync needed - active is not cached
                    }
                    return Ok(());
                }
            }
            return Ok(());
        }

        Ok(())
    }

    fn eligible_for_context(state: &PlicState, source: usize, ctx: usize) -> bool {
        let pending = ((state.pending >> source) & 1) == 1;
        let enabled = ((state.enable[ctx] >> source) & 1) == 1;
        let over_threshold = state.priority[source] > state.threshold[ctx];
        let not_active = ((state.active[ctx] >> source) & 1) == 0;
        pending && enabled && over_threshold && not_active
    }

    fn claim_interrupt_for_internal(state: &mut PlicState, ctx: usize) -> u32 {
        let mut max_prio = 0;
        let mut max_id = 0;

        for i in 1..NUM_SOURCES {
            if Self::eligible_for_context(state, i, ctx) {
                let prio = state.priority[i];
                if prio > max_prio {
                    max_prio = prio;
                    max_id = i as u32;
                }
            }
        }

        if max_id != 0 {
            // eprintln!("[PLIC] Claimed IRQ {} for context {} (prio {})", max_id, ctx, max_prio);
            // Mark in-flight for this context until completed.
            state.active[ctx] |= 1 << max_id;
        }
        max_id
    }

    pub fn claim_interrupt_for(&self, ctx: usize) -> u32 {
        let mut state = self.state.lock().unwrap();
        Self::claim_interrupt_for_internal(&mut state, ctx)
    }
    
    pub fn is_interrupt_pending(&self) -> bool {
        // For current single-hart flow, report S-mode context (1) if available, else context 0.
        let ctx = if NUM_CONTEXTS > 1 { 1 } else { 0 };
        self.is_interrupt_pending_for(ctx)
    }

    /// Accurate check using Mutex (for claim/complete path).
    /// 
    /// Use this when you need guaranteed correctness, such as
    /// before attempting a claim operation.
    pub fn is_interrupt_pending_for(&self, ctx: usize) -> bool {
        let state = self.state.lock().unwrap();
        if ctx >= NUM_CONTEXTS {
            return false;
        }
        for i in 1..NUM_SOURCES {
            if Self::eligible_for_context(&state, i, ctx) {
                if state.debug {
                    eprintln!("[PLIC] Interrupt pending for ctx={} source={}", ctx, i);
                }
                return true;
            }
        }
        // Debug: show why no interrupt
        if state.debug && state.pending != 0 {
            for i in 1..NUM_SOURCES {
                let pending = ((state.pending >> i) & 1) == 1;
                let enabled = ((state.enable[ctx] >> i) & 1) == 1;
                let over_threshold = state.priority[i] > state.threshold[ctx];
                let not_active = ((state.active[ctx] >> i) & 1) == 0;
                if pending {
                    eprintln!("[PLIC] Source {} pending but not eligible for ctx={}: enabled={} over_threshold={} (prio={} > thresh={}) not_active={}", 
                             i, ctx, enabled, over_threshold, state.priority[i], state.threshold[ctx], not_active);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_helpers() {
        assert_eq!(Plic::m_context(0), 0);
        assert_eq!(Plic::s_context(0), 1);
        assert_eq!(Plic::m_context(1), 2);
        assert_eq!(Plic::s_context(1), 3);
        assert_eq!(Plic::m_context(3), 6);
        assert_eq!(Plic::s_context(3), 7);
    }

    #[test]
    fn test_plic_multi_context() {
        let plic = Plic::new();
        
        // Enable source 1 for hart 1 S-mode (context 3)
        {
            let mut state = plic.state.lock().unwrap();
            state.enable[Plic::s_context(1)] = 1 << 1;
            state.priority[1] = 1;
            state.threshold[Plic::s_context(1)] = 0;
        }
        // Sync caches after manual state modification
        plic.sync_caches();
        
        plic.set_source_level(1, true);
        
        // Should be pending for hart 1 S-mode, not hart 0
        assert!(!plic.is_interrupt_pending_for(Plic::s_context(0)));
        assert!(plic.is_interrupt_pending_for(Plic::s_context(1)));
    }

    #[test]
    fn test_plic_claim_complete_context1() {
        let plic = Plic::new();
        // Priorities
        {
            let mut state = plic.state.lock().unwrap();
            state.priority[1] = 5;
            state.priority[10] = 3;
        }
        // Enable sources 1 and 10 for context 1 (S-mode)
        let enable_val = (1u32 << 1) | (1u32 << 10);
        let _ = plic.store(0x002000 + 0x80 * 1, 4, enable_val as u64);
        // Threshold 0 for context 1
        let _ = plic.store(0x200000 + 0x1000 * 1, 4, 0);
        // Sync priority caches (we modified them directly above)
        plic.sync_caches();
        // Assert device lines
        plic.set_source_level(1, true);
        plic.set_source_level(10, true);

        // Claim highest priority first (source 1)
        let id1 = plic.claim_interrupt_for(1);
        assert_eq!(id1, 1);
        // Next claim should return source 10 (since 1 is active)
        let id2 = plic.claim_interrupt_for(1);
        assert_eq!(id2, 10);
        // Complete source 1
        let _ = plic.store(0x200004 + 0x1000 * 1, 4, 1);
        // Claim again should allow source 1
        let id3 = plic.claim_interrupt_for(1);
        assert_eq!(id3, 1);
    }

    #[test]
    fn test_plic_cache_accessors() {
        let plic = Plic::new();
        
        // Initial state: all zeros
        assert_eq!(plic.pending_cached(), 0);
        assert_eq!(plic.enable_cached(0), 0);
        assert_eq!(plic.threshold_cached(0), 0);
        assert_eq!(plic.priority_cached(0), 0);
        
        // Out of bounds returns 0
        assert_eq!(plic.enable_cached(NUM_CONTEXTS + 100), 0);
        assert_eq!(plic.priority_cached(NUM_SOURCES + 100), 0);
    }

    #[test]
    fn test_plic_cache_sync() {
        let plic = Plic::new();
        
        // Manually modify state through store()
        plic.store(0x000000 + 4 * 10, 4, 5).unwrap();  // priority[10] = 5
        plic.store(0x002000, 4, 1 << 10).unwrap();     // enable[0] = bit 10
        plic.store(0x200000, 4, 2).unwrap();           // threshold[0] = 2
        
        // Set pending via set_source_level
        plic.set_source_level(10, true);
        
        // Caches should be synced by store() and set_source_level()
        assert_eq!(plic.priority_cached(10), 5);
        assert_eq!(plic.enable_cached(0), 1 << 10);
        assert_eq!(plic.threshold_cached(0), 2);
        assert!((plic.pending_cached() & (1 << 10)) != 0);
    }

    #[test]
    fn test_store_syncs_priority_cache() {
        let plic = Plic::new();
        
        // Write priority for source 5 via MMIO
        plic.store(0x000000 + 4 * 5, 4, 7).unwrap();
        
        // Cache should be updated
        assert_eq!(plic.priority_cached(5), 7);
        
        // Authoritative state should match
        assert_eq!(plic.get_priority()[5], 7);
    }

    #[test]
    fn test_store_syncs_enable_cache() {
        let plic = Plic::new();
        
        // Write enable for context 1 (S-mode for hart 0) via MMIO
        plic.store(0x002000 + 0x80 * 1, 4, 0x1234).unwrap();
        
        // Cache should be updated
        assert_eq!(plic.enable_cached(1), 0x1234);
    }

    #[test]
    fn test_store_syncs_threshold_cache() {
        let plic = Plic::new();
        
        // Write threshold for context 0 via MMIO
        plic.store(0x200000, 4, 3).unwrap();
        
        // Cache should be updated
        assert_eq!(plic.threshold_cached(0), 3);
    }

    #[test]
    fn test_set_source_level_syncs_cache() {
        let plic = Plic::new();
        
        assert_eq!(plic.pending_cached(), 0);
        
        plic.set_source_level(5, true);
        assert!((plic.pending_cached() & (1 << 5)) != 0);
        
        plic.set_source_level(5, false);
        assert!((plic.pending_cached() & (1 << 5)) == 0);
    }

    #[test]
    fn test_fast_pending_check() {
        let plic = Plic::new();
        
        // Setup: source 10, priority 5, enable for ctx 1, threshold 0
        plic.store(0x000000 + 4 * 10, 4, 5).unwrap();  // priority[10] = 5
        plic.store(0x002000 + 0x80 * 1, 4, 1 << 10).unwrap();  // enable[1] = bit 10
        plic.store(0x200000 + 0x1000 * 1, 4, 0).unwrap();  // threshold[1] = 0
        
        // No interrupt yet (not pending)
        assert!(!plic.is_interrupt_pending_for_fast(1));
        
        // Set source 10 as pending
        plic.set_source_level(10, true);
        
        // Now should detect interrupt
        assert!(plic.is_interrupt_pending_for_fast(1));
        
        // But not for context 0 (not enabled there)
        assert!(!plic.is_interrupt_pending_for_fast(0));
    }

    #[test]
    fn test_fast_check_respects_threshold() {
        let plic = Plic::new();
        
        // Setup: source 5, priority 3
        plic.store(0x000000 + 4 * 5, 4, 3).unwrap();
        plic.store(0x002000, 4, 1 << 5).unwrap();  // enable for ctx 0
        plic.set_source_level(5, true);
        
        // Threshold 0: should see interrupt
        plic.store(0x200000, 4, 0).unwrap();
        assert!(plic.is_interrupt_pending_for_fast(0));
        
        // Threshold 3: priority not > threshold, no interrupt
        plic.store(0x200000, 4, 3).unwrap();
        assert!(!plic.is_interrupt_pending_for_fast(0));
        
        // Threshold 2: priority > threshold, should see interrupt
        plic.store(0x200000, 4, 2).unwrap();
        assert!(plic.is_interrupt_pending_for_fast(0));
    }

    #[test]
    fn test_has_pending_candidate() {
        let plic = Plic::new();
        
        // Setup: source 3 enabled for ctx 0, but not pending
        plic.store(0x002000, 4, 1 << 3).unwrap();
        assert!(!plic.has_pending_candidate(0));
        
        // Set source 3 as pending
        plic.set_source_level(3, true);
        assert!(plic.has_pending_candidate(0));
        
        // Clear pending
        plic.set_source_level(3, false);
        assert!(!plic.has_pending_candidate(0));
    }
}
