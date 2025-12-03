//! Execution tracing for JIT debugging.

use std::collections::VecDeque;

/// Trace event types.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// Block execution started
    BlockEnter {
        pc: u64,
        is_jit: bool,
        exec_count: u32,
    },
    /// Block execution completed
    BlockExit {
        pc: u64,
        next_pc: u64,
        instructions: u32,
        cycles: u64,
    },
    /// JIT compilation triggered
    JitCompileStart {
        pc: u64,
        ops: u32,
    },
    /// JIT compilation completed
    JitCompileEnd {
        pc: u64,
        wasm_size: usize,
        time_us: u64,
        success: bool,
    },
    /// Memory access via JIT helper
    MemoryAccess {
        vaddr: u64,
        paddr: u64,
        size: u8,
        is_write: bool,
        value: u64,
    },
    /// Trap occurred during JIT execution
    Trap {
        pc: u64,
        cause: u64,
        tval: u64,
    },
    /// Block cache invalidation
    CacheInvalidate {
        pc: Option<u64>, // None = full flush
        reason: &'static str,
    },
    /// Cache hit/miss event
    CacheLookup {
        pc: u64,
        hit: bool,
    },
    /// Interrupt check triggered exit
    InterruptExit {
        pc: u64,
        instructions_executed: u32,
    },
}

/// Trace buffer with ring-buffer semantics.
pub struct TraceBuffer {
    events: VecDeque<TraceEvent>,
    capacity: usize,
    enabled: bool,
    /// Sequence number for ordering
    sequence: u64,
}

