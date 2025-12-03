//! JIT Cache with LRU eviction and memory limits.
//!
//! Manages compiled WASM functions keyed by start PC.
//!
//! ## Features
//!
//! - **LRU Eviction**: Least Recently Used blocks are evicted first
//! - **Memory Limits**: Both entry count and byte size limits
//! - **Invalidation**: Range-based invalidation for self-modifying code
//! - **Statistics**: Hit/miss ratios, eviction counts, memory usage
//!
//! ## State Machine
//!
//! Each block goes through the following states:
//!
//! ```text
//!                  ┌─────────────────┐
//!                  │     Cold        │
//!                  │  (not in cache) │
//!                  └────────┬────────┘
//!                           │ first execution
//!                           ▼
//!                  ┌─────────────────┐
//!                  │     Warming     │
//!                  │ (counting execs)│
//!                  └────────┬────────┘
//!                           │ exec_count >= threshold
//!                           ▼
//!                  ┌─────────────────┐
//!      ┌───────────│   Compiling     │
//!      │           │ (in worker)     │
//!      │           └────────┬────────┘
//!      │                    │ worker returns
//!      │                    ▼
//!      │           ┌─────────────────┐
//!      │           │     Ready       │───── execute JIT'd code
//!      │           │ (WASM cached)   │
//!      │           └─────────────────┘
//!      │
//!      │ worker returns "unsuitable"
//!      ▼
//! ┌─────────────────┐
//! │   Blacklisted   │───── always use interpreter
//! └─────────────────┘
//! ```

use lru::LruCache;
use std::collections::HashSet;
use std::num::NonZeroUsize;

/// Compiled JIT function (platform-specific).
#[cfg(target_arch = "wasm32")]
pub struct JitFunction {
    /// The compiled WebAssembly function
    pub func: js_sys::Function,
    /// Size of the WASM module in bytes
    pub wasm_size: usize,
    /// Number of times this function has been executed
    pub exec_count: u64,
    /// Block start PC
    pub pc: u64,
    /// Number of instructions in the block
    pub instruction_count: u32,
}

#[cfg(not(target_arch = "wasm32"))]
pub struct JitFunction {
    /// Compiled WASM bytes (for native, we'd use a different representation)
    pub wasm_bytes: Vec<u8>,
    /// Size in bytes
    pub wasm_size: usize,
    /// Execution count
    pub exec_count: u64,
    /// Block PC
    pub pc: u64,
    /// Instruction count
    pub instruction_count: u32,
}

/// Cache statistics.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    /// Cache lookup hits
    pub hits: u64,
    /// Cache lookup misses
    pub misses: u64,
    /// Number of entries inserted
    pub insertions: u64,
    /// Number of entries evicted (by LRU)
    pub evictions: u64,
    /// Total bytes of WASM compiled
    pub bytes_compiled: usize,
    /// Number of entries invalidated
    pub invalidations: u64,

    // Backward compatibility fields
    /// Total blocks compiled (alias for insertions)
    pub blocks_compiled: u64,
    /// Total instructions executed via JIT
    pub jit_instructions: u64,
    /// Cache hits (alias for hits)
    pub cache_hits: u64,
    /// Cache misses (alias for misses)
    pub cache_misses: u64,
}

impl CacheStats {
    /// Calculate hit ratio.
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Sync alias fields after updates.
    fn sync_aliases(&mut self) {
        self.cache_hits = self.hits;
        self.cache_misses = self.misses;
        self.blocks_compiled = self.insertions;
    }
}

/// JIT cache with memory limits and eviction.
pub struct JitCache {
    /// Compiled functions by PC (LRU ordered)
    functions: LruCache<u64, JitFunction>,

    /// Maximum total WASM bytes to cache
    max_bytes: usize,

    /// Current cached bytes
    current_bytes: usize,

    /// Statistics
    stats: CacheStats,

    /// Current generation (incremented on flush)
    pub generation: u32,

    /// PCs currently being compiled (to prevent duplicate compilation)
    compiling: HashSet<u64>,

    /// PCs that are blacklisted (unsuitable for JIT)
    blacklisted: HashSet<u64>,
}

