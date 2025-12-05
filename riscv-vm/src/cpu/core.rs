use crate::bus::Bus;
use crate::engine::block::Block;
use crate::engine::cache::BlockCache;
use crate::engine::decoder::{self, Op, Register};
use crate::engine::microop::MicroOp;
use crate::mmu::{self, AccessType as MmuAccessType, Tlb};
use std::collections::HashMap;

use super::csr::{
    CSR_MCAUSE, CSR_MEDELEG, CSR_MEPC, CSR_MHARTID, CSR_MIDELEG, CSR_MIE, CSR_MIP, CSR_MISA,
    CSR_MSTATUS, CSR_MTVAL, CSR_MTVEC, CSR_SATP, CSR_SCAUSE, CSR_SEPC, CSR_STVAL, CSR_STVEC,
    CsrFile,
};
use super::types::{Mode, Trap};

/// Cached decode result.
/// Stores (pc, raw_instruction, decoded_op) for cache hit checking.
type DecodeCacheEntry = (u64, u32, Op);

/// Cache size (power of 2 for fast modulo)
const DECODE_CACHE_SIZE: usize = 256;
const DECODE_CACHE_MASK: usize = DECODE_CACHE_SIZE - 1;

/// Result of block execution.
pub(super) enum BlockExecResult {
    /// Block completed normally, next PC.
    Continue(u64),
    /// Block ended with a trap.
    Trap { trap: Trap, fault_pc: u64 },
    /// Block needs to exit to interpreter (CSR, atomic, etc.).
    Exit { next_pc: u64 },
}

/// RISC-V CPU core.
///
/// Aligned to 128 bytes to prevent false sharing when multiple CPUs are
/// allocated in an array or Vec. Most x86-64 cache lines are 64 bytes,
/// but Apple M1/M2 uses 128-byte cache lines.
#[repr(align(128))]
pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    /// Reservation set address for LR/SC (granule-aligned), or None if no reservation.
    pub(super) reservation: Option<u64>,
    /// Simple CSR storage for Zicsr (12-bit CSR address space).
    pub(crate) csrs: CsrFile,
    /// Current privilege mode (Machine/Supervisor/User).
    pub mode: Mode,
    /// Per-hart TLB for Sv39/Sv48 translation.
    pub tlb: Tlb,
    /// Poll counter for batching interrupt checks (rolls over every 256 instructions).
    /// Exposed for testing to force immediate interrupt polling.
    pub poll_counter: u8,
    /// Instruction decode cache.
    /// Key: pc & DECODE_CACHE_MASK
    /// Value: Some((full_pc, raw_insn, decoded_op)) or None
    decode_cache: [Option<DecodeCacheEntry>; DECODE_CACHE_SIZE],
    /// Block cache for superblock execution.
    pub block_cache: BlockCache,
    /// Enable/disable superblock optimization.
    pub use_blocks: bool,
}

impl Cpu {
    /// Create a new CPU with the given entry point and hart ID.
    ///
    /// # Arguments
    /// * `pc` - Initial program counter (kernel entry point)
    /// * `hart_id` - Hardware thread ID (0 for primary, 1+ for secondary)
    pub fn new(pc: u64, hart_id: u64) -> Self {
        let mut csrs = CsrFile::new();
        // misa: rv64imac_zicsr_zifencei (value from phase-0.md)
        const MISA_RV64IMAC_ZICSR_ZIFENCEI: u64 = 0x4000_0000_0018_1125;
        csrs[CSR_MISA as usize] = MISA_RV64IMAC_ZICSR_ZIFENCEI;
        csrs[CSR_MHARTID as usize] = hart_id; // Initialize hart ID

        // mstatus initial value: all zeros except UXL/SXL can be left as 0 (WARL).
        csrs[CSR_MSTATUS as usize] = 0;

        Self {
            regs: [0; 32],
            pc,
            reservation: None,
            csrs,
            mode: Mode::Machine,
            tlb: Tlb::new(),
            poll_counter: 0,
            decode_cache: [None; DECODE_CACHE_SIZE],
            block_cache: BlockCache::new(),
            use_blocks: false, // Disabled by default; enable for production workloads
        }
    }

    /// Export the current CSR image into a compact map suitable for
    /// serialization in snapshots.
    pub fn export_csrs(&self) -> HashMap<u16, u64> {
        self.csrs.export()
    }

    /// Restore CSRs from a previously exported map.
    ///
    /// Any CSR not present in the map is reset to 0. This is intentionally
    /// low-level and bypasses architectural WARL checks; it is only used for
    /// snapshot/restore.
    pub fn import_csrs(&mut self, map: &HashMap<u16, u64>) {
        self.csrs.import(map);
    }

    /// Look up instruction in decode cache
    #[inline]
    pub(super) fn decode_cache_lookup(&self, pc: u64, raw: u32) -> Option<Op> {
        let idx = (pc as usize) & DECODE_CACHE_MASK;
        if let Some((cached_pc, cached_raw, op)) = self.decode_cache[idx] {
            if cached_pc == pc && cached_raw == raw {
                return Some(op);
            }
        }
        None
    }

    /// Insert decoded instruction into cache
    #[inline]
    pub(super) fn decode_cache_insert(&mut self, pc: u64, raw: u32, op: Op) {
        let idx = (pc as usize) & DECODE_CACHE_MASK;
        self.decode_cache[idx] = Some((pc, raw, op));
    }

    /// Invalidate entire decode cache (call on TLB flush, context switch)
    pub fn invalidate_decode_cache(&mut self) {
        self.decode_cache = [None; DECODE_CACHE_SIZE];
    }

    /// Invalidate block cache on SATP write or SFENCE.VMA
    pub fn invalidate_blocks(&mut self) {
        self.block_cache.flush();
        self.invalidate_decode_cache();
    }

    pub fn read_reg(&self, reg: Register) -> u64 {
        if reg == Register::X0 {
            0
        } else {
            self.regs[reg.to_usize()]
        }
    }

    pub fn write_reg(&mut self, reg: Register, val: u64) {
        if reg != Register::X0 {
            self.regs[reg.to_usize()] = val;
        }
    }

    pub(super) fn reservation_granule(addr: u64) -> u64 {
        const GRANULE: u64 = 64;
        addr & !(GRANULE - 1)
    }

    pub(super) fn clear_reservation_if_conflict(&mut self, addr: u64) {
        if let Some(res) = self.reservation {
            if Self::reservation_granule(res) == Self::reservation_granule(addr) {
                self.reservation = None;
            }
        }
    }

    pub fn read_csr(&self, addr: u16) -> Result<u64, Trap> {
        self.csrs.read(addr, self.mode)
    }

    pub fn write_csr(&mut self, addr: u16, val: u64) -> Result<(), Trap> {
        self.csrs.write(addr, val, self.mode)
    }