impl TraceBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
            enabled: false,
            sequence: 0,
        }
    }

    pub fn enable(&mut self) {
        self.enabled = true;
    }

    pub fn disable(&mut self) {
        self.enabled = false;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn push(&mut self, event: TraceEvent) {
        if !self.enabled {
            return;
        }
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
        self.sequence += 1;
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.sequence = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = &TraceEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get the current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Dump recent events to console.
    pub fn dump_recent(&self, count: usize) {
        let start = if self.events.len() > count {
            self.events.len() - count
        } else {
            0
        };

        #[cfg(target_arch = "wasm32")]
        web_sys::console::log_1(&format!("═══ Recent {} JIT Events ═══", count).into());
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("═══ Recent {} JIT Events ═══", count);

        for (i, event) in self.events.iter().skip(start).enumerate() {
            let msg = format_event(i, event);
            #[cfg(target_arch = "wasm32")]
            web_sys::console::log_1(&msg.into());
            #[cfg(not(target_arch = "wasm32"))]
            eprintln!("{}", msg);
        }
    }

    /// Get statistics from trace buffer.
    pub fn stats(&self) -> TraceStats {
        let mut stats = TraceStats::default();
        for event in &self.events {
            match event {
                TraceEvent::BlockEnter { is_jit: true, .. } => stats.jit_executions += 1,
                TraceEvent::BlockEnter { is_jit: false, .. } => stats.interp_executions += 1,
                TraceEvent::JitCompileEnd {
                    success: true,
                    wasm_size,
                    time_us,
                    ..
                } => {
                    stats.compilations += 1;
                    stats.total_wasm_bytes += wasm_size;
                    stats.total_compile_time_us += time_us;
                }
                TraceEvent::JitCompileEnd { success: false, .. } => {
                    stats.compilation_failures += 1;
                }
                TraceEvent::Trap { .. } => stats.traps += 1,
                TraceEvent::CacheInvalidate { .. } => stats.invalidations += 1,
                TraceEvent::CacheLookup { hit: true, .. } => stats.cache_hits += 1,
                TraceEvent::CacheLookup { hit: false, .. } => stats.cache_misses += 1,
                TraceEvent::InterruptExit { .. } => stats.interrupt_exits += 1,
                _ => {}
            }
        }
        stats
    }

    /// Find events matching a predicate.
    pub fn filter<F>(&self, predicate: F) -> Vec<&TraceEvent>
    where
        F: Fn(&TraceEvent) -> bool,
    {
        self.events.iter().filter(|e| predicate(e)).collect()
    }

    /// Find all events for a specific PC.
    pub fn events_for_pc(&self, pc: u64) -> Vec<&TraceEvent> {
        self.filter(|e| match e {
            TraceEvent::BlockEnter { pc: p, .. } => *p == pc,
            TraceEvent::BlockExit { pc: p, .. } => *p == pc,
            TraceEvent::JitCompileStart { pc: p, .. } => *p == pc,
            TraceEvent::JitCompileEnd { pc: p, .. } => *p == pc,
            TraceEvent::Trap { pc: p, .. } => *p == pc,
            _ => false,
        })
    }
}

/// Format a trace event for display.
fn format_event(index: usize, event: &TraceEvent) -> String {
    match event {
        TraceEvent::BlockEnter {
            pc,
            is_jit,
            exec_count,
        } => {
            let mode = if *is_jit { "JIT" } else { "INT" };
            format!(
                "[{:4}] ENTER {:016x} ({}) count={}",
                index, pc, mode, exec_count
            )
        }
        TraceEvent::BlockExit {
            pc,
            next_pc,
            instructions,
            cycles,
        } => {
            format!(
                "[{:4}] EXIT  {:016x} → {:016x} ({} insns, {} cycles)",
                index, pc, next_pc, instructions, cycles
            )
        }
        TraceEvent::JitCompileStart { pc, ops } => {
            format!("[{:4}] COMPILE_START {:016x} ({} ops)", index, pc, ops)
        }
        TraceEvent::JitCompileEnd {
            pc,
            wasm_size,
            time_us,
            success,
        } => {
            let status = if *success { "OK" } else { "FAIL" };
            format!(
                "[{:4}] COMPILE_END {:016x} {} ({} bytes, {}μs)",
                index, pc, status, wasm_size, time_us
            )
        }
        TraceEvent::MemoryAccess {
            vaddr,
            paddr,
            size,
            is_write,
            value,
        } => {
            let op = if *is_write { "WRITE" } else { "READ" };
            format!(
                "[{:4}] MEM {} {:016x}→{:016x} {}B val={:016x}",
                index, op, vaddr, paddr, size, value
            )
        }
        TraceEvent::Trap { pc, cause, tval } => {
            format!(
                "[{:4}] TRAP {:016x} cause={} tval={:016x}",
                index, pc, cause, tval
            )
        }
        TraceEvent::CacheInvalidate { pc, reason } => match pc {
            Some(addr) => format!("[{:4}] INVALIDATE {:016x} ({})", index, addr, reason),
            None => format!("[{:4}] INVALIDATE_ALL ({})", index, reason),
        },
        TraceEvent::CacheLookup { pc, hit } => {
            let status = if *hit { "HIT" } else { "MISS" };
            format!("[{:4}] CACHE {} {:016x}", index, status, pc)
        }
        TraceEvent::InterruptExit {
            pc,
            instructions_executed,
        } => {
            format!(
                "[{:4}] INT_EXIT {:016x} after {} insns",
                index, pc, instructions_executed
            )
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceStats {
    pub jit_executions: u64,
    pub interp_executions: u64,
    pub compilations: u64,
    pub compilation_failures: u64,
    pub total_wasm_bytes: usize,
    pub total_compile_time_us: u64,
    pub traps: u64,
    pub invalidations: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub interrupt_exits: u64,
}

impl TraceStats {
    /// Calculate JIT execution ratio.
    pub fn jit_ratio(&self) -> f64 {
        let total = self.jit_executions + self.interp_executions;
        if total == 0 {
            0.0
        } else {
            self.jit_executions as f64 / total as f64
        }
    }

    /// Calculate cache hit ratio.
    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f64 / total as f64
        }
    }

    /// Calculate average compilation time in microseconds.
    pub fn avg_compile_time_us(&self) -> f64 {
        if self.compilations == 0 {
            0.0
        } else {
            self.total_compile_time_us as f64 / self.compilations as f64
        }
    }

    /// Calculate average WASM module size.
    pub fn avg_wasm_size(&self) -> f64 {
        if self.compilations == 0 {
            0.0
        } else {
            self.total_wasm_bytes as f64 / self.compilations as f64
        }
    }

    /// Format stats as a string for display.
    pub fn format(&self) -> String {
        format!(
            "JIT Stats:\n\
             ├─ Executions: {} JIT / {} interp ({:.1}% JIT)\n\
             ├─ Compilations: {} success / {} failed\n\
             ├─ Avg compile time: {:.1}μs\n\
             ├─ Avg WASM size: {:.0} bytes\n\
             ├─ Cache: {} hits / {} misses ({:.1}% hit rate)\n\
             ├─ Traps: {}\n\
             ├─ Invalidations: {}\n\
             └─ Interrupt exits: {}",
            self.jit_executions,
            self.interp_executions,
            self.jit_ratio() * 100.0,
            self.compilations,
            self.compilation_failures,
            self.avg_compile_time_us(),
            self.avg_wasm_size(),
            self.cache_hits,
            self.cache_misses,
            self.cache_hit_ratio() * 100.0,
            self.traps,
            self.invalidations,
            self.interrupt_exits,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_buffer_capacity() {
        let mut buffer = TraceBuffer::new(3);
        buffer.enable();

        // Push 5 events into buffer of capacity 3
        for i in 0..5 {
            buffer.push(TraceEvent::BlockEnter {
                pc: i as u64 * 0x100,
                is_jit: false,
                exec_count: i,
            });
        }

        // Should only contain last 3 events
        assert_eq!(buffer.len(), 3);

        let pcs: Vec<u64> = buffer
            .iter()
            .filter_map(|e| match e {
                TraceEvent::BlockEnter { pc, .. } => Some(*pc),
                _ => None,
            })
            .collect();

        assert_eq!(pcs, vec![0x200, 0x300, 0x400]);
    }

    #[test]
    fn test_trace_buffer_disabled() {
        let mut buffer = TraceBuffer::new(10);
        // Buffer is disabled by default

        buffer.push(TraceEvent::BlockEnter {
            pc: 0x1000,
            is_jit: true,
            exec_count: 1,
        });

        assert!(buffer.is_empty());
        assert_eq!(buffer.sequence(), 0);
    }

    #[test]
    fn test_trace_buffer_enable_disable() {
        let mut buffer = TraceBuffer::new(10);

        buffer.enable();
        assert!(buffer.is_enabled());

        buffer.push(TraceEvent::BlockEnter {
            pc: 0x1000,
            is_jit: true,
            exec_count: 1,
        });
        assert_eq!(buffer.len(), 1);

        buffer.disable();
        assert!(!buffer.is_enabled());

        buffer.push(TraceEvent::BlockEnter {
            pc: 0x2000,
            is_jit: true,
            exec_count: 2,
        });
        // Should still be 1 since we disabled
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_trace_stats_calculation() {
        let mut buffer = TraceBuffer::new(100);
        buffer.enable();

        // Add some JIT executions
        for _ in 0..10 {
            buffer.push(TraceEvent::BlockEnter {
                pc: 0x1000,
                is_jit: true,
                exec_count: 1,
            });
        }

        // Add some interpreted executions
        for _ in 0..5 {
            buffer.push(TraceEvent::BlockEnter {
                pc: 0x2000,
                is_jit: false,
                exec_count: 1,
            });
        }

        // Add compilations
        buffer.push(TraceEvent::JitCompileEnd {
            pc: 0x1000,
            wasm_size: 1000,
            time_us: 500,
            success: true,
        });
        buffer.push(TraceEvent::JitCompileEnd {
            pc: 0x2000,
            wasm_size: 2000,
            time_us: 700,
            success: true,
        });
        buffer.push(TraceEvent::JitCompileEnd {
            pc: 0x3000,
            wasm_size: 0,
            time_us: 100,
            success: false,
        });

        // Add cache events
        for _ in 0..8 {
            buffer.push(TraceEvent::CacheLookup { pc: 0x1000, hit: true });
        }
        for _ in 0..2 {
            buffer.push(TraceEvent::CacheLookup {
                pc: 0x2000,
                hit: false,
            });
        }

        let stats = buffer.stats();

        assert_eq!(stats.jit_executions, 10);
        assert_eq!(stats.interp_executions, 5);
        assert_eq!(stats.compilations, 2);
        assert_eq!(stats.compilation_failures, 1);
        assert_eq!(stats.total_wasm_bytes, 3000);
        assert_eq!(stats.total_compile_time_us, 1200);
        assert_eq!(stats.cache_hits, 8);
        assert_eq!(stats.cache_misses, 2);

        // Test ratios
        let jit_ratio = stats.jit_ratio();
        assert!((jit_ratio - 0.666666).abs() < 0.001);

        let cache_ratio = stats.cache_hit_ratio();
        assert!((cache_ratio - 0.8).abs() < 0.001);

        let avg_compile = stats.avg_compile_time_us();
        assert!((avg_compile - 600.0).abs() < 0.001);

        let avg_size = stats.avg_wasm_size();
        assert!((avg_size - 1500.0).abs() < 0.001);
    }

    #[test]
    fn test_events_for_pc() {
        let mut buffer = TraceBuffer::new(100);
        buffer.enable();

        let target_pc = 0x80001000u64;

        buffer.push(TraceEvent::BlockEnter {
            pc: target_pc,
            is_jit: true,
            exec_count: 1,
        });
        buffer.push(TraceEvent::BlockEnter {
            pc: 0x80002000,
            is_jit: false,
            exec_count: 1,
        });
        buffer.push(TraceEvent::JitCompileStart {
            pc: target_pc,
            ops: 10,
        });
        buffer.push(TraceEvent::Trap {
            pc: target_pc,
            cause: 2,
            tval: 0,
        });
        buffer.push(TraceEvent::CacheLookup {
            pc: 0x80003000,
            hit: true,
        });

        let events = buffer.events_for_pc(target_pc);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn test_filter() {
        let mut buffer = TraceBuffer::new(100);
        buffer.enable();

        buffer.push(TraceEvent::Trap {
            pc: 0x1000,
            cause: 2,
            tval: 0,
        });
        buffer.push(TraceEvent::BlockEnter {
            pc: 0x2000,
            is_jit: true,
            exec_count: 1,
        });
        buffer.push(TraceEvent::Trap {
            pc: 0x3000,
            cause: 5,
            tval: 0x100,
        });

        let traps = buffer.filter(|e| matches!(e, TraceEvent::Trap { .. }));
        assert_eq!(traps.len(), 2);
    }

    #[test]
    fn test_clear() {
        let mut buffer = TraceBuffer::new(10);
        buffer.enable();

        for i in 0..5 {
            buffer.push(TraceEvent::BlockEnter {
                pc: i * 0x100,
                is_jit: false,
                exec_count: i as u32,
            });
        }

        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.sequence(), 5);

        buffer.clear();

        assert!(buffer.is_empty());
        assert_eq!(buffer.sequence(), 0);
    }

    #[test]
    fn test_stats_format() {
        let stats = TraceStats {
            jit_executions: 1000,
            interp_executions: 500,
            compilations: 50,
            compilation_failures: 2,
            total_wasm_bytes: 250000,
            total_compile_time_us: 25000,
            traps: 3,
            invalidations: 10,
            cache_hits: 950,
            cache_misses: 50,
            interrupt_exits: 100,
        };

        let formatted = stats.format();
        assert!(formatted.contains("JIT Stats:"));
        assert!(formatted.contains("1000 JIT"));
        assert!(formatted.contains("500 interp"));
        assert!(formatted.contains("66.7% JIT"));
    }

    #[test]
    fn test_trace_stats_default() {
        let stats = TraceStats::default();

        assert_eq!(stats.jit_executions, 0);
        assert_eq!(stats.interp_executions, 0);
        assert_eq!(stats.jit_ratio(), 0.0);
        assert_eq!(stats.cache_hit_ratio(), 0.0);
        assert_eq!(stats.avg_compile_time_us(), 0.0);
        assert_eq!(stats.avg_wasm_size(), 0.0);
    }
}