impl JitCache {
    /// Create cache with size limits.
    ///
    /// # Arguments
    /// * `max_entries` - Maximum number of compiled functions
    /// * `max_bytes` - Maximum total WASM bytes
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            functions: LruCache::new(
                NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::new(1).unwrap()),
            ),
            max_bytes,
            current_bytes: 0,
            stats: CacheStats::default(),
            generation: 0,
            compiling: HashSet::new(),
            blacklisted: HashSet::new(),
        }
    }

    /// Create cache with only max entries (backward compatible).
    pub fn with_max_entries(max_entries: usize) -> Self {
        // Default to 16 MB max bytes
        Self::new(max_entries, 16 * 1024 * 1024)
    }

    /// Insert compiled function, evicting if necessary.
    pub fn insert(&mut self, pc: u64, func: JitFunction) {
        let wasm_size = func.wasm_size;

        // Evict until we have room (respecting byte limit)
        while self.current_bytes + wasm_size > self.max_bytes && !self.functions.is_empty() {
            if let Some((_, evicted)) = self.functions.pop_lru() {
                self.current_bytes = self.current_bytes.saturating_sub(evicted.wasm_size);
                self.stats.evictions += 1;
            }
        }

        // Remove old entry if exists (re-compilation)
        if let Some(old) = self.functions.pop(&pc) {
            self.current_bytes = self.current_bytes.saturating_sub(old.wasm_size);
        }

        // Remove from compiling set
        self.compiling.remove(&pc);

        self.functions.put(pc, func);
        self.current_bytes += wasm_size;
        self.stats.insertions += 1;
        self.stats.bytes_compiled += wasm_size;
        self.stats.sync_aliases();
    }

    /// Insert compiled WASM bytes (backward compatible API).
    pub fn insert_bytes(&mut self, pc: u64, wasm_bytes: Vec<u8>) {
        let wasm_size = wasm_bytes.len();
        let func = JitFunction {
            #[cfg(target_arch = "wasm32")]
            func: {
                // On WASM, we need to compile the bytes to get a function
                // For now, store a placeholder - actual compilation happens at execution
                js_sys::Function::new_no_args("")
            },
            #[cfg(not(target_arch = "wasm32"))]
            wasm_bytes,
            wasm_size,
            exec_count: 0,
            pc,
            instruction_count: 0,
        };
        self.insert(pc, func);
    }

    /// Look up compiled function (updates LRU order).
    pub fn get(&mut self, pc: u64) -> Option<&JitFunction> {
        // Check generation - if we've flushed, the entry is stale
        if self.functions.get(&pc).is_some() {
            self.stats.hits += 1;
            self.stats.sync_aliases();
            self.functions.get(&pc)
        } else {
            self.stats.misses += 1;
            self.stats.sync_aliases();
            None
        }
    }

    /// Look up compiled function mutably.
    pub fn get_mut(&mut self, pc: u64) -> Option<&mut JitFunction> {
        if self.functions.get(&pc).is_some() {
            self.stats.hits += 1;
            self.stats.sync_aliases();
            self.functions.get_mut(&pc)
        } else {
            self.stats.misses += 1;
            self.stats.sync_aliases();
            None
        }
    }

    /// Check if a function is cached (without updating LRU).
    pub fn contains(&self, pc: u64) -> bool {
        self.functions.contains(&pc)
    }

    /// Peek at a function without updating LRU order.
    pub fn peek(&self, pc: u64) -> Option<&JitFunction> {
        self.functions.peek(&pc)
    }

    /// Invalidate specific entry.
    pub fn invalidate(&mut self, pc: u64) -> bool {
        if let Some(entry) = self.functions.pop(&pc) {
            self.current_bytes = self.current_bytes.saturating_sub(entry.wasm_size);
            self.stats.invalidations += 1;
            true
        } else {
            false
        }
    }

    /// Clear all entries (alias for flush).
    pub fn clear(&mut self) {
        let count = self.functions.len();
        self.functions.clear();
        self.current_bytes = 0;
        self.stats.invalidations += count as u64;
        self.compiling.clear();
        // Note: blacklisted entries persist across clear()
    }

    /// Flush all entries (invalidate on TLB flush, SATP change).
    ///
    /// Increments the generation counter and clears all compiled functions.
    pub fn flush(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.clear();
    }

    /// Invalidate entries in address range (for self-modifying code).
    pub fn invalidate_range(&mut self, start: u64, end: u64) -> usize {
        let to_remove: Vec<_> = self
            .functions
            .iter()
            .filter(|(pc, _)| **pc >= start && **pc < end)
            .map(|(pc, _)| *pc)
            .collect();

        let count = to_remove.len();
        for pc in to_remove {
            self.invalidate(pc);
        }
        count
    }

    /// Invalidate entries that overlap with a physical page.
    pub fn invalidate_page(&mut self, page_addr: u64) -> usize {
        let page_start = page_addr & !0xFFF;
        let page_end = page_start + 0x1000;
        self.invalidate_range(page_start, page_end)
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Compilation State Tracking
    // ═══════════════════════════════════════════════════════════════════════

    /// Mark a block as being compiled (to prevent duplicate compilation).
    pub fn mark_compiling(&mut self, pc: u64) {
        self.compiling.insert(pc);
    }

    /// Check if a block is currently being compiled.
    pub fn is_compiling(&self, pc: u64) -> bool {
        self.compiling.contains(&pc)
    }

    /// Mark a block as unsuitable for JIT (will never be compiled).
    ///
    /// Blacklisted blocks will never be resubmitted for compilation,
    /// even across generations. This is used for blocks that:
    /// - Contain unsupported instructions
    /// - Have too many MMIO accesses
    /// - Failed compilation due to internal errors
    pub fn blacklist(&mut self, pc: u64) {
        self.compiling.remove(&pc);
        self.blacklisted.insert(pc);
    }

    /// Check if a block is blacklisted (unsuitable for JIT).
    pub fn is_blacklisted(&self, pc: u64) -> bool {
        self.blacklisted.contains(&pc)
    }

    /// Clear the blacklist (e.g., on major config change).
    pub fn clear_blacklist(&mut self) {
        self.blacklisted.clear();
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Statistics and Capacity
    // ═══════════════════════════════════════════════════════════════════════

    /// Get cache statistics.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Get current memory usage.
    pub fn memory_usage(&self) -> usize {
        self.current_bytes
    }

    /// Get number of cached entries.
    pub fn entry_count(&self) -> usize {
        self.functions.len()
    }

    /// Get cache capacity (max entries).
    pub fn capacity(&self) -> usize {
        self.functions.cap().get()
    }

    /// Get maximum byte limit.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Iterate over all cached functions (most recent first).
    pub fn iter(&self) -> impl Iterator<Item = (&u64, &JitFunction)> {
        self.functions.iter()
    }

    /// Get the most recently used entry.
    pub fn most_recent(&self) -> Option<(&u64, &JitFunction)> {
        self.functions.iter().next()
    }

    /// Get the least recently used entry.
    pub fn least_recent(&self) -> Option<(&u64, &JitFunction)> {
        self.functions.iter().last()
    }

    /// Resize the cache (may cause evictions).
    pub fn resize(&mut self, new_max_entries: usize, new_max_bytes: usize) {
        self.max_bytes = new_max_bytes;

        // Evict to fit new byte limit
        while self.current_bytes > self.max_bytes && !self.functions.is_empty() {
            if let Some((_, evicted)) = self.functions.pop_lru() {
                self.current_bytes = self.current_bytes.saturating_sub(evicted.wasm_size);
                self.stats.evictions += 1;
            }
        }

        // Resize entry limit (LruCache handles eviction)
        if let Some(new_cap) = NonZeroUsize::new(new_max_entries) {
            self.functions.resize(new_cap);
        }
    }

    /// Reset statistics.
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Increment JIT instruction count.
    pub fn add_jit_instructions(&mut self, count: u64) {
        self.stats.jit_instructions += count;
    }
}

impl Default for JitCache {
    fn default() -> Self {
        // Default: 1024 entries, 16 MB max
        Self::new(1024, 16 * 1024 * 1024)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_func(size: usize, pc: u64) -> JitFunction {
        JitFunction {
            #[cfg(target_arch = "wasm32")]
            func: js_sys::Function::new_no_args(""),
            #[cfg(not(target_arch = "wasm32"))]
            wasm_bytes: vec![0; size],
            wasm_size: size,
            exec_count: 0,
            pc,
            instruction_count: 1,
        }
    }

    #[test]
    fn test_basic_insert_get() {
        let mut cache = JitCache::new(10, 10000);

        let func = make_test_func(100, 0x1000);
        cache.insert(0x1000, func);

        assert!(cache.contains(0x1000));
        assert!(cache.get(0x1000).is_some());
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_lru_eviction_by_count() {
        let mut cache = JitCache::new(2, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x2000, make_test_func(100, 0x2000));
        cache.insert(0x3000, make_test_func(100, 0x3000)); // Evicts 0x1000

        assert!(!cache.contains(0x1000));
        assert!(cache.contains(0x2000));
        assert!(cache.contains(0x3000));
    }

    #[test]
    fn test_eviction_by_bytes() {
        let mut cache = JitCache::new(100, 500);

        cache.insert(0x1000, make_test_func(200, 0x1000));
        cache.insert(0x2000, make_test_func(200, 0x2000));
        cache.insert(0x3000, make_test_func(200, 0x3000)); // Evicts 0x1000 to make room

        assert!(!cache.contains(0x1000));
        assert!(cache.memory_usage() <= 500);
    }

    #[test]
    fn test_invalidate_range() {
        let mut cache = JitCache::new(10, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x1100, make_test_func(100, 0x1100));
        cache.insert(0x2000, make_test_func(100, 0x2000));

        let count = cache.invalidate_range(0x1000, 0x1200);

        assert_eq!(count, 2);
        assert!(!cache.contains(0x1000));
        assert!(!cache.contains(0x1100));
        assert!(cache.contains(0x2000));
    }

    #[test]
    fn test_lru_order_updates_on_get() {
        let mut cache = JitCache::new(3, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x2000, make_test_func(100, 0x2000));
        cache.insert(0x3000, make_test_func(100, 0x3000));

        // Access 0x1000, making it most recent
        let _ = cache.get(0x1000);

        // Insert a new entry, should evict 0x2000 (now LRU)
        cache.insert(0x4000, make_test_func(100, 0x4000));

        assert!(cache.contains(0x1000)); // Still present (was accessed)
        assert!(!cache.contains(0x2000)); // Evicted
        assert!(cache.contains(0x3000));
        assert!(cache.contains(0x4000));
    }

    #[test]
    fn test_peek_does_not_update_lru() {
        let mut cache = JitCache::new(3, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x2000, make_test_func(100, 0x2000));
        cache.insert(0x3000, make_test_func(100, 0x3000));

        // Peek at 0x1000 (should NOT update LRU order)
        let _ = cache.peek(0x1000);

        // Insert a new entry, should evict 0x1000 (still LRU)
        cache.insert(0x4000, make_test_func(100, 0x4000));

        assert!(!cache.contains(0x1000)); // Evicted (peek didn't save it)
        assert!(cache.contains(0x2000));
        assert!(cache.contains(0x3000));
        assert!(cache.contains(0x4000));
    }

    #[test]
    fn test_statistics() {
        let mut cache = JitCache::new(10, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x2000, make_test_func(100, 0x2000));

        // Hits
        let _ = cache.get(0x1000);
        let _ = cache.get(0x2000);

        // Miss
        let _ = cache.get(0x3000);

        let stats = cache.stats();
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.bytes_compiled, 200);

        // Check backward-compatible aliases
        assert_eq!(stats.blocks_compiled, 2);
        assert_eq!(stats.cache_hits, 2);
        assert_eq!(stats.cache_misses, 1);
    }

    #[test]
    fn test_hit_ratio() {
        let mut cache = JitCache::new(10, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));

        // 3 hits
        let _ = cache.get(0x1000);
        let _ = cache.get(0x1000);
        let _ = cache.get(0x1000);

        // 1 miss
        let _ = cache.get(0x2000);

        assert!((cache.stats().hit_ratio() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_memory_tracking() {
        let mut cache = JitCache::new(100, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        assert_eq!(cache.memory_usage(), 100);

        cache.insert(0x2000, make_test_func(200, 0x2000));
        assert_eq!(cache.memory_usage(), 300);

        cache.invalidate(0x1000);
        assert_eq!(cache.memory_usage(), 200);

        cache.clear();
        assert_eq!(cache.memory_usage(), 0);
    }

    #[test]
    fn test_invalidate_page() {
        let mut cache = JitCache::new(10, 10000);

        // Page 1 (0x1000-0x1FFF)
        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x1500, make_test_func(100, 0x1500));
        // Page 2 (0x2000-0x2FFF)
        cache.insert(0x2000, make_test_func(100, 0x2000));

        let count = cache.invalidate_page(0x1234); // Should invalidate page 1

        assert_eq!(count, 2);
        assert!(!cache.contains(0x1000));
        assert!(!cache.contains(0x1500));
        assert!(cache.contains(0x2000));
    }

    #[test]
    fn test_resize_smaller() {
        let mut cache = JitCache::new(10, 10000);

        for i in 0..5 {
            cache.insert(i * 0x1000, make_test_func(100, i * 0x1000));
        }

        assert_eq!(cache.entry_count(), 5);

        // Resize to smaller capacity
        cache.resize(3, 10000);

        assert_eq!(cache.entry_count(), 3);
        // Should keep the 3 most recent
    }

    #[test]
    fn test_resize_byte_limit() {
        let mut cache = JitCache::new(100, 1000);

        cache.insert(0x1000, make_test_func(300, 0x1000));
        cache.insert(0x2000, make_test_func(300, 0x2000));
        cache.insert(0x3000, make_test_func(300, 0x3000));

        assert_eq!(cache.memory_usage(), 900);

        // Resize to smaller byte limit
        cache.resize(100, 500);

        assert!(cache.memory_usage() <= 500);
    }

    #[test]
    fn test_recompilation_replaces_entry() {
        let mut cache = JitCache::new(10, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        assert_eq!(cache.memory_usage(), 100);

        // Re-insert with different size (recompilation)
        cache.insert(0x1000, make_test_func(200, 0x1000));
        assert_eq!(cache.memory_usage(), 200);
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_most_least_recent() {
        let mut cache = JitCache::new(10, 10000);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.insert(0x2000, make_test_func(100, 0x2000));
        cache.insert(0x3000, make_test_func(100, 0x3000));

        assert_eq!(*cache.most_recent().unwrap().0, 0x3000);
        assert_eq!(*cache.least_recent().unwrap().0, 0x1000);

        // Access 0x1000, making it most recent
        let _ = cache.get(0x1000);

        assert_eq!(*cache.most_recent().unwrap().0, 0x1000);
        assert_eq!(*cache.least_recent().unwrap().0, 0x2000);
    }

    #[test]
    fn test_compiling_state() {
        let mut cache = JitCache::new(10, 10000);

        assert!(!cache.is_compiling(0x1000));

        cache.mark_compiling(0x1000);
        assert!(cache.is_compiling(0x1000));

        // Insert clears compiling state
        cache.insert(0x1000, make_test_func(100, 0x1000));
        assert!(!cache.is_compiling(0x1000));
    }

    #[test]
    fn test_blacklist() {
        let mut cache = JitCache::new(10, 10000);

        assert!(!cache.is_blacklisted(0x1000));

        cache.blacklist(0x1000);
        assert!(cache.is_blacklisted(0x1000));

        // Blacklist persists across flush
        cache.flush();
        assert!(cache.is_blacklisted(0x1000));

        // But can be cleared explicitly
        cache.clear_blacklist();
        assert!(!cache.is_blacklisted(0x1000));
    }

    #[test]
    fn test_flush_increments_generation() {
        let mut cache = JitCache::new(10, 10000);

        assert_eq!(cache.generation, 0);

        cache.insert(0x1000, make_test_func(100, 0x1000));
        cache.flush();

        assert_eq!(cache.generation, 1);
        assert!(!cache.contains(0x1000));
    }
}