    /// Map a `Trap` into (is_interrupt, cause, tval) per privileged spec, or `None` if it's a host-only error.
    fn trap_to_cause_tval(trap: &Trap) -> Option<(bool, u64, u64)> {
        match *trap {
            Trap::InstructionAddressMisaligned(addr) => Some((false, 0, addr)),
            Trap::InstructionAccessFault(addr) => Some((false, 1, addr)),
            Trap::IllegalInstruction(bits) => Some((false, 2, bits)),
            Trap::Breakpoint => Some((false, 3, 0)),
            Trap::LoadAddressMisaligned(addr) => Some((false, 4, addr)),
            Trap::LoadAccessFault(addr) => Some((false, 5, addr)),
            Trap::StoreAddressMisaligned(addr) => Some((false, 6, addr)),
            Trap::StoreAccessFault(addr) => Some((false, 7, addr)),
            Trap::EnvironmentCallFromU => Some((false, 8, 0)),
            Trap::EnvironmentCallFromS => Some((false, 9, 0)),
            Trap::EnvironmentCallFromM => Some((false, 11, 0)),
            Trap::InstructionPageFault(addr) => Some((false, 12, addr)),
            Trap::LoadPageFault(addr) => Some((false, 13, addr)),
            Trap::StorePageFault(addr) => Some((false, 15, addr)),

            Trap::SupervisorSoftwareInterrupt => Some((true, 1, 0)),
            Trap::MachineSoftwareInterrupt => Some((true, 3, 0)),
            Trap::SupervisorTimerInterrupt => Some((true, 5, 0)),
            Trap::MachineTimerInterrupt => Some((true, 7, 0)),
            Trap::SupervisorExternalInterrupt => Some((true, 9, 0)),
            Trap::MachineExternalInterrupt => Some((true, 11, 0)),

            Trap::RequestedTrap(_) | Trap::Fatal(_) => None,
        }
    }

    pub(super) fn handle_trap<T>(
        &mut self,
        trap: Trap,
        pc: u64,
        _insn_raw: Option<u32>,
    ) -> Result<T, Trap> {
        // Fatal/host-only traps bypass architectural trap entry.
        if let Some((is_interrupt, cause, tval)) = Self::trap_to_cause_tval(&trap) {
            // Determine delegation target per medeleg/mideleg
            let medeleg = self.csrs[CSR_MEDELEG as usize];
            let mideleg = self.csrs[CSR_MIDELEG as usize];
            let deleg_bit = 1u64 << (cause as u64);

            let deleg_to_s = match self.mode {
                // Delegation to a lower privilege is only meaningful when not in Machine mode
                Mode::Machine => false,
                _ => {
                    if is_interrupt {
                        (mideleg & deleg_bit) != 0
                    } else {
                        (medeleg & deleg_bit) != 0
                    }
                }
            };

            if deleg_to_s {
                // Supervisor trap entry (do not modify M-mode CSRs)
                // Save faulting PC and tval to supervisor CSRs
                self.csrs[CSR_SEPC as usize] = pc;
                self.csrs[CSR_STVAL as usize] = tval;
                let scause_val = ((is_interrupt as u64) << 63) | (cause & 0x7FFF_FFFF_FFFF_FFFF);
                self.csrs[CSR_SCAUSE as usize] = scause_val;

                // Update mstatus: SPP, SPIE, clear SIE
                let mut mstatus = self.csrs[CSR_MSTATUS as usize];
                if log::log_enabled!(log::Level::Trace) {
                    log::trace!("Trap to S-mode: mstatus_before={:x}", mstatus);
                }

                let sie = (mstatus >> 1) & 1;
                // SPIE <= SIE
                mstatus = (mstatus & !(1 << 5)) | (sie << 5);
                // SIE <= 0
                mstatus &= !(1 << 1);
                // SPP <= current privilege (1 if S, 0 if U)
                let spp = match self.mode {
                    Mode::Supervisor => 1,
                    _ => 0,
                };
                mstatus = (mstatus & !(1 << 8)) | (spp << 8);
                self.csrs[CSR_MSTATUS as usize] = mstatus;

                if log::log_enabled!(log::Level::Trace) {
                    log::trace!("Trap to S-mode: mstatus_after={:x}", mstatus);
                }

                self.mode = Mode::Supervisor;

                // Set PC to stvec (vectored if interrupt and mode==1)
                let stvec = self.csrs[CSR_STVEC as usize];
                let base = stvec & !0b11;
                let mode = stvec & 0b11;
                let vectored = mode == 1;
                let target_pc = if is_interrupt && vectored {
                    base.wrapping_add(4 * cause)
                } else {
                    base
                };
                self.pc = target_pc;
            } else {
                // Machine trap entry (default)
                // Save faulting PC and tval.
                self.csrs[CSR_MEPC as usize] = pc;
                self.csrs[CSR_MTVAL as usize] = tval;

                let mcause_val = ((is_interrupt as u64) << 63) | (cause & 0x7FFF_FFFF_FFFF_FFFF);
                self.csrs[CSR_MCAUSE as usize] = mcause_val;

                // Update mstatus: MPP, MPIE, clear MIE
                let mut mstatus = self.csrs[CSR_MSTATUS as usize];
                let mie = (mstatus >> 3) & 1;
                // MPIE <= MIE, MIE <= 0
                mstatus = (mstatus & !(1 << 7)) | (mie << 7);
                mstatus &= !(1 << 3);
                // MPP <= current mode.
                let mpp = self.mode.to_mpp();
                mstatus = (mstatus & !(0b11 << 11)) | (mpp << 11);
                self.csrs[CSR_MSTATUS as usize] = mstatus;
                self.mode = Mode::Machine;

                // Set PC to mtvec (vectored if interrupt and mode==1)
                let mtvec = self.csrs[CSR_MTVEC as usize];
                let base = mtvec & !0b11;
                let mode = mtvec & 0b11;
                let vectored = mode == 1;
                let target_pc = if is_interrupt && vectored {
                    base.wrapping_add(4 * cause)
                } else {
                    base
                };
                self.pc = target_pc;
            }
        }

        Err(trap)
    }

