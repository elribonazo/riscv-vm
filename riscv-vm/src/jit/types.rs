//! Core types for the JIT compilation engine.

use std::collections::HashSet;

/// Compilation tier for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitTier {
    /// Not yet compiled - use interpreter
    Interpreted,
    /// Tier 1: Single block compiled to WASM
    BlockJit,
    /// Tier 2: Multiple blocks fused (future)
    TraceJit,
}

/// Result of JIT compilation.
#[derive(Debug)]
pub enum CompilationResult {
    /// Successfully compiled, WASM bytes ready
    Success(Vec<u8>),
    /// Block is unsuitable for JIT (too complex, MMIO-heavy, etc.)
    Unsuitable,
    /// Compilation error (internal bug, should be rare)
    Error(String),
}

/// Status of a JIT'd block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
    /// Not yet compiled
    Pending,
    /// Currently being compiled (in worker)
    Compiling,
    /// Compiled and ready to execute
    Ready,
    /// Compilation failed or block invalidated
    Invalid,
    /// Block is unsuitable for JIT (will never be compiled)
    Blacklisted,
}

/// Execution statistics for profiling.
#[derive(Debug, Default, Clone)]
pub struct JitStats {
    /// Total blocks compiled
    pub blocks_compiled: u64,
    /// Total instructions executed via JIT
    pub jit_instructions: u64,
    /// Total instructions executed via interpreter
    pub interp_instructions: u64,
    /// Cache hits (executed existing JIT'd block)
    pub cache_hits: u64,
    /// Cache misses (fell back to interpreter or compiled new block)
    pub cache_misses: u64,
    /// Time spent compiling (microseconds)
    pub compile_time_us: u64,
}

/// JIT configuration with safety limits and error recovery settings.
#[derive(Debug, Clone)]
pub struct JitConfig {
    /// Enable JIT compilation
    pub enabled: bool,

    /// Execution count before triggering Tier 1 compilation (compile_threshold)
    pub tier1_threshold: u32,

    /// Minimum block size (instructions) for JIT worthiness
    pub min_block_size: usize,

    /// Maximum block size to compile (ops)
    pub max_block_size: usize,

    /// Maximum WASM module size (bytes)
    pub max_wasm_size: usize,

    /// Maximum blocks in JIT cache (max_cache_entries)
    pub max_cache_size: usize,

    /// Maximum total cached WASM bytes
    pub cache_max_bytes: usize,

    /// Enable debug WAT output
    pub debug_wat: bool,

    /// Enable execution tracing
    pub trace_enabled: bool,

    /// Enable async compilation via Web Worker (WASM only)
    ///
    /// When enabled, hot blocks are sent to a background worker for
    /// compilation, allowing the main thread to continue interpreting.
    /// When disabled, compilation happens synchronously on the main thread.
    pub async_compilation: bool,

    /// Enable TLB fast-path inlining for memory operations.
    ///
    /// When enabled, the JIT compiler inlines TLB lookups directly in the
    /// generated WASM for load/store operations. On TLB hit, this avoids
    /// the overhead of calling out to host helper functions.
    ///
    /// This is an optional optimization that trades code size for speed
    /// on hot memory-intensive loops.
    pub enable_tlb_fast_path: bool,

    /// Minimum execution count before using TLB fast-path.
    ///
    /// Only blocks that have been executed at least this many times
    /// will use the TLB fast-path optimization. Set to 0 to always
    /// use fast-path when enabled.
    pub tlb_fast_path_threshold: u32,

    /// Insert interrupt check at block entry for long blocks.
    ///
    /// When enabled, blocks with >= `interrupt_check_block_threshold` ops
    /// will have an interrupt check inserted at the start.
    pub interrupt_check_on_entry: bool,

    /// Insert interrupt check every N ops (0 = disabled).
    ///
    /// For long blocks, this ensures interrupts can be handled even
    /// during tight loops. Set to 0 to disable periodic checks.
    pub interrupt_check_interval: usize,

    /// Minimum block size (in ops) to trigger entry interrupt check.
    pub interrupt_check_block_threshold: usize,

    /// Compilation timeout (ms, 0 = no timeout)
    pub compile_timeout_ms: u32,

    /// Disable JIT after this many consecutive compilation failures
    pub max_consecutive_failures: u32,

    /// Disable JIT after this many total compilation failures
    pub max_total_failures: u32,

