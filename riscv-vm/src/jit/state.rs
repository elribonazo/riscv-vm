//! JIT CPU State Layout
//!
//! Defines the memory layout for CPU state accessed by JIT'd code.
//! This must match exactly between Rust and generated WASM.
//!
//! ## JIT State Region Layout (within SharedArrayBuffer)
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │ Offset    │ Size      │ Region                                      │
//! ├──────────────────────────────────────────────────────────────────────┤
//! │ 0x0000    │ 0x0100    │ CPU Registers (32 × 8 bytes)                │
//! │ 0x0100    │ 0x0010    │ PC + Mode                                   │
//! │ 0x0200    │ 0x0080    │ Trap signaling (pending, code, value)       │
//! │ 0x0300    │ 0x0080    │ Helper results + JIT flags                  │
//! │ 0x0400    │ 0x0600    │ TLB Entries (64 entries × 24 bytes)         │
//! │ 0x0A00    │ 0x0100    │ CSR Mirror (SATP, MSTATUS, etc.)            │
//! │ 0x0B00    │ ...       │ Reserved                                    │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The JIT state region is at offset 0x13000 in the SharedArrayBuffer
//! (see `shared_mem.rs`). DRAM follows at 0x14000.

/// Offset of the JIT CPU state region in SharedArrayBuffer.
pub const JIT_STATE_OFFSET: usize = 0x13000;

/// Size of the JIT CPU state region.
pub const JIT_STATE_SIZE: usize = 4096;

// ═══════════════════════════════════════════════════════════════════════════
// TLB Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// TLB size (must be power of 2 for efficient indexing).
pub const TLB_SIZE: usize = 64;

/// TLB mask for index computation (TLB_SIZE - 1).
pub const TLB_MASK: u64 = (TLB_SIZE - 1) as u64;

/// Size of each TLB entry in bytes.
/// Layout: vpn(8) | ppn(8) | asid(2) | perm(1) | level(1) | valid(1) | pad(3) = 24 bytes
pub const TLB_ENTRY_SIZE: usize = 24;

/// Total size of TLB region in bytes.
pub const TLB_REGION_SIZE: usize = TLB_SIZE * TLB_ENTRY_SIZE; // 0x600 (1536 bytes)

/// Offset of TLB region within the JIT state area (relative to JIT_STATE_OFFSET).
/// Placed after helper results at 0x0400.
pub const TLB_REGION_OFFSET: usize = 0x0400;

// ═══════════════════════════════════════════════════════════════════════════
// CSR Mirror Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Offset of CSR mirror region within JIT state area (relative to JIT_STATE_OFFSET).
/// Placed after TLB entries.
pub const CSR_REGION_OFFSET: usize = 0x0A00;

/// Size of CSR mirror region.
pub const CSR_REGION_SIZE: usize = 0x0100;

/// RISC-V DRAM base address (physical memory starts here).
pub const DRAM_BASE: u64 = 0x8000_0000;

/// CPU state layout for JIT access.
/// All offsets are relative to the JIT_STATE_OFFSET.
pub mod offsets {
    /// Base offset of register file (regs[0..32])
    pub const REGS_BASE: u32 = 0x0000;
    /// Size of each register (8 bytes for RV64)
    pub const REG_SIZE: u32 = 8;
    /// Total size of register file
    pub const REGS_SIZE: u32 = 32 * REG_SIZE;

    /// Program counter
    pub const PC: u32 = 0x0100;
    /// Privilege mode (0=User, 1=Supervisor, 3=Machine)
    pub const MODE: u32 = 0x0108;

    /// Trap signaling region
    /// Trap pending flag (u32): Non-zero if a trap occurred during JIT execution
    pub const TRAP_PENDING: u32 = 0x0200;
    /// Trap code (u32): RISC-V exception/interrupt cause
    pub const TRAP_CODE: u32 = 0x0204;
    /// Trap value (u64): Associated value (faulting address, instruction bits, etc.)
    pub const TRAP_VALUE: u32 = 0x0208;

    /// Helper function results
    pub const HELPER_RESULT: u32 = 0x0300;
    pub const HELPER_RESULT_OK: u32 = 0x0304;

    /// Interrupt pending flag (set by host, checked by JIT'd code)
    /// Layout: u32 (non-zero = interrupt pending)
    pub const INTERRUPT_PENDING: u32 = 0x0310;

