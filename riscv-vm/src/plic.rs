use crate::dram::MemoryError;

pub const PLIC_BASE: u64 = 0x0C00_0000;
pub const PLIC_SIZE: u64 = 0x400_0000;

pub const UART_IRQ: u32 = 10;
pub const VIRTIO0_IRQ: u32 = 1;

const NUM_SOURCES: usize = 32;
const NUM_CONTEXTS: usize = 2; // 0 = M-mode hart0, 1 = S-mode hart0

pub struct Plic {
    pub priority: [u32; NUM_SOURCES],
    pub pending: u32, // Level-triggered mirror of device IRQ lines (bit per source)
    pub enable: [u32; NUM_CONTEXTS],
    pub threshold: [u32; NUM_CONTEXTS],
    pub active: [u32; NUM_CONTEXTS], // Per-context in-flight IRQs (claimed but not completed)
    pub debug: bool,
    // Multi-context arrays enable SMP readiness while preserving single-hart behavior.
}

impl Plic {
    pub fn new() -> Self {
        Self {
            priority: [0; NUM_SOURCES],
            pending: 0,
            enable: [0; NUM_CONTEXTS],
            threshold: [0; NUM_CONTEXTS],
            active: [0; NUM_CONTEXTS],
            debug: false,
        }
    }

    pub fn update_pending(&mut self, source: u32) {
        // Backward compatibility helper: set as pending (edge → level).
        // Bus.refresh_irqs() may later clear this if device line is low.
        if source < 32 {
            if self.debug {
                 eprintln!("[PLIC] Update Pending source={}", source);
            }
            self.pending |= 1 << source;
        }
    }

    // New: level-triggered source line setter
    pub fn set_source_level(&mut self, source: u32, level: bool) {
        if source >= 32 {
            return;
        }
        let was_pending = (self.pending & (1 << source)) != 0;
        if level {
            if self.debug && !was_pending {
                 eprintln!("[PLIC] IRQ Line High: source={} enable[0]=0x{:x} enable[1]=0x{:x} prio={}", 
                          source, self.enable[0], self.enable[1], self.priority[source as usize]);
            }
            self.pending |= 1 << source;
        } else {
            self.pending &= !(1 << source);
        }
    }