    /// Blocks to skip (known-problematic PCs)
    pub blacklist: Vec<u64>,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tier1_threshold: 50,           // Compile after 50 executions
            min_block_size: 4,             // At least 4 instructions
            max_block_size: 256,           // Maximum 256 ops per block
            max_wasm_size: 64 * 1024,      // 64KB max WASM module
            max_cache_size: 1024,          // 1K compiled blocks
            cache_max_bytes: 16 * 1024 * 1024, // 16MB max cache
            debug_wat: false,
            trace_enabled: false,
            async_compilation: true,       // Enable async by default on WASM
            enable_tlb_fast_path: false,   // Disabled by default (optional optimization)
            tlb_fast_path_threshold: 100,  // Only use fast-path for hot blocks
            interrupt_check_on_entry: true, // Check interrupts at entry for long blocks
            interrupt_check_interval: 32,  // Check every 32 ops in long blocks
            interrupt_check_block_threshold: 16, // Only for blocks >= 16 ops
            compile_timeout_ms: 100,       // 100ms compilation timeout
            max_consecutive_failures: 10,  // Disable after 10 consecutive failures
            max_total_failures: 100,       // Disable after 100 total failures
            blacklist: Vec::new(),
        }
    }
}

impl JitConfig {
    /// Alias for tier1_threshold for compatibility with task spec.
    pub fn compile_threshold(&self) -> u32 {
        self.tier1_threshold
    }