    /// Instructions executed in current JIT invocation (u64)
    pub const INSN_COUNT: u32 = 0x0318;

    /// JIT block entry PC (u64) - the PC at which the block started
    pub const ENTRY_PC: u32 = 0x0320;

    /// Exit code from last JIT block (u64)
    /// High 32 bits: reason (0=normal, 1=trap, 2=interpreter, 3=interrupt)
    /// Low 32 bits: additional data
    pub const EXIT_CODE: u32 = 0x0328;

    /// Calculate register offset
    #[inline]
    pub const fn reg(idx: u8) -> u32 {
        REGS_BASE + (idx as u32) * REG_SIZE
    }
}

/// CSR mirror offsets (relative to CSR_REGION_OFFSET within JIT state).
/// These are copies of CSRs that the JIT needs to check for TLB validation
/// and permission checks.
pub mod csr_mirror {
    /// SATP value for TLB validation (u64)
    /// Contains MODE (bits 63:60), ASID (bits 59:44), PPN (bits 43:0)
    pub const SATP: u32 = 0x00;

    /// MSTATUS for permission checks (u64)
    /// Relevant bits: MXR (bit 19), SUM (bit 18), MPRV (bit 17)
    pub const MSTATUS: u32 = 0x08;

    /// Current privilege level (u64)
    /// Values: 0=User, 1=Supervisor, 3=Machine
    pub const PRIVILEGE: u32 = 0x10;

    /// Current ASID extracted from SATP (u64)
    /// Cached for fast TLB lookup validation
    pub const CURRENT_ASID: u32 = 0x18;

    /// MEDELEG for exception delegation checking (u64)
    pub const MEDELEG: u32 = 0x20;

    /// MIDELEG for interrupt delegation (u64)
    pub const MIDELEG: u32 = 0x28;

    /// MIE - Machine Interrupt Enable (u64)
    pub const MIE: u32 = 0x30;

    /// MIP - Machine Interrupt Pending (u64)
    pub const MIP: u32 = 0x38;
}

/// TLB entry field offsets (relative to entry start).
///
/// Layout:
/// ```text
/// Offset  Size  Field       Description
/// ──────  ────  ─────       ───────────
/// 0       8     vpn         Virtual Page Number
/// 8       8     ppn         Physical Page Number
/// 16      2     asid        Address Space ID
/// 18      1     perm        Permission bits (R/W/X/U/A/D/G)
/// 19      1     level       Page table level (0-2)
/// 20      1     valid       Entry valid flag
/// 21      3     padding     Alignment padding
/// ──────  ────
/// Total:  24 bytes per entry
/// ```
pub mod tlb_entry {
    /// VPN field offset (8 bytes)
    pub const VPN: u32 = 0;
    /// PPN field offset (8 bytes)
    pub const PPN: u32 = 8;
    /// ASID field offset (2 bytes)
    pub const ASID: u32 = 16;
    /// Permission bits offset (1 byte): R=bit0, W=bit1, X=bit2, U=bit3, A=bit4, D=bit5, G=bit6
    pub const PERM: u32 = 18;
    /// Page table level offset (1 byte)
    pub const LEVEL: u32 = 19;
    /// Valid flag offset (1 byte)
    pub const VALID: u32 = 20;

    // Permission bit masks (matching mmu.rs constants)
    /// Read permission bit
    pub const PERM_READ: u8 = 1 << 0;
    /// Write permission bit
    pub const PERM_WRITE: u8 = 1 << 1;
    /// Execute permission bit
    pub const PERM_EXEC: u8 = 1 << 2;
    /// User-mode accessible bit
    pub const PERM_USER: u8 = 1 << 3;
    /// Accessed bit
    pub const PERM_ACCESSED: u8 = 1 << 4;
    /// Dirty bit
    pub const PERM_DIRTY: u8 = 1 << 5;
    /// Global mapping bit (ignores ASID)
    pub const PERM_GLOBAL: u8 = 1 << 6;
}