    pub fn load(&mut self, offset: u64, size: u64) -> Result<u64, MemoryError> {
        if size != 4 {
            return Ok(0); 
        }

        // Priority registers: 0x000000 .. 0x0000FC (4 bytes each)
        if offset < 0x001000 {
            let idx = (offset >> 2) as usize;
            if idx < NUM_SOURCES {
                return Ok(self.priority[idx] as u64);
            }
        }
        // Pending bits: 0x001000
        if offset == 0x001000 {
            return Ok(self.pending as u64);
        }
        // Enable per context: 0x002000 + 0x80 * context
        if offset >= 0x002000 && offset < 0x002000 + 0x80 * (NUM_CONTEXTS as u64) {
            let ctx = ((offset - 0x002000) / 0x80) as usize;
            let inner = (offset - 0x002000) % 0x80;
            if ctx < NUM_CONTEXTS && inner == 0 {
                return Ok(self.enable[ctx] as u64);
            }
        }
        // Context registers: threshold @ 0x200000 + 0x1000 * ctx, claim @ +4
        if offset >= 0x200000 {
            let ctx = ((offset - 0x200000) / 0x1000) as usize;
            if ctx < NUM_CONTEXTS {
                let base = 0x200000 + (0x1000 * ctx as u64);
                if offset == base {
                    return Ok(self.threshold[ctx] as u64);
                }
                if offset == base + 4 {
                    let claim = self.claim_interrupt_for(ctx);
                    if crate::plic::Plic::debug_trace() {
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

    pub fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        if size != 4 {
            return Ok(());
        }
        let val = value as u32;

        // Priority
        if offset < 0x001000 {
            let idx = (offset >> 2) as usize;
            if idx < NUM_SOURCES {
                self.priority[idx] = val;
            }
            return Ok(());
        }
        // Pending is read-only to software
        if offset == 0x001000 {
            return Ok(());
        }
        // Enable per context
        if offset >= 0x002000 && offset < 0x002000 + 0x80 * (NUM_CONTEXTS as u64) {
            let ctx = ((offset - 0x002000) / 0x80) as usize;
            let inner = (offset - 0x002000) % 0x80;
            if ctx < NUM_CONTEXTS && inner == 0 {
                self.enable[ctx] = val;
            }
            return Ok(());
        }
        // Threshold / Claim-Complete per context
        if offset >= 0x200000 {
            let ctx = ((offset - 0x200000) / 0x1000) as usize;
            if ctx < NUM_CONTEXTS {
                let base = 0x200000 + (0x1000 * ctx as u64);
                if offset == base {
                    self.threshold[ctx] = val;
                    return Ok(());
                }
                if offset == base + 4 {
                    // Completion: value is the source ID to complete
                    let id = (val & 0xffff) as u32;
                    if id > 0 && (id as usize) < NUM_SOURCES {
                        // eprintln!("[PLIC] Completed IRQ {} for context {}", id, ctx);
                        self.active[ctx] &= !(1 << id);
                    }
                    return Ok(());
                }
            }
            return Ok(());
        }

        Ok(())
    }

    fn eligible_for_context(&self, source: usize, ctx: usize) -> bool {
        let pending = ((self.pending >> source) & 1) == 1;
        let enabled = ((self.enable[ctx] >> source) & 1) == 1;
        let over_threshold = self.priority[source] > self.threshold[ctx];
        let not_active = ((self.active[ctx] >> source) & 1) == 0;
        pending && enabled && over_threshold && not_active
    }

    pub fn claim_interrupt_for(&mut self, ctx: usize) -> u32 {
        let mut max_prio = 0;
        let mut max_id = 0;

        for i in 1..NUM_SOURCES {
            if self.eligible_for_context(i, ctx) {
                let prio = self.priority[i];
                if prio > max_prio {
                    max_prio = prio;
                    max_id = i as u32;
                }
            }
        }

        if max_id != 0 {
            // eprintln!("[PLIC] Claimed IRQ {} for context {} (prio {})", max_id, ctx, max_prio);
            // Mark in-flight for this context until completed.
            self.active[ctx] |= 1 << max_id;
        }
        max_id
    }
    
    pub fn is_interrupt_pending(&self) -> bool {
        // For current single-hart flow, report S-mode context (1) if available, else context 0.
        let ctx = if NUM_CONTEXTS > 1 { 1 } else { 0 };
        self.is_interrupt_pending_for(ctx)
    }

    pub fn is_interrupt_pending_for(&self, ctx: usize) -> bool {
        if ctx >= NUM_CONTEXTS {
            return false;
        }
        for i in 1..NUM_SOURCES {
            if self.eligible_for_context(i, ctx) {
                if self.debug {
                    eprintln!("[PLIC] Interrupt pending for ctx={} source={}", ctx, i);
                }
                return true;
            }
        }
        // Debug: show why no interrupt
        if self.debug && self.pending != 0 {
            for i in 1..NUM_SOURCES {
                let pending = ((self.pending >> i) & 1) == 1;
                let enabled = ((self.enable[ctx] >> i) & 1) == 1;
                let over_threshold = self.priority[i] > self.threshold[ctx];
                let not_active = ((self.active[ctx] >> i) & 1) == 0;
                if pending {
                    eprintln!("[PLIC] Source {} pending but not eligible for ctx={}: enabled={} over_threshold={} (prio={} > thresh={}) not_active={}", 
                             i, ctx, enabled, over_threshold, self.priority[i], self.threshold[ctx], not_active);
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
    fn test_plic_claim_complete_context1() {
        let mut plic = Plic::new();
        // Priorities
        plic.priority[1] = 5;
        plic.priority[10] = 3;
        // Enable sources 1 and 10 for context 1 (S-mode)
        let enable_val = (1u32 << 1) | (1u32 << 10);
        let _ = plic.store(0x002000 + 0x80 * 1, 4, enable_val as u64);
        // Threshold 0 for context 1
        let _ = plic.store(0x200000 + 0x1000 * 1, 4, 0);
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
}