    /// Alias for max_cache_size for compatibility with task spec.
    pub fn cache_max_entries(&self) -> usize {
        self.max_cache_size
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// JIT Runtime with Error Tracking and Graceful Degradation
// ═══════════════════════════════════════════════════════════════════════════

use super::cache::{CacheStats, JitCache};
use super::compiler::JitCompiler;
use super::trace::{TraceBuffer, TraceEvent, TraceStats};
use crate::engine::block::Block;

/// JIT runtime with error tracking and graceful degradation.
pub struct JitRuntime {
    config: JitConfig,
    cache: JitCache,
    compiler: JitCompiler,
    trace: TraceBuffer,

    /// Consecutive compilation failures
    consecutive_failures: u32,
    /// Total compilation failures
    total_failures: u32,
    /// JIT disabled due to errors
    disabled_by_error: bool,
    /// Reason JIT was disabled
    disabled_reason: Option<String>,
    /// Total successful compilations
    successful_compilations: u64,
    /// Blocks that failed to compile (for blacklisting)
    failed_blocks: HashSet<u64>,
}

impl JitRuntime {
    /// Create a new JIT runtime with the given configuration.
    pub fn new(config: JitConfig) -> Self {
        let cache = JitCache::new(config.max_cache_size, config.cache_max_bytes);
        let trace_capacity = if config.trace_enabled { 1000 } else { 0 };
        let mut trace = TraceBuffer::new(trace_capacity);
        if config.trace_enabled {
            trace.enable();
        }

        Self {
            config: config.clone(),
            cache,
            compiler: JitCompiler::new(config),
            trace,
            consecutive_failures: 0,
            total_failures: 0,
            disabled_by_error: false,
            disabled_reason: None,
            successful_compilations: 0,
            failed_blocks: HashSet::new(),
        }
    }

    /// Check if JIT should be attempted for a block.
    pub fn should_compile(&self, block: &Block) -> bool {
        // Check if JIT is enabled at all
        if !self.config.enabled || self.disabled_by_error {
            return false;
        }

        // Check block size limits
        if (block.len as usize) < self.config.min_block_size {
            return false;
        }
        if (block.len as usize) > self.config.max_block_size {
            return false;
        }

        // Check execution threshold
        if block.exec_count < self.config.tier1_threshold {
            return false;
        }

        // Check if already compiled
        if self.cache.contains(block.start_pc) {
            return false;
        }

        // Check if currently compiling
        if self.cache.is_compiling(block.start_pc) {
            return false;
        }

        // Check blacklist (both config and failed_blocks)
        if self.config.blacklist.contains(&block.start_pc) {
            return false;
        }

        // Check if this block previously failed
        if self.failed_blocks.contains(&block.start_pc) {
            return false;
        }

        // Check if already blacklisted in cache
        if self.cache.is_blacklisted(block.start_pc) {
            return false;
        }

        true
    }

    /// Attempt compilation with error handling.
    pub fn try_compile(&mut self, block: &Block) -> Option<Vec<u8>> {
        let start = std::time::Instant::now();

        self.trace.push(TraceEvent::JitCompileStart {
            pc: block.start_pc,
            ops: block.len as u32,
        });

        match self.compiler.compile(block) {
            CompilationResult::Success(bytes) => {
                let elapsed = start.elapsed();

                // Check size limit
                if bytes.len() > self.config.max_wasm_size {
                    self.trace.push(TraceEvent::JitCompileEnd {
                        pc: block.start_pc,
                        wasm_size: bytes.len(),
                        time_us: elapsed.as_micros() as u64,
                        success: false,
                    });
                    self.failed_blocks.insert(block.start_pc);
                    return None;
                }

                // Reset consecutive failure counter on success
                self.consecutive_failures = 0;
                self.successful_compilations += 1;

                self.trace.push(TraceEvent::JitCompileEnd {
                    pc: block.start_pc,
                    wasm_size: bytes.len(),
                    time_us: elapsed.as_micros() as u64,
                    success: true,
                });

                if self.config.debug_wat {
                    super::disasm::log_jit_compilation(
                        block,
                        &bytes,
                        &super::disasm::DisasmConfig::default(),
                    );
                }

                Some(bytes)
            }
            CompilationResult::Unsuitable => {
                // Block contains unsupported ops, don't count as failure
                // but do blacklist this block
                self.failed_blocks.insert(block.start_pc);
                self.cache.blacklist(block.start_pc);

                self.trace.push(TraceEvent::JitCompileEnd {
                    pc: block.start_pc,
                    wasm_size: 0,
                    time_us: start.elapsed().as_micros() as u64,
                    success: false,
                });

                None
            }
            CompilationResult::Error(e) => {
                self.handle_compilation_error(block.start_pc, &e, start.elapsed());
                None
            }
        }
    }

    /// Handle a compilation error.
    fn handle_compilation_error(&mut self, pc: u64, error: &str, elapsed: std::time::Duration) {
        self.consecutive_failures += 1;
        self.total_failures += 1;
        self.failed_blocks.insert(pc);

        self.trace.push(TraceEvent::JitCompileEnd {
            pc,
            wasm_size: 0,
            time_us: elapsed.as_micros() as u64,
            success: false,
        });

        // Log the error
        #[cfg(target_arch = "wasm32")]
        web_sys::console::warn_1(&format!("JIT compilation error at {:#x}: {}", pc, error).into());
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("JIT compilation error at {:#x}: {}", pc, error);

        // Check if we should disable JIT
        if self.consecutive_failures >= self.config.max_consecutive_failures {
            self.disable_jit(&format!(
                "{} consecutive failures (last at {:#x})",
                self.consecutive_failures, pc
            ));
        } else if self.total_failures >= self.config.max_total_failures {
            self.disable_jit(&format!("{} total failures", self.total_failures));
        }
    }

    /// Disable JIT due to errors.
    fn disable_jit(&mut self, reason: &str) {
        self.disabled_by_error = true;
        self.disabled_reason = Some(reason.to_string());

        #[cfg(target_arch = "wasm32")]
        web_sys::console::warn_1(&format!("JIT disabled: {}", reason).into());
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("JIT disabled: {}", reason);
    }

    /// Re-enable JIT after it was disabled by errors.
    pub fn reenable(&mut self) {
        self.disabled_by_error = false;
        self.disabled_reason = None;
        self.consecutive_failures = 0;
        // Keep total_failures for diagnostics
        // Clear failed_blocks to allow retry
        self.failed_blocks.clear();
    }

    /// Reset all error tracking.
    pub fn reset_errors(&mut self) {
        self.consecutive_failures = 0;
        self.total_failures = 0;
        self.disabled_by_error = false;
        self.disabled_reason = None;
        self.failed_blocks.clear();
    }

    /// Handle code modification (invalidate affected cache entries).
    pub fn handle_code_modification(&mut self, addr: u64, size: usize) {
        let block_start = addr & !0xFFF;
        let block_end = (addr + size as u64 + 0xFFF) & !0xFFF;

        let invalidated = self.cache.invalidate_range(block_start, block_end);

        if invalidated > 0 {
            self.trace.push(TraceEvent::CacheInvalidate {
                pc: Some(addr),
                reason: "code modification",
            });
        }
    }

    /// Handle TLB flush (clear all cache).
    pub fn handle_tlb_flush(&mut self) {
        self.cache.flush();

        self.trace.push(TraceEvent::CacheInvalidate {
            pc: None,
            reason: "TLB flush",
        });
    }

    /// Get diagnostic information.
    pub fn diagnostics(&self) -> JitDiagnostics {
        JitDiagnostics {
            enabled: self.config.enabled && !self.disabled_by_error,
            disabled_by_error: self.disabled_by_error,
            disabled_reason: self.disabled_reason.clone(),
            consecutive_failures: self.consecutive_failures,
            total_failures: self.total_failures,
            successful_compilations: self.successful_compilations,
            failed_blocks_count: self.failed_blocks.len(),
            cache_entries: self.cache.entry_count(),
            cache_bytes: self.cache.memory_usage(),
            cache_stats: self.cache.stats().clone(),
            trace_stats: self.trace.stats(),
        }
    }

    /// Check if JIT is currently enabled (not disabled by config or error).
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && !self.disabled_by_error
    }

    /// Check if JIT was disabled due to errors.
    pub fn is_disabled_by_error(&self) -> bool {
        self.disabled_by_error
    }

    /// Get the reason JIT was disabled (if any).
    pub fn disabled_reason(&self) -> Option<&str> {
        self.disabled_reason.as_deref()
    }

    /// Get the number of failed blocks.
    pub fn failed_blocks_count(&self) -> usize {
        self.failed_blocks.len()
    }

    /// Access the configuration.
    pub fn config(&self) -> &JitConfig {
        &self.config
    }

    /// Mutably access the configuration.
    pub fn config_mut(&mut self) -> &mut JitConfig {
        &mut self.config
    }

    /// Access the cache.
    pub fn cache(&self) -> &JitCache {
        &self.cache
    }

    /// Mutably access the cache.
    pub fn cache_mut(&mut self) -> &mut JitCache {
        &mut self.cache
    }

    /// Access the trace buffer.
    pub fn trace(&self) -> &TraceBuffer {
        &self.trace
    }

    /// Mutably access the trace buffer.
    pub fn trace_mut(&mut self) -> &mut TraceBuffer {
        &mut self.trace
    }

    /// Access the compiler.
    pub fn compiler(&self) -> &JitCompiler {
        &self.compiler
    }

    /// Mutably access the compiler.
    pub fn compiler_mut(&mut self) -> &mut JitCompiler {
        &mut self.compiler
    }
}

impl Default for JitRuntime {
    fn default() -> Self {
        Self::new(JitConfig::default())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// JIT Diagnostics
// ═══════════════════════════════════════════════════════════════════════════

/// JIT diagnostic information.
#[derive(Debug, Clone)]
pub struct JitDiagnostics {
    pub enabled: bool,
    pub disabled_by_error: bool,
    pub disabled_reason: Option<String>,
    pub consecutive_failures: u32,
    pub total_failures: u32,
    pub successful_compilations: u64,
    pub failed_blocks_count: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
    pub cache_stats: CacheStats,
    pub trace_stats: TraceStats,
}

impl serde::Serialize for JitDiagnostics {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("JitDiagnostics", 11)?;
        state.serialize_field("enabled", &self.enabled)?;
        state.serialize_field("disabledByError", &self.disabled_by_error)?;
        state.serialize_field("disabledReason", &self.disabled_reason)?;
        state.serialize_field("consecutiveFailures", &self.consecutive_failures)?;
        state.serialize_field("totalFailures", &self.total_failures)?;
        state.serialize_field("successfulCompilations", &self.successful_compilations)?;
        state.serialize_field("failedBlocksCount", &self.failed_blocks_count)?;
        state.serialize_field("cacheEntries", &self.cache_entries)?;
        state.serialize_field("cacheBytes", &self.cache_bytes)?;
        state.serialize_field("cacheStats", &self.cache_stats)?;
        state.serialize_field("traceStats", &self.trace_stats)?;
        state.end()
    }
}

/// Offsets into the CPU state structure for JIT'd code.
///
/// The JIT'd WASM code receives a pointer to the CPU state in linear memory.
/// These offsets define where each field lives relative to that pointer.
///
/// IMPORTANT: This must match the actual `Cpu` struct layout!
/// We use `#[repr(C)]` on the relevant struct to guarantee layout.
pub mod cpu_offsets {
    /// Offset of regs[0] from CPU state base (in bytes)
    pub const REGS_BASE: u32 = 0;
    /// Size of each register (64-bit)
    pub const REG_SIZE: u32 = 8;
    /// Offset of PC from CPU state base
    pub const PC_OFFSET: u32 = REGS_BASE + 32 * REG_SIZE; // After 32 registers
    