/// Trap codes for JIT signaling.
pub mod trap_codes {
    pub const NONE: u32 = 0;
    pub const ILLEGAL_INSTRUCTION: u32 = 2;
    pub const EBREAK: u32 = 3;
    pub const LOAD_ACCESS_FAULT: u32 = 5;
    pub const STORE_ACCESS_FAULT: u32 = 7;
    pub const ECALL: u32 = 8; // Base, actual depends on mode
    pub const LOAD_PAGE_FAULT: u32 = 13;
    pub const STORE_PAGE_FAULT: u32 = 15;
}

/// Sync CPU state from Rust `Cpu` struct to shared memory.
///
/// Call this before executing a JIT'd block.
#[cfg(target_arch = "wasm32")]
pub fn sync_to_shared(cpu: &crate::cpu::Cpu, shared_buffer: &js_sys::SharedArrayBuffer) {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let base = JIT_STATE_OFFSET as u32;

    // Copy registers
    for (i, &reg) in cpu.regs.iter().enumerate() {
        let offset = base + offsets::reg(i as u8);
        let bytes = reg.to_le_bytes();
        for (j, &b) in bytes.iter().enumerate() {
            view.set_index(offset + j as u32, b);
        }
    }

    // Copy PC
    let pc_bytes = cpu.pc.to_le_bytes();
    for (j, &b) in pc_bytes.iter().enumerate() {
        view.set_index(base + offsets::PC + j as u32, b);
    }

    // Copy mode
    use crate::cpu::types::Mode;
    let mode_val = match cpu.mode {
        Mode::User => 0u8,
        Mode::Supervisor => 1u8,
        Mode::Machine => 3u8,
    };
    view.set_index(base + offsets::MODE, mode_val);

    // Clear trap pending
    view.set_index(base + offsets::TRAP_PENDING, 0);
}