    /// Translate a virtual address to a physical address using the MMU.
    ///
    /// On translation failure, this enters the trap handler and returns the
    /// trap via `Err`.
    pub(super) fn translate_addr(
        &mut self,
        bus: &dyn Bus,
        vaddr: u64,
        access: MmuAccessType,
        pc: u64,
        insn_raw: Option<u32>,
    ) -> Result<u64, Trap> {
        let satp = self.csrs[CSR_SATP as usize];
        let mstatus = self.csrs[CSR_MSTATUS as usize];
        match mmu::translate(bus, &mut self.tlb, self.mode, satp, mstatus, vaddr, access) {
            Ok(pa) => Ok(pa),
            Err(trap) => self.handle_trap(trap, pc, insn_raw),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Block Execution Engine
    // ═══════════════════════════════════════════════════════════════════════

    /// Result of block execution.
    /// Continue(next_pc): Block completed normally
    /// Trap: Block ended with a trap
    /// Exit: Block needs to exit to interpreter
    pub(super) fn execute_block_inner(&mut self, block: &Block, bus: &dyn Bus) -> BlockExecResult {
        let base_pc = block.start_pc;
        let len = block.len as usize;
        // Copy the ops array to avoid borrow issues during execution
        let ops = block.ops;

        let mut idx = 0usize;

        while idx < len {
            let op = ops[idx];
            idx += 1;

            match op {
                // ═══════════════════════════════════════════════════════════
                // Fast path: ALU operations (no memory, no traps)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Addi { rd, rs1, imm } => {
                    let val = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Add { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize].wrapping_add(self.regs[rs2 as usize]);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sub { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize].wrapping_sub(self.regs[rs2 as usize]);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Xori { rd, rs1, imm } => {
                    let val = self.regs[rs1 as usize] ^ (imm as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Ori { rd, rs1, imm } => {
                    let val = self.regs[rs1 as usize] | (imm as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Andi { rd, rs1, imm } => {
                    let val = self.regs[rs1 as usize] & (imm as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Xor { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize] ^ self.regs[rs2 as usize];
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Or { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize] | self.regs[rs2 as usize];
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::And { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize] & self.regs[rs2 as usize];
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Slli { rd, rs1, shamt } => {
                    let val = self.regs[rs1 as usize] << shamt;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Srli { rd, rs1, shamt } => {
                    let val = self.regs[rs1 as usize] >> shamt;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Srai { rd, rs1, shamt } => {
                    let val = ((self.regs[rs1 as usize] as i64) >> shamt) as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sll { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize] << (self.regs[rs2 as usize] & 0x3F);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Srl { rd, rs1, rs2 } => {
                    let val = self.regs[rs1 as usize] >> (self.regs[rs2 as usize] & 0x3F);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sra { rd, rs1, rs2 } => {
                    let val = ((self.regs[rs1 as usize] as i64) >> (self.regs[rs2 as usize] & 0x3F))
                        as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Slti { rd, rs1, imm } => {
                    let val = if (self.regs[rs1 as usize] as i64) < imm {
                        1
                    } else {
                        0
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sltiu { rd, rs1, imm } => {
                    let val = if self.regs[rs1 as usize] < (imm as u64) {
                        1
                    } else {
                        0
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Slt { rd, rs1, rs2 } => {
                    let val = if (self.regs[rs1 as usize] as i64) < (self.regs[rs2 as usize] as i64)
                    {
                        1
                    } else {
                        0
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sltu { rd, rs1, rs2 } => {
                    let val = if self.regs[rs1 as usize] < self.regs[rs2 as usize] {
                        1
                    } else {
                        0
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Lui { rd, imm } => {
                    if rd != 0 {
                        self.regs[rd as usize] = imm as u64;
                    }
                }

                MicroOp::Auipc { rd, imm, pc_offset } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let val = pc.wrapping_add(imm as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                // ═══════════════════════════════════════════════════════════
                // 32-bit ALU operations
                // ═══════════════════════════════════════════════════════════
                MicroOp::Addiw { rd, rs1, imm } => {
                    let val =
                        (self.regs[rs1 as usize].wrapping_add(imm as u64) as i32) as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Addw { rd, rs1, rs2 } => {
                    let val = (self.regs[rs1 as usize].wrapping_add(self.regs[rs2 as usize]) as i32)
                        as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Subw { rd, rs1, rs2 } => {
                    let val = (self.regs[rs1 as usize].wrapping_sub(self.regs[rs2 as usize]) as i32)
                        as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Slliw { rd, rs1, shamt } => {
                    let val = ((self.regs[rs1 as usize] as u32) << shamt) as i32 as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Srliw { rd, rs1, shamt } => {
                    let val = ((self.regs[rs1 as usize] as u32) >> shamt) as i32 as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sraiw { rd, rs1, shamt } => {
                    let val = ((self.regs[rs1 as usize] as i32) >> shamt) as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sllw { rd, rs1, rs2 } => {
                    let val = ((self.regs[rs1 as usize] as u32) << (self.regs[rs2 as usize] & 0x1F))
                        as i32 as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Srlw { rd, rs1, rs2 } => {
                    let val = ((self.regs[rs1 as usize] as u32) >> (self.regs[rs2 as usize] & 0x1F))
                        as i32 as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Sraw { rd, rs1, rs2 } => {
                    let val = ((self.regs[rs1 as usize] as i32) >> (self.regs[rs2 as usize] & 0x1F))
                        as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                // ═══════════════════════════════════════════════════════════
                // M-extension (Multiply/Divide)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Mul { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i64 as i128;
                    let b = self.regs[rs2 as usize] as i64 as i128;
                    let val = (a.wrapping_mul(b) as i64) as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Mulh { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i64 as i128;
                    let b = self.regs[rs2 as usize] as i64 as i128;
                    let val = ((a.wrapping_mul(b) >> 64) as i64) as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Mulhsu { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i64 as i128;
                    let b = self.regs[rs2 as usize] as u64 as i128;
                    let val = ((a.wrapping_mul(b) >> 64) as i64) as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Mulhu { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as u128;
                    let b = self.regs[rs2 as usize] as u128;
                    let val = (a.wrapping_mul(b) >> 64) as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Div { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i64;
                    let b = self.regs[rs2 as usize] as i64;
                    let val = if b == 0 {
                        -1i64 as u64
                    } else if a == i64::MIN && b == -1 {
                        i64::MIN as u64
                    } else {
                        (a / b) as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Divu { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize];
                    let b = self.regs[rs2 as usize];
                    let val = if b == 0 { u64::MAX } else { a / b };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Rem { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i64;
                    let b = self.regs[rs2 as usize] as i64;
                    let val = if b == 0 {
                        a as u64
                    } else if a == i64::MIN && b == -1 {
                        0
                    } else {
                        (a % b) as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Remu { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize];
                    let b = self.regs[rs2 as usize];
                    let val = if b == 0 { a } else { a % b };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Mulw { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i32 as i64;
                    let b = self.regs[rs2 as usize] as i32 as i64;
                    let val = (a.wrapping_mul(b) as i32) as i64 as u64;
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Divw { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i32;
                    let b = self.regs[rs2 as usize] as i32;
                    let val = if b == 0 {
                        -1i32 as i64 as u64
                    } else if a == i32::MIN && b == -1 {
                        i32::MIN as i64 as u64
                    } else {
                        (a / b) as i64 as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Divuw { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as u32;
                    let b = self.regs[rs2 as usize] as u32;
                    let val = if b == 0 {
                        u32::MAX as i32 as i64 as u64
                    } else {
                        (a / b) as i32 as i64 as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Remw { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as i32;
                    let b = self.regs[rs2 as usize] as i32;
                    let val = if b == 0 {
                        a as i64 as u64
                    } else if a == i32::MIN && b == -1 {
                        0
                    } else {
                        (a % b) as i64 as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                MicroOp::Remuw { rd, rs1, rs2 } => {
                    let a = self.regs[rs1 as usize] as u32;
                    let b = self.regs[rs2 as usize] as u32;
                    let val = if b == 0 {
                        a as i32 as i64 as u64
                    } else {
                        (a % b) as i32 as i64 as u64
                    };
                    if rd != 0 {
                        self.regs[rd as usize] = val;
                    }
                }

                // ═══════════════════════════════════════════════════════════
                // Load operations (may trap)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Ld {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read64(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = val;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lw {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read32(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = (val as i32) as i64 as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lwu {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read32(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = val as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lh {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read16(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = (val as i16) as i64 as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lhu {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read16(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = val as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lb {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read8(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = (val as i8) as i64 as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                MicroOp::Lbu {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Load) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    match bus.read8(pa) {
                        Ok(val) => {
                            if rd != 0 {
                                self.regs[rd as usize] = val as u64;
                            }
                        }
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    }
                }

                // ═══════════════════════════════════════════════════════════
                // Store operations (may trap)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Sd {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let val = self.regs[rs2 as usize];
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Store) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    if let Err(trap) = bus.write64(pa, val) {
                        return BlockExecResult::Trap { trap, fault_pc: pc };
                    }
                    self.clear_reservation_if_conflict(addr);
                }

                MicroOp::Sw {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let val = self.regs[rs2 as usize] as u32;
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Store) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    if let Err(trap) = bus.write32(pa, val) {
                        return BlockExecResult::Trap { trap, fault_pc: pc };
                    }
                    self.clear_reservation_if_conflict(addr);
                }

                MicroOp::Sh {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let val = self.regs[rs2 as usize] as u16;
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Store) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    if let Err(trap) = bus.write16(pa, val) {
                        return BlockExecResult::Trap { trap, fault_pc: pc };
                    }
                    self.clear_reservation_if_conflict(addr);
                }

                MicroOp::Sb {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                } => {
                    let addr = self.regs[rs1 as usize].wrapping_add(imm as u64);
                    let val = self.regs[rs2 as usize] as u8;
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let pa = match self.translate_addr_for_block(bus, addr, MmuAccessType::Store) {
                        Ok(pa) => pa,
                        Err(trap) => return BlockExecResult::Trap { trap, fault_pc: pc },
                    };
                    if let Err(trap) = bus.write8(pa, val) {
                        return BlockExecResult::Trap { trap, fault_pc: pc };
                    }
                    self.clear_reservation_if_conflict(addr);
                }

                // ═══════════════════════════════════════════════════════════
                // Control flow (block terminators)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Jal {
                    rd,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let link = pc.wrapping_add(insn_len as u64);
                    if rd != 0 {
                        self.regs[rd as usize] = link;
                    }
                    let next_pc = pc.wrapping_add(imm as u64);
                    return BlockExecResult::Continue(next_pc);
                }

                MicroOp::Jalr {
                    rd,
                    rs1,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let link = pc.wrapping_add(insn_len as u64);
                    let target = (self.regs[rs1 as usize].wrapping_add(imm as u64)) & !1;
                    if rd != 0 {
                        self.regs[rd as usize] = link;
                    }
                    return BlockExecResult::Continue(target);
                }

                MicroOp::Beq {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let next = if self.regs[rs1 as usize] == self.regs[rs2 as usize] {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                MicroOp::Bne {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let next = if self.regs[rs1 as usize] != self.regs[rs2 as usize] {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                MicroOp::Blt {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let taken = (self.regs[rs1 as usize] as i64) < (self.regs[rs2 as usize] as i64);
                    let next = if taken {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                MicroOp::Bge {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let taken =
                        (self.regs[rs1 as usize] as i64) >= (self.regs[rs2 as usize] as i64);
                    let next = if taken {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                MicroOp::Bltu {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let taken = self.regs[rs1 as usize] < self.regs[rs2 as usize];
                    let next = if taken {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                MicroOp::Bgeu {
                    rs1,
                    rs2,
                    imm,
                    pc_offset,
                    insn_len,
                } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    let taken = self.regs[rs1 as usize] >= self.regs[rs2 as usize];
                    let next = if taken {
                        pc.wrapping_add(imm as u64)
                    } else {
                        pc.wrapping_add(insn_len as u64)
                    };
                    return BlockExecResult::Continue(next);
                }

                // ═══════════════════════════════════════════════════════════
                // System operations (exit to interpreter)
                // ═══════════════════════════════════════════════════════════
                MicroOp::Ecall { pc_offset }
                | MicroOp::Ebreak { pc_offset }
                | MicroOp::Mret { pc_offset }
                | MicroOp::Sret { pc_offset }
                | MicroOp::SfenceVma { pc_offset }
                | MicroOp::Csrrw { pc_offset, .. }
                | MicroOp::Csrrs { pc_offset, .. }
                | MicroOp::Csrrc { pc_offset, .. }
                | MicroOp::Csrrwi { pc_offset, .. }
                | MicroOp::Csrrsi { pc_offset, .. }
                | MicroOp::Csrrci { pc_offset, .. }
                | MicroOp::LrW { pc_offset, .. }
                | MicroOp::LrD { pc_offset, .. }
                | MicroOp::ScW { pc_offset, .. }
                | MicroOp::ScD { pc_offset, .. }
                | MicroOp::AmoSwap { pc_offset, .. }
                | MicroOp::AmoAdd { pc_offset, .. }
                | MicroOp::AmoXor { pc_offset, .. }
                | MicroOp::AmoAnd { pc_offset, .. }
                | MicroOp::AmoOr { pc_offset, .. }
                | MicroOp::AmoMin { pc_offset, .. }
                | MicroOp::AmoMax { pc_offset, .. }
                | MicroOp::AmoMinu { pc_offset, .. }
                | MicroOp::AmoMaxu { pc_offset, .. } => {
                    let pc = base_pc.wrapping_add(pc_offset as u64);
                    return BlockExecResult::Exit { next_pc: pc };
                }

                MicroOp::Wfi { pc_offset: _ } => {
                    // WFI: spin briefly and continue
                    for _ in 0..10 {
                        std::hint::spin_loop();
                    }
                    // Continue to next instruction in block
                }

                MicroOp::Fence => {
                    // No-op in our memory model
                }
            }
        }

        // Block ended without terminator (shouldn't happen with valid compilation)
        BlockExecResult::Continue(base_pc.wrapping_add(block.byte_len as u64))
    }

    /// Translate address without entering trap handler (for block execution)
    fn translate_addr_for_block(
        &mut self,
        bus: &dyn Bus,
        vaddr: u64,
        access: MmuAccessType,
    ) -> Result<u64, Trap> {
        let satp = self.csrs[CSR_SATP as usize];
        let mstatus = self.csrs[CSR_MSTATUS as usize];
        mmu::translate(bus, &mut self.tlb, self.mode, satp, mstatus, vaddr, access)
    }

    /// Handle block execution result and return to normal step() flow
    pub(super) fn handle_block_result(
        &mut self,
        result: BlockExecResult,
        bus: &dyn Bus,
    ) -> Result<(), Trap> {
        match result {
            BlockExecResult::Continue(next_pc) => {
                self.pc = next_pc;
                Ok(())
            }
            BlockExecResult::Trap { trap, fault_pc } => self.handle_trap(trap, fault_pc, None),
            BlockExecResult::Exit { next_pc } => {
                // Re-run current instruction with interpreter
                self.pc = next_pc;
                self.step_single(bus)
            }
        }
    }

    #[inline]
    pub(super) fn fetch_and_expand(&mut self, bus: &dyn Bus) -> Result<(u32, u8), Trap> {
        let pc = self.pc;
        if pc % 2 != 0 {
            return self.handle_trap(Trap::InstructionAddressMisaligned(pc), pc, None);
        }

        // Optimization: if PC is 4-byte aligned, try to read 32 bits at once
        if pc % 4 == 0 {
            let pa = self.translate_addr(bus, pc, MmuAccessType::Instruction, pc, None)?;
            if let Ok(word) = bus.read32(pa) {
                // Check if it's a compressed instruction (bits [1:0] != 0b11)
                if word & 0x3 != 0x3 {
                    // Compressed: expand lower 16 bits
                    let insn32 = match decoder::expand_compressed((word & 0xFFFF) as u16) {
                        Ok(v) => v,
                        Err(trap) => return self.handle_trap(trap, pc, None),
                    };
                    return Ok((insn32, 2));
                }
                // Full 32-bit instruction
                return Ok((word, 4));
            }
            // Fall through to 16-bit fetch on read32 failure
        }

        // Fetch first halfword via MMU (instruction access).
        let pa_low = self.translate_addr(bus, pc, MmuAccessType::Instruction, pc, None)?;
        let half = match bus.read16(pa_low) {
            Ok(v) => v,
            Err(e) => {
                // Map load faults from the bus into instruction faults.
                let mapped = match e {
                    Trap::LoadAccessFault(_) => Trap::InstructionAccessFault(pc),
                    Trap::LoadAddressMisaligned(_) => Trap::InstructionAddressMisaligned(pc),
                    other => other,
                };
                return self.handle_trap(mapped, pc, None);
            }
        };

        if half & 0x3 != 0x3 {
            // Compressed 16-bit instruction
            let insn32 = match decoder::expand_compressed(half) {
                Ok(v) => v,
                Err(trap) => return self.handle_trap(trap, pc, None),
            };
            Ok((insn32, 2))
        } else {
            // 32-bit instruction; fetch high half via MMU as well.
            let pc_hi = pc.wrapping_add(2);
            let pa_hi = self.translate_addr(bus, pc_hi, MmuAccessType::Instruction, pc, None)?;
            let hi = match bus.read16(pa_hi) {
                Ok(v) => v,
                Err(e) => {
                    let mapped = match e {
                        Trap::LoadAccessFault(_) => Trap::InstructionAccessFault(pc),
                        Trap::LoadAddressMisaligned(_) => Trap::InstructionAddressMisaligned(pc),
                        other => other,
                    };
                    return self.handle_trap(mapped, pc, None);
                }
            };
            let insn32 = (half as u32) | ((hi as u32) << 16);
            Ok((insn32, 4))
        }
    }

    pub(super) fn check_pending_interrupt(&self) -> Option<Trap> {
        let mstatus = self.csrs[CSR_MSTATUS as usize];
        let mip = self.csrs[CSR_MIP as usize];
        let mie = self.csrs[CSR_MIE as usize];
        let mideleg = self.csrs[CSR_MIDELEG as usize];

        // SIE is a shadow of MIE for supervisor interrupt bits (SSIP=1, STIP=5, SEIP=9)
        let sie_mask: u64 = (1 << 1) | (1 << 5) | (1 << 9);
        let sie = mie & sie_mask;

        // Mask delegated interrupts out of machine set, and into supervisor set.
        let m_pending = (mip & mie) & !mideleg;
        let s_pending = (mip & sie) & mideleg;

        // Machine mode global enable:
        // Enabled if (currently in Machine and MIE==1) OR (currently below Machine).
        let m_enabled = match self.mode {
            Mode::Machine => ((mstatus >> 3) & 1) == 1, // MIE
            _ => true,
        };
        if m_enabled {
            if (m_pending & (1 << 11)) != 0 {
                return Some(Trap::MachineExternalInterrupt);
            } // MEIP
            if (m_pending & (1 << 3)) != 0 {
                return Some(Trap::MachineSoftwareInterrupt);
            } // MSIP
            if (m_pending & (1 << 7)) != 0 {
                return Some(Trap::MachineTimerInterrupt);
            } // MTIP
        }

        // Supervisor mode global enable:
        // Enabled if (currently in Supervisor and SIE==1) OR (currently in User).
        let s_enabled = match self.mode {
            Mode::Machine => false,
            Mode::Supervisor => ((mstatus >> 1) & 1) == 1, // SIE
            Mode::User => true,
        };

        // DEBUG: Check if we are about to trap for interrupt when we shouldn't
        if s_enabled && s_pending != 0 {
            if log::log_enabled!(log::Level::Trace) {
                log::trace!(
                    "Interrupt pending: s_pending={:x} mstatus={:x} mode={:?}",
                    s_pending,
                    mstatus,
                    self.mode
                );
            }
        }

        if s_enabled {
            if (s_pending & (1 << 9)) != 0 {
                return Some(Trap::SupervisorExternalInterrupt);
            } // SEIP
            if (s_pending & (1 << 1)) != 0 {
                return Some(Trap::SupervisorSoftwareInterrupt);
            } // SSIP
            if (s_pending & (1 << 5)) != 0 {
                return Some(Trap::SupervisorTimerInterrupt);
            } // STIP
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SystemBus;

    // --- Memory layout tests (Task 10.1) ---------------------------------

    #[test]
    fn measure_struct_sizes() {
        println!("=== Phase 10 Memory Layout Analysis ===");
        println!("Cpu size: {} bytes", std::mem::size_of::<Cpu>());
        println!("Cpu align: {} bytes", std::mem::align_of::<Cpu>());
        println!();

        // Verify cache-line alignment for multi-hart safety
        assert!(
            std::mem::align_of::<Cpu>() >= 64,
            "Cpu should be cache-line aligned (>= 64 bytes)"
        );
    }

    #[test]
    fn test_cpu_alignment() {
        // Verify Cpu is properly aligned for cache-line isolation
        assert!(
            std::mem::align_of::<Cpu>() >= 64,
            "Cpu alignment should be at least 64 bytes for cache-line isolation"
        );

        // On Apple Silicon, cache lines are 128 bytes, so we align to 128
        assert_eq!(
            std::mem::align_of::<Cpu>(),
            128,
            "Cpu should be aligned to 128 bytes for Apple M1/M2 compatibility"
        );
    }

    #[test]
    fn test_cpu_array_no_false_sharing() {
        // Create multiple CPUs in a Vec to simulate multi-hart scenario
        let cpus: Vec<Cpu> = (0..4).map(|i| Cpu::new(0x8000_0000, i as u64)).collect();

        // Verify that adjacent CPUs are at least 64 bytes apart (one cache line)
        for i in 0..cpus.len() - 1 {
            let addr_i = &cpus[i] as *const _ as usize;
            let addr_j = &cpus[i + 1] as *const _ as usize;
            let distance = addr_j - addr_i;

            assert!(
                distance >= 64,
                "CPUs {} and {} are only {} bytes apart (need >= 64 for cache-line isolation)",
                i,
                i + 1,
                distance
            );
        }
    }

    // --- Test helpers ----------------------------------------------------

    fn encode_i(imm: i32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
        (((imm as u32) & 0xFFF) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
    }

    fn encode_r(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
    }

    fn encode_s(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
        let imm = imm as u32;
        let imm11_5 = (imm >> 5) & 0x7F;
        let imm4_0 = imm & 0x1F;
        (imm11_5 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (imm4_0 << 7) | opcode
    }

    fn encode_b(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
        // imm is a signed byte offset, must be multiple of 2
        let imm = imm as u32;
        let imm12 = (imm >> 12) & 0x1;
        let imm10_5 = (imm >> 5) & 0x3F;
        let imm4_1 = (imm >> 1) & 0xF;
        let imm11 = (imm >> 11) & 0x1;

        (imm12 << 31)
            | (imm10_5 << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | (imm4_1 << 8)
            | (imm11 << 7)
            | opcode
    }

    fn make_bus() -> SystemBus {
        SystemBus::new(0x8000_0000, 1024 * 1024) // 1MB
    }

    fn encode_amo(
        funct5: u32,
        aq: bool,
        rl: bool,
        rs2: u32,
        rs1: u32,
        funct3: u32,
        rd: u32,
    ) -> u32 {
        let funct7 = (funct5 << 2) | ((aq as u32) << 1) | (rl as u32);
        encode_r(funct7, rs2, rs1, funct3, rd, 0x2F)
    }

    #[test]
    fn test_addi() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // ADDI x1, x0, -1
        let insn = encode_i(-1, 0, 0, 1, 0x13);
        bus.write32(0x8000_0000, insn).unwrap();

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(cpu.pc, 0x8000_0004);
    }

    #[test]
    fn test_lui() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // LUI x2, 0x12345
        // imm field is already << 12 in the encoding helper
        let imm = 0x12345 << 12;
        let insn = ((imm as u32) & 0xFFFFF000) | (2 << 7) | 0x37;
        bus.write32(0x8000_0000, insn).unwrap();

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X2), 0x0000_0000_1234_5000);
    }

    #[test]
    fn test_load_store() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // SD x1, 0(x2) -> Store x1 at x2+0
        // x1 = 0xDEADBEEF, x2 = 0x8000_0100
        cpu.write_reg(Register::X1, 0xDEADBEEF);
        cpu.write_reg(Register::X2, 0x8000_0100);

        // SD: Op=0x23, funct3=3, rs1=2, rs2=1, imm=0
        // Using manual encoding here: imm=0 so only rs2/rs1/funct3/opcode matter.
        let sd_insn = (1 << 20) | (2 << 15) | (3 << 12) | 0x23;
        bus.write32(0x8000_0000, sd_insn).unwrap();

        cpu.step(&bus).unwrap();
        assert_eq!(bus.read64(0x8000_0100).unwrap(), 0xDEADBEEF);

        // LD x3, 0(x2) -> Load x3 from x2+0
        // LD: Op=0x03, funct3=3, rd=3, rs1=2, imm=0
        let ld_insn = (2 << 15) | (3 << 12) | (3 << 7) | 0x03;
        bus.write32(0x8000_0004, ld_insn).unwrap();

        cpu.step(&bus).unwrap(); // Execute SD (pc was incremented in previous step? No wait)
        // Previous step PC went 0->4. Now at 4.

        assert_eq!(cpu.read_reg(Register::X3), 0xDEADBEEF);
    }

    #[test]
    fn test_x0_invariant() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // Place a value in memory
        let addr = 0x8000_0100;
        bus.write64(addr, 0xDEAD_BEEF_DEAD_BEEF).unwrap();

        // Set x2 = addr
        cpu.write_reg(Register::X2, addr);

        // 1) ADDI x0, x0, 5
        let addi_x0 = encode_i(5, 0, 0, 0, 0x13);
        // 2) LD x0, 0(x2)
        let ld_x0 = encode_i(0, 2, 3, 0, 0x03);

        bus.write32(0x8000_0000, addi_x0).unwrap();
        bus.write32(0x8000_0004, ld_x0).unwrap();

        cpu.step(&bus).unwrap();
        cpu.step(&bus).unwrap();

        // x0 must remain hard-wired to zero
        assert_eq!(cpu.read_reg(Register::X0), 0);
    }

    #[test]
    fn test_branch_taken_and_not_taken() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // BEQ x1, x2, +8 (pc + 8 when taken)
        let beq_insn = encode_b(8, 2, 1, 0x0, 0x63);
        bus.write32(0x8000_0000, beq_insn).unwrap();

        // Taken: x1 == x2
        cpu.write_reg(Register::X1, 5);
        cpu.write_reg(Register::X2, 5);
        cpu.pc = 0x8000_0000;
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0008);

        // Not taken: x1 != x2
        cpu.write_reg(Register::X1, 1);
        cpu.write_reg(Register::X2, 2);
        cpu.pc = 0x8000_0000;
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0004);
    }

    #[test]
    fn test_w_ops_sign_extension() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // Set x1 = 0x0000_0000_8000_0000 (low 32 bits have sign bit set)
        cpu.write_reg(Register::X1, 0x0000_0000_8000_0000);
        cpu.write_reg(Register::X2, 0); // x2 = 0

        // ADDW x3, x1, x2  (opcode=0x3B, funct3=0, funct7=0)
        let addw = encode_r(0x00, 2, 1, 0x0, 3, 0x3B);
        bus.write32(0x8000_0000, addw).unwrap();

        cpu.step(&bus).unwrap();

        // Expect sign-extended 32-bit result: 0xFFFF_FFFF_8000_0000
        assert_eq!(cpu.read_reg(Register::X3), 0xFFFF_FFFF_8000_0000);
    }

    #[test]
    fn test_m_extension_mul_div_rem() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // MUL: 3 * 4 = 12
        cpu.write_reg(Register::X1, 3);
        cpu.write_reg(Register::X2, 4);
        let mul = encode_r(0x01, 2, 1, 0x0, 3, 0x33); // MUL x3, x1, x2
        bus.write32(0x8000_0000, mul).unwrap();
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X3), 12);

        // MULH / MULHSU / MULHU basic sanity using large values
        cpu.pc = 0x8000_0004;
        cpu.write_reg(Register::X1, 0x8000_0000_0000_0000);
        cpu.write_reg(Register::X2, 2);
        let mulh = encode_r(0x01, 2, 1, 0x1, 4, 0x33); // MULH x4, x1, x2
        let mulhsu = encode_r(0x01, 2, 1, 0x2, 5, 0x33); // MULHSU x5, x1, x2
        let mulhu = encode_r(0x01, 2, 1, 0x3, 6, 0x33); // MULHU x6, x1, x2
        bus.write32(0x8000_0004, mulh).unwrap();
        bus.write32(0x8000_0008, mulhsu).unwrap();
        bus.write32(0x8000_000C, mulhu).unwrap();
        cpu.step(&bus).unwrap();
        cpu.step(&bus).unwrap();
        cpu.step(&bus).unwrap();

        // From spec example: low product is 0, high signed part negative
        assert_eq!(cpu.read_reg(Register::X3), 12);
        assert_ne!(cpu.read_reg(Register::X4), 0);
        assert_ne!(cpu.read_reg(Register::X5), 0);
        assert_ne!(cpu.read_reg(Register::X6), 0);

        // DIV / DIVU / REM / REMU corner cases
        cpu.pc = 0x8000_0010;
        cpu.write_reg(Register::X1, u64::MAX); // -1 as signed
        cpu.write_reg(Register::X2, 0);
        let div = encode_r(0x01, 2, 1, 0x4, 7, 0x33); // DIV x7, x1, x2
        let divu = encode_r(0x01, 2, 1, 0x5, 8, 0x33); // DIVU x8, x1, x2
        let rem = encode_r(0x01, 2, 1, 0x6, 9, 0x33); // REM x9, x1, x2
        let remu = encode_r(0x01, 2, 1, 0x7, 10, 0x33); // REMU x10, x1, x2
        bus.write32(0x8000_0010, div).unwrap();
        bus.write32(0x8000_0014, divu).unwrap();
        bus.write32(0x8000_0018, rem).unwrap();
        bus.write32(0x8000_001C, remu).unwrap();

        for _ in 0..4 {
            cpu.step(&bus).unwrap();
        }

        assert_eq!(cpu.read_reg(Register::X7), u64::MAX); // DIV by 0 -> -1
        assert_eq!(cpu.read_reg(Register::X8), u64::MAX); // DIVU by 0 -> all ones
        assert_eq!(cpu.read_reg(Register::X9), u64::MAX); // REM by 0 -> rs1
        assert_eq!(cpu.read_reg(Register::X10), u64::MAX); // REMU by 0 -> rs1

        // Overflow case: -(2^63) / -1 -> -(2^63), rem = 0
        cpu.pc = 0x8000_0020;
        cpu.write_reg(Register::X1, i64::MIN as u64);
        cpu.write_reg(Register::X2, (!0u64) as u64); // -1
        let div_over = encode_r(0x01, 2, 1, 0x4, 11, 0x33); // DIV x11, x1, x2
        let rem_over = encode_r(0x01, 2, 1, 0x6, 12, 0x33); // REM x12, x1, x2
        bus.write32(0x8000_0020, div_over).unwrap();
        bus.write32(0x8000_0024, rem_over).unwrap();
        cpu.step(&bus).unwrap();
        cpu.step(&bus).unwrap();

        assert_eq!(cpu.read_reg(Register::X11), i64::MIN as u64);
        assert_eq!(cpu.read_reg(Register::X12), 0);
    }

    #[test]
    fn test_compressed_addi_and_lwsp_paths() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // Encodings from assembler with rv64imac (see rvc_test.S in dev notes):
        let c_addi_x11_1: u16 = 0x0585; // addi x11,x11,1 (C.ADDI)
        let c_addi16sp_16: u16 = 0x0141; // addi sp,sp,16 (C.ADDI16SP)
        let c_lwsp_a5_12: u16 = 0x47B2; // lw a5,12(sp) (C.LWSP)

        // Initialize registers / memory
        cpu.write_reg(Register::X11, 10);
        let sp_base = 0x8000_0100;
        cpu.write_reg(Register::X2, sp_base); // sp
        // After C.ADDI16SP 16, sp = sp_base + 16. C.LWSP uses offset 12 from new sp.
        bus.write32(sp_base + 16 + 12, 0xDEAD_BEEF).unwrap();

        // Place compressed instructions at 0,2,4
        bus.write16(0x8000_0000, c_addi_x11_1).unwrap();
        bus.write16(0x8000_0002, c_addi16sp_16).unwrap();
        bus.write16(0x8000_0004, c_lwsp_a5_12).unwrap();

        // Execute three steps; PC should advance by 2 for each compressed inst.
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0002);
        assert_eq!(cpu.read_reg(Register::X11), 11);

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0004);
        assert_eq!(cpu.read_reg(Register::X2), 0x8000_0110); // sp + 16

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0006);
        assert_eq!(cpu.read_reg(Register::X15), 0xFFFF_FFFF_DEAD_BEEF); // a5 (sign-extended lw)
    }

    #[test]
    fn test_zicsr_basic_csrs() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);
        let csr_addr: u32 = 0x300; // mstatus

        // CSRRWI x1, mstatus, 5  (mstatus = 5, x1 = old = 0)
        let csrrwi = {
            let zimm = 5u32;
            (csr_addr << 20) | (zimm << 15) | (0x5 << 12) | (1 << 7) | 0x73
        };
        bus.write32(0x8000_0000, csrrwi).unwrap();
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0);

        // CSRRSI x2, mstatus, 0xA  (mstatus = 5 | 0xA = 0xF, x2 = old = 5)
        let csrrsi = {
            let zimm = 0xAu32;
            (csr_addr << 20) | (zimm << 15) | (0x6 << 12) | (2 << 7) | 0x73
        };
        bus.write32(0x8000_0004, csrrsi).unwrap();
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X2), 5);

        // CSRRCI x3, mstatus, 0x3  (mstatus = 0xF & !0x3 = 0xC, x3 = old = 0xF)
        let csrrci = {
            let zimm = 0x3u32;
            (csr_addr << 20) | (zimm << 15) | (0x7 << 12) | (3 << 7) | 0x73
        };
        bus.write32(0x8000_0008, csrrci).unwrap();
        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X3), 0xF);
    }

    #[test]
    fn test_a_extension_lr_sc_basic() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        let addr = 0x8000_0200;
        bus.write64(addr, 0xDEAD_BEEF_DEAD_BEEF).unwrap();

        cpu.write_reg(Register::X1, addr); // base
        cpu.write_reg(Register::X2, 0x0123_4567_89AB_CDEF); // value to store with SC

        // LR.D x3, 0(x1)
        let lr_d = encode_amo(0b00010, false, false, 0, 1, 0x3, 3);
        // SC.D x4, x2, 0(x1)
        let sc_d = encode_amo(0b00011, false, false, 2, 1, 0x3, 4);

        bus.write32(0x8000_0000, lr_d).unwrap();
        bus.write32(0x8000_0004, sc_d).unwrap();

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X3), 0xDEAD_BEEF_DEAD_BEEF);

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X4), 0); // SC success
        assert_eq!(bus.read64(addr).unwrap(), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn test_a_extension_reservation_and_misaligned_sc() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        let addr = 0x8000_0300;
        bus.write64(addr, 0xCAFEBABE_F00D_F00D).unwrap();

        cpu.write_reg(Register::X1, addr); // base
        cpu.write_reg(Register::X2, 1); // increment

        // LR.D to establish reservation
        let lr_d = encode_amo(0b00010, false, false, 0, 1, 0x3, 3);
        // AMOADD.D x4, x2, 0(x1) -> increments and clears reservation
        let amoadd_d = encode_amo(0b00000, false, false, 2, 1, 0x3, 4);
        // SC.D x5, x2, 0(x1) -> should fail (x5=1) because reservation cleared
        let sc_d = encode_amo(0b00011, false, false, 2, 1, 0x3, 5);

        bus.write32(0x8000_0000, lr_d).unwrap();
        bus.write32(0x8000_0004, amoadd_d).unwrap();
        bus.write32(0x8000_0008, sc_d).unwrap();

        cpu.step(&bus).unwrap(); // LR
        cpu.step(&bus).unwrap(); // AMOADD
        cpu.step(&bus).unwrap(); // SC (should fail)

        assert_eq!(cpu.read_reg(Register::X5), 1);

        // Misaligned SC.D must trap with StoreAddressMisaligned
        cpu.pc = 0x8000_0010;
        cpu.write_reg(Register::X1, addr + 1); // misaligned
        let sc_misaligned = encode_amo(0b00011, false, false, 2, 1, 0x3, 6);
        bus.write32(0x8000_0010, sc_misaligned).unwrap();

        let res = cpu.step(&bus);
        match res {
            Err(Trap::StoreAddressMisaligned(a)) => assert_eq!(a, addr + 1),
            _ => panic!("Expected StoreAddressMisaligned trap"),
        }
    }

    #[test]
    fn test_load_sign_and_zero_extension() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        let addr = 0x8000_0100;
        // 0xFFEE_DDCC_BBAA_9988 laid out little-endian in memory
        bus.write64(addr, 0xFFEE_DDCC_BBAA_9988).unwrap();

        cpu.write_reg(Register::X1, addr); // base pointer

        // LB x2, 0(x1)
        let lb = encode_i(0, 1, 0, 2, 0x03);
        // LBU x3, 0(x1)
        let lbu = encode_i(0, 1, 4, 3, 0x03);
        // LH x4, 0(x1)
        let lh = encode_i(0, 1, 1, 4, 0x03);
        // LHU x5, 0(x1)
        let lhu = encode_i(0, 1, 5, 5, 0x03);
        // LW x6, 0(x1)
        let lw = encode_i(0, 1, 2, 6, 0x03);
        // LWU x7, 0(x1)
        let lwu = encode_i(0, 1, 6, 7, 0x03);
        // LD x8, 0(x1)
        let ld = encode_i(0, 1, 3, 8, 0x03);

        let base_pc = 0x8000_0000;
        for (i, insn) in [lb, lbu, lh, lhu, lw, lwu, ld].into_iter().enumerate() {
            bus.write32(base_pc + (i as u64) * 4, insn).unwrap();
        }

        // Execute all loads
        for _ in 0..7 {
            cpu.step(&bus).unwrap();
        }

        assert_eq!(cpu.read_reg(Register::X2), 0xFFFF_FFFF_FFFF_FF88); // LB (sign-extended 0x88)
        assert_eq!(cpu.read_reg(Register::X3), 0x88); // LBU
        assert_eq!(cpu.read_reg(Register::X4), 0xFFFF_FFFF_FFFF_9988); // LH
        assert_eq!(cpu.read_reg(Register::X5), 0x9988); // LHU
        assert_eq!(cpu.read_reg(Register::X6), 0xFFFF_FFFF_BBAA_9988); // LW
        assert_eq!(cpu.read_reg(Register::X7), 0xBBAA_9988); // LWU
        assert_eq!(cpu.read_reg(Register::X8), 0xFFEE_DDCC_BBAA_9988); // LD
    }

    #[test]
    fn test_misaligned_load_and_store_traps() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // x2 = misaligned address
        cpu.write_reg(Register::X2, 0x8000_0001);

        // LW x1, 0(x2)  -> should trap with LoadAddressMisaligned
        let lw = encode_i(0, 2, 2, 1, 0x03);
        bus.write32(0x8000_0000, lw).unwrap();

        let res = cpu.step(&bus);
        match res {
            Err(Trap::LoadAddressMisaligned(a)) => assert_eq!(a, 0x8000_0001),
            _ => panic!("Expected LoadAddressMisaligned trap"),
        }

        // SW x1, 0(x2)  -> should trap with StoreAddressMisaligned
        cpu.pc = 0x8000_0000;
        let sw = encode_s(0, 1, 2, 2, 0x23);
        bus.write32(0x8000_0000, sw).unwrap();

        let res = cpu.step(&bus);
        match res {
            Err(Trap::StoreAddressMisaligned(a)) => assert_eq!(a, 0x8000_0001),
            _ => panic!("Expected StoreAddressMisaligned trap"),
        }
    }

    #[test]
    fn test_access_fault_outside_dram() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // LW x1, 0(x0) -> effective address 0x0 (outside DRAM, but aligned)
        let lw = encode_i(0, 0, 2, 1, 0x03);
        bus.write32(0x8000_0000, lw).unwrap();

        let res = cpu.step(&bus);
        match res {
            Err(Trap::LoadAccessFault(a)) => assert_eq!(a, 0),
            _ => panic!("Expected LoadAccessFault trap"),
        }
    }

    #[test]
    fn test_jal() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // JAL x1, 8
        // Op=0x6F, rd=1, imm=8.
        // J-type: imm[20|10:1|11|19:12]
        // imm=8 (0x8). bit3=1.
        // imm[10:1] = 0100... no wait.
        // 8 = 1000 binary.
        // bit1..10 -> bits 1..4 are 0010 ? No.
        // 8 >> 1 = 4.
        // imm[10:1] = 4.
        // insn: imm[20] | imm[10:1] | imm[11] | imm[19:12] | rd | opcode
        // 0 | 4 | 0 | 0 | 1 | 0x6F
        // (4 << 21) | (1 << 7) | 0x6F
        let jal_insn = (4 << 21) | (1 << 7) | 0x6F;
        bus.write32(0x8000_0000, jal_insn).unwrap();

        cpu.step(&bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0x8000_0004); // Link address
        assert_eq!(cpu.pc, 0x8000_0008); // Target
    }

    #[test]
    fn test_misaligned_fetch() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0001, 0); // Odd PC

        let res = cpu.step(&bus);
        match res {
            Err(Trap::InstructionAddressMisaligned(addr)) => assert_eq!(addr, 0x8000_0001),
            _ => panic!("Expected misaligned trap"),
        }
    }

    #[test]
    fn test_smoke_sum() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);

        // Data at 0x8000_0100
        let data: [u32; 5] = [1, 2, 3, 4, 5];
        for (i, val) in data.iter().enumerate() {
            bus.write32(0x8000_0100 + (i * 4) as u64, *val).unwrap();
        }

        // Program
        // Need to construct 0x80000100 without sign extension issues.
        // 1. ADDI x1, x0, 1
        // 2. SLLI x1, x1, 31 -> 0x80000000
        // 3. ADDI x1, x1, 0x100 -> 0x80000100
        let prog = [
            0x00100093, // addi x1, x0, 1
            0x01F09093, // slli x1, x1, 31
            0x10008093, // addi x1, x1, 0x100 -> Base
            0x00500113, // addi x2, x0, 5 -> Count
            0x00000193, // addi x3, x0, 0 -> Sum
            // loop:
            0x0000A203, // lw x4, 0(x1)
            0x004181B3, // add x3, x3, x4
            0x00408093, // addi x1, x1, 4
            0xFFF10113, // addi x2, x2, -1
            0xFE0118E3, // bne x2, x0, loop (-16)
            0x00100073, // ebreak
        ];

        for (i, val) in prog.iter().enumerate() {
            bus.write32(0x8000_0000 + (i * 4) as u64, *val).unwrap();
        }

        // Run until ebreak
        let mut steps = 0;
        loop {
            steps += 1;
            if steps > 1000 {
                panic!("Infinite loop");
            }
            match cpu.step(&bus) {
                Ok(_) => {}
                Err(Trap::Breakpoint) => break,
                Err(e) => panic!("Unexpected trap at pc 0x{:x}: {:?}", cpu.pc, e),
            }
        }

        // Check sum
        assert_eq!(cpu.read_reg(Register::X3), 15);
    }

    #[test]
    fn test_interrupts_clint_plic() {
        let bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000, 0);
        // Set poll_counter to 255 so the first step triggers immediate interrupt check
        cpu.poll_counter = 255;

        // 1. Setup MTVEC to 0x8000_1000 (Direct)
        let mtvec_val = 0x8000_1000;
        cpu.write_csr(CSR_MTVEC, mtvec_val).unwrap();

        // 2. Enable MIE in mstatus (Global Interrupt Enable)
        // mstatus bit 3 is MIE.
        let mstatus_val = 1 << 3;
        cpu.write_csr(CSR_MSTATUS, mstatus_val).unwrap();

        // 3. Enable MTIE (Timer) and MEIE (External) and MSIE (Software) in mie
        // MTIE=7, MEIE=11, MSIE=3
        let mie_val = (1 << 7) | (1 << 11) | (1 << 3);
        cpu.write_csr(CSR_MIE, mie_val).unwrap();

        // --- Test CLINT Timer Interrupt ---
        // Set mtimecmp[0] to 100
        bus.clint.set_mtimecmp(0, 100);
        // Set mtime to 101 (trigger condition)
        bus.clint.set_mtime(101);

        // We need a valid instruction at PC to attempt fetch, although interrupt checks before fetch.
        bus.write32(0x8000_0000, 0x00000013).unwrap(); // NOP (addi x0, x0, 0)

        let res = cpu.step(&bus);
        match res {
            Err(Trap::MachineTimerInterrupt) => {
                // Success
                assert_eq!(cpu.pc, 0x8000_1000); // jumped to mtvec
                // Check mcause: Interrupt=1, Cause=7 -> 0x8000...0007
                let mcause = cpu.read_csr(CSR_MCAUSE).unwrap();
                assert_eq!(mcause, 0x8000_0000_0000_0007);
            }
            _ => panic!("Expected MachineTimerInterrupt, got {:?}", res),
        }

        // Clear the interrupt condition
        bus.clint.set_mtimecmp(0, 200);
        // CPU is now at handler. We need to "return" (mret) or just reset state for next test.
        // Reset PC back to start
        cpu.pc = 0x8000_0000;
        // Re-enable MIE (trap disabled it)
        let mut mstatus = cpu.read_csr(CSR_MSTATUS).unwrap();
        mstatus |= 1 << 3;
        cpu.write_csr(CSR_MSTATUS, mstatus).unwrap();
        // Set poll_counter to 255 for immediate interrupt check on next step
        cpu.poll_counter = 255;

        // --- Test PLIC UART Interrupt ---
        // Configure PLIC
        // 1. Set Priority for Source 10 (UART) to 1
        bus.plic.store(0x000000 + 4 * 10, 4, 1).unwrap();
        // 2. Enable Source 10 for Context 0 (M-mode)
        // Enable addr for ctx 0: 0x002000. Bit 10.
        bus.plic.store(0x002000, 4, 1 << 10).unwrap();
        // 3. Set Threshold for Context 0 to 0
        bus.plic.store(0x200000, 4, 0).unwrap();

        // Trigger UART Interrupt
        // Writing to IER (Enable RDIE=1) and pushing a char to Input
        // UART RBR is at offset 0. IER is at offset 1.
        bus.uart.store(1, 1, 1).unwrap(); // IER = 1 (RX Data Available Interrupt)
        bus.uart.push_input(b'A');
        // This should set uart.lsr[0]=1, and because IER[0]=1, uart.interrupting=true.

        // Update bus interrupts so PLIC sees UART line high
        bus.check_interrupts();

        // Step CPU
        let res = cpu.step(&bus);
        match res {
            Err(Trap::MachineExternalInterrupt) => {
                // Success
                assert_eq!(cpu.pc, 0x8000_1000);
                let mcause = cpu.read_csr(CSR_MCAUSE).unwrap();
                assert_eq!(mcause, 0x8000_0000_0000_000B); // Cause 11
            }
            _ => panic!("Expected MachineExternalInterrupt, got {:?}", res),
        }
    }
}