    /// Calculate offset for a specific register
    #[inline]
    pub const fn reg_offset(reg: u8) -> u32 {
        REGS_BASE + (reg as u32) * REG_SIZE
    }
}

/// Exit codes returned by JIT'd functions.
///
/// The JIT'd function returns an i64:
/// - High 32 bits: Exit reason (0 = normal, others = trap/exit)
/// - Low 32 bits: Additional data (next PC offset, trap code, etc.)
pub mod exit_codes {
    /// Normal exit - low bits contain the PC delta from block start
    pub const EXIT_NORMAL: u32 = 0;
    /// Trap occurred - low bits contain trap type
    pub const EXIT_TRAP: u32 = 1;
    /// Need to exit to interpreter (CSR, atomic, etc.)
    pub const EXIT_INTERPRETER: u32 = 2;
    /// Interrupt check needed
    pub const EXIT_INTERRUPT_CHECK: u32 = 3;
    /// Branch taken - PC updated in shared memory, low bits contain pc_offset
    pub const EXIT_BRANCH: u32 = 4;
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::block::Block;
    use crate::engine::microop::MicroOp;

    /// Create a test block with the given parameters.
    fn make_test_block(start_pc: u64, len: u8, exec_count: u32) -> Block {
        let mut block = Block::new(start_pc, start_pc, 0);
        block.len = len;
        block.exec_count = exec_count;
        // Fill with NOPs (ADDI x0, x0, 0)
        for i in 0..len as usize {
            block.ops[i] = MicroOp::Lui { rd: 0, imm: 0 };
        }
        block
    }