/// Sync CPU state from shared memory back to Rust `Cpu` struct.
///
/// Call this after executing a JIT'd block.
/// Returns `Some((trap_code, trap_value))` if a trap occurred.
#[cfg(target_arch = "wasm32")]
pub fn sync_from_shared(
    cpu: &mut crate::cpu::Cpu,
    shared_buffer: &js_sys::SharedArrayBuffer,
) -> Option<(u32, u64)> {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let base = JIT_STATE_OFFSET as u32;

    // Read registers
    for i in 0..32 {
        let offset = base + offsets::reg(i as u8);
        let mut bytes = [0u8; 8];
        for j in 0..8 {
            bytes[j] = view.get_index(offset + j as u32);
        }
        cpu.regs[i] = u64::from_le_bytes(bytes);
    }
    cpu.regs[0] = 0; // x0 is always 0

    // Read PC
    let mut pc_bytes = [0u8; 8];
    for j in 0..8 {
        pc_bytes[j] = view.get_index(base + offsets::PC + j as u32);
    }
    cpu.pc = u64::from_le_bytes(pc_bytes);

    // Check for trap
    let trap_pending = view.get_index(base + offsets::TRAP_PENDING);
    if trap_pending != 0 {
        let mut code_bytes = [0u8; 4];
        let mut val_bytes = [0u8; 8];
        for j in 0..4 {
            code_bytes[j] = view.get_index(base + offsets::TRAP_CODE + j as u32);
        }
        for j in 0..8 {
            val_bytes[j] = view.get_index(base + offsets::TRAP_VALUE + j as u32);
        }
        let trap_code = u32::from_le_bytes(code_bytes);
        let trap_value = u64::from_le_bytes(val_bytes);
        return Some((trap_code, trap_value));
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════════
// TLB Sync Functions (WASM target)
// ═══════════════════════════════════════════════════════════════════════════

/// Sync TLB entries from CPU's MMU to shared memory.
///
/// Call this before executing a JIT'd block to ensure the TLB cache
/// in shared memory is up-to-date for fast-path memory access.
#[cfg(target_arch = "wasm32")]
pub fn sync_tlb_to_shared(cpu: &crate::cpu::Cpu, shared_buffer: &js_sys::SharedArrayBuffer) {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let tlb_base = (JIT_STATE_OFFSET + TLB_REGION_OFFSET) as u32;

    // Access TLB entries through the public interface
    for i in 0..TLB_SIZE {
        let offset = tlb_base + (i as u32) * (TLB_ENTRY_SIZE as u32);

        // Get entry from CPU's TLB via index
        let entry = cpu.tlb.get_entry(i);

        // Write VPN (8 bytes)
        for (j, &b) in entry.vpn.to_le_bytes().iter().enumerate() {
            view.set_index(offset + tlb_entry::VPN + j as u32, b);
        }

        // Write PPN (8 bytes)
        for (j, &b) in entry.ppn.to_le_bytes().iter().enumerate() {
            view.set_index(offset + tlb_entry::PPN + j as u32, b);
        }

        // Write ASID (2 bytes)
        for (j, &b) in entry.asid.to_le_bytes().iter().enumerate() {
            view.set_index(offset + tlb_entry::ASID + j as u32, b);
        }

        // Write perm (1 byte)
        view.set_index(offset + tlb_entry::PERM, entry.perm);

        // Write level (1 byte)
        view.set_index(offset + tlb_entry::LEVEL, entry.level);

        // Write valid (1 byte)
        view.set_index(offset + tlb_entry::VALID, entry.valid as u8);
    }
}

/// Sync a single TLB entry to shared memory.
///
/// More efficient than full sync when only one entry changed.
#[cfg(target_arch = "wasm32")]
pub fn sync_tlb_entry_to_shared(
    entry: &crate::mmu::TlbEntry,
    index: usize,
    shared_buffer: &js_sys::SharedArrayBuffer,
) {
    use js_sys::Uint8Array;

    if index >= TLB_SIZE {
        return;
    }

    let view = Uint8Array::new(shared_buffer);
    let offset =
        (JIT_STATE_OFFSET + TLB_REGION_OFFSET) as u32 + (index as u32) * (TLB_ENTRY_SIZE as u32);

    // Write VPN (8 bytes)
    for (j, &b) in entry.vpn.to_le_bytes().iter().enumerate() {
        view.set_index(offset + tlb_entry::VPN + j as u32, b);
    }

    // Write PPN (8 bytes)
    for (j, &b) in entry.ppn.to_le_bytes().iter().enumerate() {
        view.set_index(offset + tlb_entry::PPN + j as u32, b);
    }

    // Write ASID (2 bytes)
    for (j, &b) in entry.asid.to_le_bytes().iter().enumerate() {
        view.set_index(offset + tlb_entry::ASID + j as u32, b);
    }

    // Write perm, level, valid
    view.set_index(offset + tlb_entry::PERM, entry.perm);
    view.set_index(offset + tlb_entry::LEVEL, entry.level);
    view.set_index(offset + tlb_entry::VALID, entry.valid as u8);
}

// ═══════════════════════════════════════════════════════════════════════════
// CSR Sync Functions (WASM target)
// ═══════════════════════════════════════════════════════════════════════════

/// Sync CSR values to shared memory for JIT permission checks.
///
/// Call this before executing a JIT'd block and after any CSR write
/// that affects memory permissions (SATP, MSTATUS).
#[cfg(target_arch = "wasm32")]
pub fn sync_csrs_to_shared(cpu: &crate::cpu::Cpu, shared_buffer: &js_sys::SharedArrayBuffer) {
    use crate::cpu::csr::{CSR_MEDELEG, CSR_MIDELEG, CSR_MIE, CSR_MIP, CSR_MSTATUS, CSR_SATP};
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let csr_base = (JIT_STATE_OFFSET + CSR_REGION_OFFSET) as u32;

    // Helper to write u64 to view
    let write_u64 = |view: &Uint8Array, offset: u32, val: u64| {
        for (j, &b) in val.to_le_bytes().iter().enumerate() {
            view.set_index(offset + j as u32, b);
        }
    };

    // Read CSRs from CPU
    let satp = cpu.csrs[CSR_SATP as usize];
    let mstatus = cpu.csrs[CSR_MSTATUS as usize];
    let medeleg = cpu.csrs[CSR_MEDELEG as usize];
    let mideleg = cpu.csrs[CSR_MIDELEG as usize];
    let mie = cpu.csrs[CSR_MIE as usize];
    let mip = cpu.csrs[CSR_MIP as usize];

    // Extract ASID from SATP (bits 59:44)
    let asid = (satp >> 44) & 0xFFFF;

    // Get privilege level
    use crate::cpu::types::Mode;
    let privilege: u64 = match cpu.mode {
        Mode::User => 0,
        Mode::Supervisor => 1,
        Mode::Machine => 3,
    };

    // Write to CSR mirror region
    write_u64(&view, csr_base + csr_mirror::SATP, satp);
    write_u64(&view, csr_base + csr_mirror::MSTATUS, mstatus);
    write_u64(&view, csr_base + csr_mirror::PRIVILEGE, privilege);
    write_u64(&view, csr_base + csr_mirror::CURRENT_ASID, asid);
    write_u64(&view, csr_base + csr_mirror::MEDELEG, medeleg);
    write_u64(&view, csr_base + csr_mirror::MIDELEG, mideleg);
    write_u64(&view, csr_base + csr_mirror::MIE, mie);
    write_u64(&view, csr_base + csr_mirror::MIP, mip);
}

// ═══════════════════════════════════════════════════════════════════════════
// Interrupt Pending Functions (WASM target)
// ═══════════════════════════════════════════════════════════════════════════

/// Set the interrupt pending flag in shared memory.
///
/// Call this from the main thread when an interrupt needs handling.
/// The JIT'd code will check this flag and exit if set.
#[cfg(target_arch = "wasm32")]
pub fn set_interrupt_pending(shared_buffer: &js_sys::SharedArrayBuffer, pending: bool) {
    use js_sys::{Atomics, Int32Array};

    let view = Int32Array::new(shared_buffer);
    let offset = (JIT_STATE_OFFSET + offsets::INTERRUPT_PENDING as usize) / 4;
    let value = if pending { 1 } else { 0 };
    let _ = Atomics::store(&view, offset as u32, value);
}

/// Check if interrupt is pending in shared memory.
#[cfg(target_arch = "wasm32")]
pub fn interrupt_pending(shared_buffer: &js_sys::SharedArrayBuffer) -> bool {
    use js_sys::{Atomics, Int32Array};

    let view = Int32Array::new(shared_buffer);
    let offset = (JIT_STATE_OFFSET + offsets::INTERRUPT_PENDING as usize) / 4;
    Atomics::load(&view, offset as u32).unwrap_or(0) != 0
}

// ═══════════════════════════════════════════════════════════════════════════
// TLB Invalidation Functions (WASM target)
// ═══════════════════════════════════════════════════════════════════════════

/// Invalidate all TLB entries in shared memory.
///
/// Call this on SFENCE.VMA with rs1=x0, rs2=x0 (flush all).
#[cfg(target_arch = "wasm32")]
pub fn invalidate_tlb(shared_buffer: &js_sys::SharedArrayBuffer) {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let tlb_base = (JIT_STATE_OFFSET + TLB_REGION_OFFSET) as u32;

    for i in 0..TLB_SIZE {
        let offset = tlb_base + (i as u32) * (TLB_ENTRY_SIZE as u32) + tlb_entry::VALID;
        view.set_index(offset, 0);
    }
}

/// Invalidate TLB entry for specific VPN in shared memory.
///
/// Call this on SFENCE.VMA with rs1!=x0 (flush specific VA).
#[cfg(target_arch = "wasm32")]
pub fn invalidate_tlb_vpn(shared_buffer: &js_sys::SharedArrayBuffer, vpn: u64) {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let index = (vpn as usize) & (TLB_SIZE - 1);
    let tlb_base = (JIT_STATE_OFFSET + TLB_REGION_OFFSET) as u32;
    let offset = tlb_base + (index as u32) * (TLB_ENTRY_SIZE as u32);

    // Read stored VPN to check if it matches
    let mut vpn_bytes = [0u8; 8];
    for j in 0..8 {
        vpn_bytes[j] = view.get_index(offset + tlb_entry::VPN + j as u32);
    }
    let stored_vpn = u64::from_le_bytes(vpn_bytes);

    // Only invalidate if VPN matches
    if stored_vpn == vpn {
        view.set_index(offset + tlb_entry::VALID, 0);
    }
}

/// Invalidate TLB entries by ASID in shared memory.
///
/// Call this on SFENCE.VMA with rs2!=x0 (flush by ASID).
/// Global mappings are not flushed.
#[cfg(target_arch = "wasm32")]
pub fn invalidate_tlb_asid(shared_buffer: &js_sys::SharedArrayBuffer, asid: u64) {
    use js_sys::Uint8Array;

    let view = Uint8Array::new(shared_buffer);
    let tlb_base = (JIT_STATE_OFFSET + TLB_REGION_OFFSET) as u32;
    let asid16 = asid as u16;

    for i in 0..TLB_SIZE {
        let offset = tlb_base + (i as u32) * (TLB_ENTRY_SIZE as u32);

        // Read valid flag
        let valid = view.get_index(offset + tlb_entry::VALID);
        if valid == 0 {
            continue;
        }

        // Read perm to check global bit
        let perm = view.get_index(offset + tlb_entry::PERM);
        if (perm & tlb_entry::PERM_GLOBAL) != 0 {
            // Global entries are not flushed by ASID
            continue;
        }

        // Read ASID
        let mut asid_bytes = [0u8; 2];
        for j in 0..2 {
            asid_bytes[j] = view.get_index(offset + tlb_entry::ASID + j as u32);
        }
        let entry_asid = u16::from_le_bytes(asid_bytes);

        // Invalidate if ASID matches
        if entry_asid == asid16 {
            view.set_index(offset + tlb_entry::VALID, 0);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Combined Sync Function
// ═══════════════════════════════════════════════════════════════════════════

/// Full sync of CPU state, TLB, and CSRs to shared memory.
///
/// Call this before executing a JIT'd block for complete state sync.
#[cfg(target_arch = "wasm32")]
pub fn sync_all_to_shared(cpu: &crate::cpu::Cpu, shared_buffer: &js_sys::SharedArrayBuffer) {
    sync_to_shared(cpu, shared_buffer);
    sync_csrs_to_shared(cpu, shared_buffer);
    sync_tlb_to_shared(cpu, shared_buffer);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_offsets() {
        // Verify register offsets are correctly calculated
        assert_eq!(offsets::reg(0), 0x0000);
        assert_eq!(offsets::reg(1), 0x0008);
        assert_eq!(offsets::reg(31), 0x00F8);
    }

    #[test]
    fn test_trap_codes_match_riscv_spec() {
        // RISC-V exception codes from privileged spec
        assert_eq!(trap_codes::ILLEGAL_INSTRUCTION, 2);
        assert_eq!(trap_codes::EBREAK, 3);
        assert_eq!(trap_codes::LOAD_ACCESS_FAULT, 5);
        assert_eq!(trap_codes::STORE_ACCESS_FAULT, 7);
        assert_eq!(trap_codes::LOAD_PAGE_FAULT, 13);
        assert_eq!(trap_codes::STORE_PAGE_FAULT, 15);
    }

    #[test]
    fn test_state_region_size() {
        // Verify the state region doesn't exceed allocated size
        let max_used = offsets::EXIT_CODE + 8; // Last field + its size
        assert!(max_used as usize <= JIT_STATE_SIZE);
    }

    #[test]
    fn test_tlb_region_fits() {
        // Verify TLB region fits within JIT state
        let tlb_end = TLB_REGION_OFFSET + TLB_REGION_SIZE;
        assert!(
            tlb_end <= JIT_STATE_SIZE,
            "TLB region ({} bytes) exceeds JIT state size ({} bytes)",
            tlb_end,
            JIT_STATE_SIZE
        );
    }

    #[test]
    fn test_tlb_mask() {
        // Verify TLB_MASK is correct for TLB_SIZE
        assert_eq!(TLB_MASK, (TLB_SIZE - 1) as u64);
        // TLB_SIZE should be power of 2
        assert!(TLB_SIZE.is_power_of_two());
    }

    #[test]
    fn test_csr_region_fits() {
        // Verify CSR region fits within JIT state
        let csr_end = CSR_REGION_OFFSET + CSR_REGION_SIZE;
        assert!(
            csr_end <= JIT_STATE_SIZE,
            "CSR region ({} bytes @ 0x{:X}) exceeds JIT state size ({} bytes)",
            CSR_REGION_SIZE,
            CSR_REGION_OFFSET,
            JIT_STATE_SIZE
        );
    }

    #[test]
    fn test_regions_non_overlapping() {
        // Verify memory regions don't overlap
        // Registers: 0x0000 - 0x0100
        // PC/Mode: 0x0100 - 0x0110
        // Trap: 0x0200 - 0x0280
        // Helper: 0x0300 - 0x0400
        // TLB: 0x0400 - 0x0A00
        // CSR: 0x0A00 - 0x0B00

        let tlb_end = TLB_REGION_OFFSET + TLB_REGION_SIZE;
        assert!(
            tlb_end <= CSR_REGION_OFFSET,
            "TLB region (ends at 0x{:X}) overlaps CSR region (starts at 0x{:X})",
            tlb_end,
            CSR_REGION_OFFSET
        );

        // Verify TLB starts after helper results
        assert!(
            0x0400 <= TLB_REGION_OFFSET,
            "TLB region (0x{:X}) overlaps helper results region",
            TLB_REGION_OFFSET
        );
    }

    #[test]
    fn test_tlb_entry_layout() {
        // Verify TLB entry size matches field layout
        let expected_size = 24; // vpn(8) + ppn(8) + asid(2) + perm(1) + level(1) + valid(1) + pad(3)
        assert_eq!(TLB_ENTRY_SIZE, expected_size);

        // Verify field offsets don't overlap
        assert!(tlb_entry::VPN < tlb_entry::PPN);
        assert!(tlb_entry::PPN < tlb_entry::ASID);
        assert!(tlb_entry::ASID < tlb_entry::PERM);
        assert!(tlb_entry::PERM < tlb_entry::LEVEL);
        assert!(tlb_entry::LEVEL < tlb_entry::VALID);
        assert!(tlb_entry::VALID < TLB_ENTRY_SIZE as u32);
    }

    #[test]
    fn test_csr_mirror_offsets() {
        // Verify CSR mirror offsets are sequential and don't overlap
        assert!(csr_mirror::SATP < csr_mirror::MSTATUS);
        assert!(csr_mirror::MSTATUS < csr_mirror::PRIVILEGE);
        assert!(csr_mirror::PRIVILEGE < csr_mirror::CURRENT_ASID);
        assert!(csr_mirror::CURRENT_ASID < csr_mirror::MEDELEG);
        assert!(csr_mirror::MEDELEG < csr_mirror::MIDELEG);
        assert!(csr_mirror::MIDELEG < csr_mirror::MIE);
        assert!(csr_mirror::MIE < csr_mirror::MIP);

        // Verify all CSRs fit in the region
        let last_csr_end = csr_mirror::MIP as usize + 8;
        assert!(
            last_csr_end <= CSR_REGION_SIZE,
            "CSR mirror (ends at 0x{:X}) exceeds region size (0x{:X})",
            last_csr_end,
            CSR_REGION_SIZE
        );
    }

    #[test]
    fn test_interrupt_pending_offset() {
        // Verify interrupt pending is in the helper/JIT flags region
        assert!(offsets::INTERRUPT_PENDING >= 0x0300);
        assert!(offsets::INTERRUPT_PENDING < TLB_REGION_OFFSET as u32);
    }

    #[test]
    fn test_jit_state_offsets() {
        // Verify new JIT state offsets are in correct region
        assert!(offsets::INSN_COUNT >= 0x0310);
        assert!(offsets::ENTRY_PC >= 0x0318);
        assert!(offsets::EXIT_CODE >= 0x0328);

        // All should be before TLB region
        assert!((offsets::EXIT_CODE as usize + 8) <= TLB_REGION_OFFSET);
    }

    #[test]
    fn test_tlb_permission_bits() {
        // Verify permission bits don't overlap
        assert_eq!(tlb_entry::PERM_READ, 1 << 0);
        assert_eq!(tlb_entry::PERM_WRITE, 1 << 1);
        assert_eq!(tlb_entry::PERM_EXEC, 1 << 2);
        assert_eq!(tlb_entry::PERM_USER, 1 << 3);
        assert_eq!(tlb_entry::PERM_ACCESSED, 1 << 4);
        assert_eq!(tlb_entry::PERM_DIRTY, 1 << 5);
        assert_eq!(tlb_entry::PERM_GLOBAL, 1 << 6);

        // All bits should fit in one byte (verify no overflow in const definitions)
        let all_bits: u8 = tlb_entry::PERM_READ
            | tlb_entry::PERM_WRITE
            | tlb_entry::PERM_EXEC
            | tlb_entry::PERM_USER
            | tlb_entry::PERM_ACCESSED
            | tlb_entry::PERM_DIRTY
            | tlb_entry::PERM_GLOBAL;
        // Verify no bits overlap (all should be distinct)
        assert_eq!(all_bits.count_ones(), 7);
    }
}