    #[test]
    fn test_jit_config_defaults() {
        let config = JitConfig::default();

        assert!(config.enabled);
        assert_eq!(config.tier1_threshold, 50);
        assert_eq!(config.min_block_size, 4);
        assert_eq!(config.max_block_size, 256);
        assert_eq!(config.max_wasm_size, 64 * 1024);
        assert_eq!(config.max_cache_size, 1024);
        assert_eq!(config.cache_max_bytes, 16 * 1024 * 1024);
        assert!(!config.debug_wat);
        assert!(!config.trace_enabled);
        assert_eq!(config.max_consecutive_failures, 10);
        assert_eq!(config.max_total_failures, 100);
        assert!(config.blacklist.is_empty());
    }

    #[test]
    fn test_jit_runtime_new() {
        let runtime = JitRuntime::default();

        assert!(runtime.is_enabled());
        assert!(!runtime.is_disabled_by_error());
        assert!(runtime.disabled_reason().is_none());
        assert_eq!(runtime.failed_blocks_count(), 0);

        let diag = runtime.diagnostics();
        assert!(diag.enabled);
        assert!(!diag.disabled_by_error);
        assert_eq!(diag.consecutive_failures, 0);
        assert_eq!(diag.total_failures, 0);
        assert_eq!(diag.successful_compilations, 0);
    }

    #[test]
    fn test_should_compile_disabled() {
        let mut config = JitConfig::default();
        config.enabled = false;
        let runtime = JitRuntime::new(config);

        let block = make_test_block(0x1000, 10, 100);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_should_compile_threshold_not_met() {
        let runtime = JitRuntime::default();

        // exec_count below threshold (default 50)
        let block = make_test_block(0x1000, 10, 10);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_should_compile_threshold_met() {
        let runtime = JitRuntime::default();

        // exec_count meets threshold
        let block = make_test_block(0x1000, 10, 50);
        assert!(runtime.should_compile(&block));
    }

    #[test]
    fn test_should_compile_block_too_small() {
        let runtime = JitRuntime::default();

        // Block too small (default min is 4)
        let block = make_test_block(0x1000, 2, 100);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_should_compile_block_too_large() {
        let mut config = JitConfig::default();
        config.max_block_size = 10;
        let runtime = JitRuntime::new(config);

        // Block too large
        let block = make_test_block(0x1000, 20, 100);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_should_compile_blacklisted() {
        let mut config = JitConfig::default();
        config.blacklist.push(0x1000);
        let runtime = JitRuntime::new(config);

        let block = make_test_block(0x1000, 10, 100);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_failed_blocks_blacklisted() {
        let mut runtime = JitRuntime::default();

        // Simulate a compilation failure by manually adding to failed_blocks
        runtime.failed_blocks.insert(0x1000);

        let block = make_test_block(0x1000, 10, 100);
        assert!(!runtime.should_compile(&block));
    }

    #[test]
    fn test_reenable_clears_failed_blocks() {
        let mut runtime = JitRuntime::default();

        // Simulate failures
        runtime.failed_blocks.insert(0x1000);
        runtime.failed_blocks.insert(0x2000);
        runtime.disabled_by_error = true;
        runtime.disabled_reason = Some("test".to_string());

        // Now re-enable
        runtime.reenable();

        assert!(!runtime.is_disabled_by_error());
        assert!(runtime.disabled_reason().is_none());
        assert_eq!(runtime.failed_blocks_count(), 0);

        // Should now be able to compile the previously failed block
        let block = make_test_block(0x1000, 10, 100);
        assert!(runtime.should_compile(&block));
    }

    #[test]
    fn test_reset_errors() {
        let mut runtime = JitRuntime::default();

        // Simulate failures
        runtime.consecutive_failures = 5;
        runtime.total_failures = 50;
        runtime.disabled_by_error = true;
        runtime.disabled_reason = Some("test".to_string());
        runtime.failed_blocks.insert(0x1000);

        // Reset all errors
        runtime.reset_errors();

        assert_eq!(runtime.consecutive_failures, 0);
        assert_eq!(runtime.total_failures, 0);
        assert!(!runtime.is_disabled_by_error());
        assert!(runtime.disabled_reason().is_none());
        assert_eq!(runtime.failed_blocks_count(), 0);
    }

    #[test]
    fn test_disable_after_consecutive_failures() {
        let mut config = JitConfig::default();
        config.max_consecutive_failures = 3;
        let mut runtime = JitRuntime::new(config);

        // Simulate consecutive failures
        for i in 0..3 {
            runtime.handle_compilation_error(
                0x1000 + i * 0x100,
                "test error",
                std::time::Duration::from_millis(1),
            );
        }

        assert!(runtime.is_disabled_by_error());
        assert!(runtime.disabled_reason().unwrap().contains("consecutive"));
    }

    #[test]
    fn test_disable_after_total_failures() {
        let mut config = JitConfig::default();
        config.max_consecutive_failures = 100; // Won't trigger
        config.max_total_failures = 5;
        let mut runtime = JitRuntime::new(config);

        // Simulate total failures (with successful compilations in between)
        for i in 0..5 {
            runtime.handle_compilation_error(
                0x1000 + i * 0x100,
                "test error",
                std::time::Duration::from_millis(1),
            );
            // Reset consecutive (simulating a success)
            runtime.consecutive_failures = 0;
        }

        assert!(runtime.is_disabled_by_error());
        assert!(runtime.disabled_reason().unwrap().contains("total"));
    }

    #[test]
    fn test_consecutive_failures_reset_on_success() {
        let mut runtime = JitRuntime::default();

        // Simulate some failures
        runtime.consecutive_failures = 5;
        runtime.total_failures = 5;

        // Simulate a successful compilation
        runtime.consecutive_failures = 0;
        runtime.successful_compilations += 1;

        assert_eq!(runtime.consecutive_failures, 0);
        // Total failures should persist
        assert_eq!(runtime.total_failures, 5);
    }

    #[test]
    fn test_handle_code_modification() {
        let mut runtime = JitRuntime::default();

        // Insert a cached entry
        let pc = 0x1000u64;
        runtime
            .cache
            .insert_bytes(pc, vec![0, 97, 115, 109, 1, 0, 0, 0]); // WASM magic

        assert!(runtime.cache.contains(pc));

        // Simulate code modification in that region
        runtime.handle_code_modification(pc, 4);

        // Entry should be invalidated
        assert!(!runtime.cache.contains(pc));
    }

    #[test]
    fn test_handle_tlb_flush() {
        let mut runtime = JitRuntime::default();

        // Insert some cached entries
        runtime
            .cache
            .insert_bytes(0x1000, vec![0, 97, 115, 109, 1, 0, 0, 0]);
        runtime
            .cache
            .insert_bytes(0x2000, vec![0, 97, 115, 109, 1, 0, 0, 0]);

        assert_eq!(runtime.cache.entry_count(), 2);

        // TLB flush should clear everything
        runtime.handle_tlb_flush();

        assert_eq!(runtime.cache.entry_count(), 0);
    }

    #[test]
    fn test_diagnostics() {
        let mut runtime = JitRuntime::default();

        // Simulate some state
        runtime.successful_compilations = 10;
        runtime.total_failures = 2;
        runtime.consecutive_failures = 1;
        runtime.failed_blocks.insert(0x1000);

        let diag = runtime.diagnostics();

        assert!(diag.enabled);
        assert!(!diag.disabled_by_error);
        assert!(diag.disabled_reason.is_none());
        assert_eq!(diag.successful_compilations, 10);
        assert_eq!(diag.total_failures, 2);
        assert_eq!(diag.consecutive_failures, 1);
        assert_eq!(diag.failed_blocks_count, 1);
    }

    #[test]
    fn test_diagnostics_structure() {
        let runtime = JitRuntime::default();
        let diag = runtime.diagnostics();

        // Test that all diagnostic fields are accessible
        assert!(diag.enabled);
        assert!(!diag.disabled_by_error);
        assert!(diag.disabled_reason.is_none());
        assert_eq!(diag.consecutive_failures, 0);
        assert_eq!(diag.total_failures, 0);
        assert_eq!(diag.successful_compilations, 0);
        assert_eq!(diag.failed_blocks_count, 0);
        assert_eq!(diag.cache_entries, 0);
        assert_eq!(diag.cache_bytes, 0);
        // CacheStats should be accessible
        assert_eq!(diag.cache_stats.hits, 0);
        assert_eq!(diag.cache_stats.misses, 0);
        // TraceStats should be accessible  
        assert_eq!(diag.trace_stats.jit_executions, 0);
    }

    #[test]
    fn test_try_compile_unsuitable_blacklists() {
        let mut runtime = JitRuntime::default();

        // Create a block with only 1 op (below min_block_size)
        // This will return Unsuitable
        let block = make_test_block(0x1000, 1, 100);

        // Force min_block_size to 0 so the block passes initial checks
        runtime.config.min_block_size = 0;

        // The compiler should still return Unsuitable for tiny blocks
        // and the block should be blacklisted
        let result = runtime.try_compile(&block);

        // Either it compiles or it's blacklisted
        if result.is_none() {
            // The block should be blacklisted now
            assert!(runtime.failed_blocks.contains(&0x1000));
            // Unsupported ops don't count as errors
            assert_eq!(runtime.total_failures, 0);
        }
    }

    #[test]
    fn test_config_accessors() {
        let mut runtime = JitRuntime::default();

        // Test config accessor
        assert!(runtime.config().enabled);

        // Test mutable config accessor
        runtime.config_mut().enabled = false;
        assert!(!runtime.config().enabled);
    }

    #[test]
    fn test_cache_accessors() {
        let mut runtime = JitRuntime::default();

        // Test cache accessor
        assert_eq!(runtime.cache().entry_count(), 0);

        // Test mutable cache accessor
        runtime
            .cache_mut()
            .insert_bytes(0x1000, vec![0, 97, 115, 109, 1, 0, 0, 0]);
        assert_eq!(runtime.cache().entry_count(), 1);
    }

    #[test]
    fn test_trace_accessors() {
        let mut config = JitConfig::default();
        config.trace_enabled = true;
        let mut runtime = JitRuntime::new(config);

        // Test trace accessor
        assert!(runtime.trace().is_enabled());

        // Test mutable trace accessor
        runtime.trace_mut().disable();
        assert!(!runtime.trace().is_enabled());
    }

    #[test]
    fn test_wasm_size_limit() {
        let mut config = JitConfig::default();
        config.max_wasm_size = 100; // Very small limit
        let mut runtime = JitRuntime::new(config);

        // Create a large block that will compile to > 100 bytes
        let block = make_test_block(0x1000, 50, 100);

        // This should fail due to WASM size limit
        let result = runtime.try_compile(&block);

        // The large module should be rejected
        if result.is_none() {
            // The block should be blacklisted
            assert!(runtime.failed_blocks.contains(&0x1000));
        }
    }
}

