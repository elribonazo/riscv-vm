use super::core::Cpu;
use super::csr::{
    CSR_MENVCFG, CSR_MEPC, CSR_MHARTID, CSR_MIP, CSR_MSTATUS, CSR_SATP, CSR_SEPC, CSR_STIMECMP,
    CSR_TIME,
};
use crate::Mode;
use crate::Trap;
use crate::bus::Bus;
use crate::devices::clint::{CLINT_BASE, MTIME_OFFSET};
use crate::engine::block::{Block, BlockCompiler, CompileResult, MAX_BLOCK_SIZE};
use crate::engine::decoder::{self, Op, Register};
use crate::engine::microop::MicroOp;
use crate::mmu::AccessType as MmuAccessType;

impl Cpu {
    pub fn step(&mut self, bus: &dyn Bus) -> Result<(), Trap> {
        // Batch interrupt polling: only check every 256 instructions for performance.
        self.poll_counter = self.poll_counter.wrapping_add(1);

        if self.poll_counter == 0 {
            // Poll device-driven interrupts into MIP mask.
            let hart_id = self.csrs[CSR_MHARTID as usize] as usize;
            let mut hw_mip = bus.poll_interrupts_for_hart(hart_id);

            // Sstc support: raise STIP (bit 5) when time >= stimecmp and Sstc enabled.
            let menvcfg = self.csrs[CSR_MENVCFG as usize];
            let sstc_enabled = ((menvcfg >> 63) & 1) == 1;
            let stimecmp = self.csrs[CSR_STIMECMP as usize];
            if sstc_enabled && stimecmp != 0 {
                if let Ok(now) = bus.read64(CLINT_BASE + MTIME_OFFSET) {
                    if now >= stimecmp {
                        hw_mip |= 1 << 5; // STIP
                    }
                }
            }

            // Update MIP
            let hw_bits: u64 = (1 << 3) | (1 << 7) | (1 << 9) | (1 << 11);
            let hw_bits_with_stip: u64 = hw_bits | (1 << 5);
            let mask = if sstc_enabled {
                hw_bits_with_stip
            } else {
                hw_bits
            };
            let old_mip = self.csrs[CSR_MIP as usize];
            self.csrs[CSR_MIP as usize] = (old_mip & !mask) | (hw_mip & mask);

            if let Some(trap) = self.check_pending_interrupt() {
                return self.handle_trap(trap, self.pc, None);
            }
        }

        // Try superblock execution if enabled
        if self.use_blocks {
            if let Some(result) = self.try_execute_block(bus) {
                return result;
            }
        }

        // Fallback to single-step interpretation
        self.step_single_inner(bus)
    }

    /// Try to execute a compiled block at current PC.
    /// Returns Some(result) if block was executed, None if should fall back to interpreter.
    fn try_execute_block(&mut self, bus: &dyn Bus) -> Option<Result<(), Trap>> {
        let pc = self.pc;

        // Check block cache for existing block
        if let Some(block) = self.block_cache.get(pc) {
            // Clone needed values to avoid borrow issues
            let block_start_pc = block.start_pc;
            let block_len = block.len;
            let block_byte_len = block.byte_len;
            let block_ops: [MicroOp; MAX_BLOCK_SIZE] = block.ops;

            // Create a temporary block for execution
            let exec_block = Block {
                start_pc: block_start_pc,
                start_pa: block.start_pa,
                len: block_len,
                byte_len: block_byte_len,
                ops: block_ops,
                exec_count: 0,
                generation: block.generation,
            };

            // Execute the block
            let result = self.execute_block_inner(&exec_block, bus);

            // Update execution count
            if let Some(cached_block) = self.block_cache.get_mut(pc) {
                cached_block.exec_count = cached_block.exec_count.saturating_add(1);
            }

            return Some(self.handle_block_result(result, bus));
        }

        // Try to compile a new block
        let generation = self.block_cache.generation;
        let satp = self.csrs[CSR_SATP as usize];
        let mstatus = self.csrs[CSR_MSTATUS as usize];

        let compile_result = {
            let mut compiler = BlockCompiler {
                bus,
                satp,
                mstatus,
                mode: self.mode,
                tlb: &mut self.tlb,
            };
            compiler.compile(pc, generation)
        };

        match compile_result {
            CompileResult::Ok(block) => {
                // Clone needed values before inserting
                let exec_block = Block {
                    start_pc: block.start_pc,
                    start_pa: block.start_pa,
                    len: block.len,
                    byte_len: block.byte_len,
                    ops: block.ops,
                    exec_count: 0,
                    generation: block.generation,
                };

                // Insert into cache
                self.block_cache.insert(block);

                // Execute the block
                let result = self.execute_block_inner(&exec_block, bus);
                Some(self.handle_block_result(result, bus))
            }
            CompileResult::Trap(trap) => Some(self.handle_trap(trap, pc, None)),
            CompileResult::Unsuitable => {
                // Fall through to single-step
                None
            }
        }
    }

    /// Execute a single instruction (interpreter mode).
    /// This is the original step() implementation without the interrupt check.
    pub(super) fn step_single(&mut self, bus: &dyn Bus) -> Result<(), Trap> {
        // Check interrupts (needed when called from block exit)
        self.poll_counter = self.poll_counter.wrapping_add(1);
        if self.poll_counter == 0 {
            let hart_id = self.csrs[CSR_MHARTID as usize] as usize;
            let mut hw_mip = bus.poll_interrupts_for_hart(hart_id);

            let menvcfg = self.csrs[CSR_MENVCFG as usize];
            let sstc_enabled = ((menvcfg >> 63) & 1) == 1;
            let stimecmp = self.csrs[CSR_STIMECMP as usize];
            if sstc_enabled && stimecmp != 0 {
                if let Ok(now) = bus.read64(CLINT_BASE + MTIME_OFFSET) {
                    if now >= stimecmp {
                        hw_mip |= 1 << 5;
                    }
                }
            }

            let hw_bits: u64 = (1 << 3) | (1 << 7) | (1 << 9) | (1 << 11);
            let hw_bits_with_stip: u64 = hw_bits | (1 << 5);
            let mask = if sstc_enabled {
                hw_bits_with_stip
            } else {
                hw_bits
            };
            let old_mip = self.csrs[CSR_MIP as usize];
            self.csrs[CSR_MIP as usize] = (old_mip & !mask) | (hw_mip & mask);

            if let Some(trap) = self.check_pending_interrupt() {
                return self.handle_trap(trap, self.pc, None);
            }
        }

        self.step_single_inner(bus)
    }

    /// Inner implementation of single-step execution (no interrupt check).
    fn step_single_inner(&mut self, bus: &dyn Bus) -> Result<(), Trap> {
        let pc = self.pc;
        // Fetch (supports compressed 16-bit and regular 32-bit instructions)
        let (insn_raw, insn_len) = self.fetch_and_expand(bus)?;

        // Try decode cache first
        let op = if let Some(cached_op) = self.decode_cache_lookup(pc, insn_raw) {
            cached_op
        } else {
            // Cache miss: decode and insert
            let op = match decoder::decode(insn_raw) {
                Ok(v) => v,
                Err(trap) => return self.handle_trap(trap, pc, Some(insn_raw)),
            };
            self.decode_cache_insert(pc, insn_raw, op);
            op
        };

        let mut next_pc = pc.wrapping_add(insn_len as u64);

        match op {
            Op::Lui { rd, imm } => {
                self.write_reg(rd, imm as u64);
            }
            Op::Auipc { rd, imm } => {
                self.write_reg(rd, pc.wrapping_add(imm as u64));
            }
            Op::Jal { rd, imm } => {
                self.write_reg(rd, pc.wrapping_add(insn_len as u64));
                next_pc = pc.wrapping_add(imm as u64);
                if next_pc % 2 != 0 {
                    return self.handle_trap(
                        Trap::InstructionAddressMisaligned(next_pc),
                        pc,
                        Some(insn_raw),
                    );
                }
            }
            Op::Jalr { rd, rs1, imm } => {
                let target = self.read_reg(rs1).wrapping_add(imm as u64) & !1;
                self.write_reg(rd, pc.wrapping_add(insn_len as u64));
                next_pc = target;
                if next_pc % 2 != 0 {
                    return self.handle_trap(
                        Trap::InstructionAddressMisaligned(next_pc),
                        pc,
                        Some(insn_raw),
                    );
                }
            }
            Op::Branch {
                rs1,
                rs2,
                imm,
                funct3,
            } => {
                let val1 = self.read_reg(rs1);
                let val2 = self.read_reg(rs2);
                let taken = match funct3 {
                    0 => val1 == val2,                   // BEQ
                    1 => val1 != val2,                   // BNE
                    4 => (val1 as i64) < (val2 as i64),  // BLT
                    5 => (val1 as i64) >= (val2 as i64), // BGE
                    6 => val1 < val2,                    // BLTU
                    7 => val1 >= val2,                   // BGEU
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                if taken {
                    next_pc = pc.wrapping_add(imm as u64);
                    if next_pc % 2 != 0 {
                        return self.handle_trap(
                            Trap::InstructionAddressMisaligned(next_pc),
                            pc,
                            Some(insn_raw),
                        );
                    }
                }
            }
            Op::Load {
                rd,
                rs1,
                imm,
                funct3,
            } => {
                let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                let val = match funct3 {
                    0 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read8(pa) {
                            Ok(v) => (v as i8) as i64 as u64, // LB
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    1 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read16(pa) {
                            Ok(v) => (v as i16) as i64 as u64, // LH
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    2 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read32(pa) {
                            Ok(v) => (v as i32) as i64 as u64, // LW
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    3 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read64(pa) {
                            Ok(v) => v, // LD
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    4 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read8(pa) {
                            Ok(v) => v as u64, // LBU
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    5 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read16(pa) {
                            Ok(v) => v as u64, // LHU
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    6 => {
                        let pa = self.translate_addr(
                            bus,
                            addr,
                            MmuAccessType::Load,
                            pc,
                            Some(insn_raw),
                        )?;
                        match bus.read32(pa) {
                            Ok(v) => v as u64, // LWU
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                self.write_reg(rd, val);
            }
            Op::Store {
                rs1,
                rs2,
                imm,
                funct3,
            } => {
                let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                let pa =
                    self.translate_addr(bus, addr, MmuAccessType::Store, pc, Some(insn_raw))?;
                // Any store to the reservation granule clears LR/SC reservation.
                self.clear_reservation_if_conflict(addr);
                let val = self.read_reg(rs2);
                let res = match funct3 {
                    0 => bus.write8(pa, val as u8),   // SB
                    1 => bus.write16(pa, val as u16), // SH
                    2 => bus.write32(pa, val as u32), // SW
                    3 => bus.write64(pa, val),        // SD
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                if let Err(e) = res {
                    return self.handle_trap(e, pc, Some(insn_raw));
                }
            }
            Op::OpImm {
                rd,
                rs1,
                imm,
                funct3,
                funct7,
            } => {
                let val1 = self.read_reg(rs1);
                let res = match funct3 {
                    0 => val1.wrapping_add(imm as u64), // ADDI
                    2 => {
                        if (val1 as i64) < imm {
                            1
                        } else {
                            0
                        }
                    } // SLTI
                    3 => {
                        if val1 < (imm as u64) {
                            1
                        } else {
                            0
                        }
                    } // SLTIU
                    4 => val1 ^ (imm as u64),           // XORI
                    6 => val1 | (imm as u64),           // ORI
                    7 => val1 & (imm as u64),           // ANDI
                    1 => {
                        // SLLI
                        let shamt = imm & 0x3F;
                        val1 << shamt
                    }
                    5 => {
                        // SRLI / SRAI
                        let shamt = imm & 0x3F;
                        if funct7 & 0x20 != 0 {
                            // SRAI
                            ((val1 as i64) >> shamt) as u64
                        } else {
                            // SRLI
                            val1 >> shamt
                        }
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                self.write_reg(rd, res);
            }
            Op::Op {
                rd,
                rs1,
                rs2,
                funct3,
                funct7,
            } => {
                let val1 = self.read_reg(rs1);
                let val2 = self.read_reg(rs2);
                let res = match (funct3, funct7) {
                    (0, 0x00) => val1.wrapping_add(val2), // ADD
                    (0, 0x20) => val1.wrapping_sub(val2), // SUB
                    // M-extension (RV64M) - MUL/DIV/REM on XLEN=64
                    (0, 0x01) => {
                        // MUL: low 64 bits of signed(rs1) * signed(rs2)
                        let a = val1 as i64 as i128;
                        let b = val2 as i64 as i128;
                        (a.wrapping_mul(b) as i64) as u64
                    }
                    (1, 0x00) => val1 << (val2 & 0x3F), // SLL
                    (1, 0x01) => {
                        // MULH: high 64 bits of signed * signed
                        let a = val1 as i64 as i128;
                        let b = val2 as i64 as i128;
                        ((a.wrapping_mul(b) >> 64) as i64) as u64
                    }
                    (2, 0x00) => {
                        if (val1 as i64) < (val2 as i64) {
                            1
                        } else {
                            0
                        }
                    } // SLT
                    (2, 0x01) => {
                        // MULHSU: high 64 bits of signed * unsigned
                        let a = val1 as i64 as i128;
                        let b = val2 as u64 as i128;
                        ((a.wrapping_mul(b) >> 64) as i64) as u64
                    }
                    (3, 0x00) => {
                        if val1 < val2 {
                            1
                        } else {
                            0
                        }
                    } // SLTU
                    (3, 0x01) => {
                        // MULHU: high 64 bits of unsigned * unsigned
                        let a = val1 as u128;
                        let b = val2 as u128;
                        ((a.wrapping_mul(b) >> 64) as u64) as u64
                    }
                    (4, 0x00) => val1 ^ val2, // XOR
                    (4, 0x01) => {
                        // DIV (signed)
                        let a = val1 as i64;
                        let b = val2 as i64;
                        let q = if b == 0 {
                            -1i64
                        } else if a == i64::MIN && b == -1 {
                            i64::MIN
                        } else {
                            a / b
                        };
                        q as u64
                    }
                    (5, 0x00) => val1 >> (val2 & 0x3F), // SRL
                    (5, 0x01) => {
                        // DIVU (unsigned)
                        let a = val1;
                        let b = val2;
                        let q = if b == 0 { u64::MAX } else { a / b };
                        q
                    }
                    (5, 0x20) => ((val1 as i64) >> (val2 & 0x3F)) as u64, // SRA
                    (6, 0x00) => val1 | val2,                             // OR
                    (6, 0x01) => {
                        // REM (signed)
                        let a = val1 as i64;
                        let b = val2 as i64;
                        let r = if b == 0 {
                            a
                        } else if a == i64::MIN && b == -1 {
                            0
                        } else {
                            a % b
                        };
                        r as u64
                    }
                    (7, 0x00) => val1 & val2, // AND
                    (7, 0x01) => {
                        // REMU (unsigned)
                        let a = val1;
                        let b = val2;
                        let r = if b == 0 { a } else { a % b };
                        r
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                self.write_reg(rd, res);
            }
            Op::OpImm32 {
                rd,
                rs1,
                imm,
                funct3,
                funct7,
            } => {
                let val1 = self.read_reg(rs1);
                let res = match funct3 {
                    0 => (val1.wrapping_add(imm as u64) as i32) as i64 as u64, // ADDIW
                    1 => ((val1 as u32) << (imm & 0x1F)) as i32 as i64 as u64, // SLLIW
                    5 => {
                        let shamt = imm & 0x1F;
                        if funct7 & 0x20 != 0 {
                            // SRAIW
                            ((val1 as i32) >> shamt) as i64 as u64
                        } else {
                            // SRLIW
                            ((val1 as u32) >> shamt) as i32 as i64 as u64
                        }
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                self.write_reg(rd, res);
            }
            Op::Op32 {
                rd,
                rs1,
                rs2,
                funct3,
                funct7,
            } => {
                let val1 = self.read_reg(rs1);
                let val2 = self.read_reg(rs2);
                let res = match (funct3, funct7) {
                    (0, 0x00) => (val1.wrapping_add(val2) as i32) as i64 as u64, // ADDW
                    (0, 0x20) => (val1.wrapping_sub(val2) as i32) as i64 as u64, // SUBW
                    (0, 0x01) => {
                        // MULW: low 32 bits of signed* signed, sign-extended to 64
                        let a = val1 as i32 as i64;
                        let b = val2 as i32 as i64;
                        let prod = (a as i128).wrapping_mul(b as i128);
                        (prod as i32) as i64 as u64
                    }
                    (1, 0x00) => ((val1 as u32) << (val2 & 0x1F)) as i32 as i64 as u64, // SLLW
                    (5, 0x00) => ((val1 as u32) >> (val2 & 0x1F)) as i32 as i64 as u64, // SRLW
                    (4, 0x01) => {
                        // DIVW (signed 32-bit)
                        let a = val1 as i32 as i64;
                        let b = val2 as i32 as i64;
                        let q = if b == 0 {
                            -1i64
                        } else if a == i64::from(i32::MIN) && b == -1 {
                            i64::from(i32::MIN)
                        } else {
                            a / b
                        };
                        (q as i32) as i64 as u64
                    }
                    (5, 0x20) => ((val1 as i32) >> (val2 & 0x1F)) as i64 as u64, // SRAW
                    (5, 0x01) => {
                        // DIVUW (unsigned 32-bit)
                        let a = val1 as u32 as u64;
                        let b = val2 as u32 as u64;
                        let q = if b == 0 { u64::MAX } else { a / b };
                        (q as u32) as i32 as i64 as u64
                    }
                    (6, 0x01) => {
                        // REMW (signed 32-bit)
                        let a = val1 as i32 as i64;
                        let b = val2 as i32 as i64;
                        let r = if b == 0 {
                            a
                        } else if a == i64::from(i32::MIN) && b == -1 {
                            0
                        } else {
                            a % b
                        };
                        (r as i32) as i64 as u64
                    }
                    (7, 0x01) => {
                        // REMUW (unsigned 32-bit)
                        let a = val1 as u32 as u64;
                        let b = val2 as u32 as u64;
                        let r = if b == 0 { a } else { a % b };
                        (r as u32) as i32 as i64 as u64
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };
                self.write_reg(rd, res);
            }
            Op::Amo {
                rd,
                rs1,
                rs2,
                funct3,
                funct5,
                ..
            } => {
                let addr = self.read_reg(rs1);

                // Translate once per AMO/LD/ST sequence.
                let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;

                // Only word (funct3=2) and doubleword (funct3=3) widths are valid.
                let is_word = match funct3 {
                    2 => true,
                    3 => false,
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                };

                // LR/SC vs AMO op distinguished by funct5
                match funct5 {
                    0b00010 => {
                        // LR.W / LR.D
                        let loaded = if is_word {
                            match bus.read32(pa) {
                                Ok(v) => v as i32 as i64 as u64,
                                Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                            }
                        } else {
                            match bus.read64(pa) {
                                Ok(v) => v,
                                Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                            }
                        };
                        self.write_reg(rd, loaded);
                        self.reservation = Some(Self::reservation_granule(addr));
                    }
                    0b00011 => {
                        // SC.W / SC.D
                        // Alignment checks (must be naturally aligned) on the virtual address.
                        if is_word && addr % 4 != 0 {
                            return self.handle_trap(
                                Trap::StoreAddressMisaligned(addr),
                                pc,
                                Some(insn_raw),
                            );
                        }
                        if !is_word && addr % 8 != 0 {
                            return self.handle_trap(
                                Trap::StoreAddressMisaligned(addr),
                                pc,
                                Some(insn_raw),
                            );
                        }
                        let granule = Self::reservation_granule(addr);
                        if self.reservation == Some(granule) {
                            // Successful store
                            let val = self.read_reg(rs2);
                            let res = if is_word {
                                bus.write32(pa, val as u32)
                            } else {
                                bus.write64(pa, val)
                            };
                            if let Err(e) = res {
                                return self.handle_trap(e, pc, Some(insn_raw));
                            }
                            self.write_reg(rd, 0);
                            self.reservation = None;
                        } else {
                            // Failed store, no memory access
                            self.write_reg(rd, 1);
                        }
                    }
                    // AMO* operations - MUST be atomic across all harts
                    // Use Bus trait's atomic methods which properly synchronize
                    // across WASM workers using JavaScript Atomics API.
                    0b00001 => {
                        // AMOSWAP
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_swap(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b00000 => {
                        // AMOADD
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_add(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b00100 => {
                        // AMOXOR
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_xor(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b01000 => {
                        // AMOOR
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_or(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b01100 => {
                        // AMOAND
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_and(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b10000 => {
                        // AMOMIN (signed)
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_min(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b10100 => {
                        // AMOMAX (signed)
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_max(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b11000 => {
                        // AMOMINU (unsigned)
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_minu(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    0b11100 => {
                        // AMOMAXU (unsigned)
                        self.clear_reservation_if_conflict(addr);
                        let rs2_val = self.read_reg(rs2);
                        match bus.atomic_maxu(pa, rs2_val, is_word) {
                            Ok(old) => self.write_reg(rd, old),
                            Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                        }
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                }
            }
            Op::System {
                rd,
                rs1,
                funct3,
                imm,
                ..
            } => {
                match funct3 {
                    0 => {
                        // SYSTEM (ECALL/EBREAK, MRET/SRET, SFENCE.VMA)

                        // Detect SFENCE.VMA via mask/match (funct7=0001001, opcode=0x73, rd=0).
                        const SFENCE_VMA_MASK: u32 = 0b1111111_00000_00000_111_00000_1111111;
                        const SFENCE_VMA_MATCH: u32 = 0b0001001_00000_00000_000_00000_1110011; // 0x12000073

                        if (insn_raw & SFENCE_VMA_MASK) == SFENCE_VMA_MATCH {
                            // Only legal from S or M mode.
                            if matches!(self.mode, Mode::User) {
                                return self.handle_trap(
                                    Trap::IllegalInstruction(insn_raw as u64),
                                    pc,
                                    Some(insn_raw),
                                );
                            }
                            // Simplest implementation: flush entire TLB.
                            self.tlb.flush();
                            // Also invalidate decode cache (PC->PA mappings may have changed)
                            self.invalidate_decode_cache();
                        } else {
                            match insn_raw {
                                0x0010_0073 => {
                                    // EBREAK
                                    return self.handle_trap(Trap::Breakpoint, pc, Some(insn_raw));
                                }
                                0x1050_0073 => {
                                    // WFI - Wait For Interrupt
                                    // Instead of busy-spinning, hint to the CPU to reduce power usage.
                                    // This uses the PAUSE instruction on x86 or equivalent on other archs.
                                    // Multiple iterations give the scheduler a chance to run other threads.
                                    for _ in 0..10 {
                                        std::hint::spin_loop();
                                    }
                                }
                                0x0000_0073 => {
                                    // ECALL - route based on current privilege mode
                                    let trap = match self.mode {
                                        Mode::User => Trap::EnvironmentCallFromU,
                                        Mode::Supervisor => Trap::EnvironmentCallFromS,
                                        Mode::Machine => Trap::EnvironmentCallFromM,
                                    };
                                    return self.handle_trap(trap, pc, Some(insn_raw));
                                }
                                0x3020_0073 => {
                                    // MRET
                                    if self.mode != Mode::Machine {
                                        return self.handle_trap(
                                            Trap::IllegalInstruction(insn_raw as u64),
                                            pc,
                                            Some(insn_raw),
                                        );
                                    }

                                    let mut mstatus = self.csrs[CSR_MSTATUS as usize];
                                    let mepc = self.csrs[CSR_MEPC as usize];

                                    // Extract MPP and MPIE
                                    let mpp_bits = (mstatus >> 11) & 0b11;
                                    let mpie = (mstatus >> 7) & 1;

                                    // Set new privilege mode from MPP
                                    self.mode = Mode::from_mpp(mpp_bits);

                                    // MIE <= MPIE, MPIE <= 1, MPP <= U (00)
                                    mstatus = (mstatus & !(1 << 3)) | (mpie << 3);
                                    mstatus |= 1 << 7; // MPIE = 1
                                    mstatus &= !(0b11 << 11); // MPP = U (00)

                                    self.csrs[CSR_MSTATUS as usize] = mstatus;
                                    next_pc = mepc;
                                }
                                0x1020_0073 => {
                                    // SRET (only valid from S-mode)
                                    if self.mode != Mode::Supervisor {
                                        return self.handle_trap(
                                            Trap::IllegalInstruction(insn_raw as u64),
                                            pc,
                                            Some(insn_raw),
                                        );
                                    }

                                    // We model only the SPP/SIE/SPIE subset of mstatus.
                                    let mut mstatus = self.csrs[CSR_MSTATUS as usize];
                                    let sepc = self.csrs[CSR_SEPC as usize];

                                    // SPP is bit 8, SPIE is bit 5, SIE is bit 1.
                                    let spp = (mstatus >> 8) & 1;
                                    let spie = (mstatus >> 5) & 1;

                                    self.mode = if spp == 0 {
                                        Mode::User
                                    } else {
                                        Mode::Supervisor
                                    };

                                    // SIE <= SPIE, SPIE <= 1, SPP <= U (0)
                                    mstatus = (mstatus & !(1 << 1)) | (spie << 1);
                                    mstatus |= 1 << 5; // SPIE = 1
                                    mstatus &= !(1 << 8); // SPP = U

                                    self.csrs[CSR_MSTATUS as usize] = mstatus;
                                    next_pc = sepc;
                                }
                                _ => {
                                    return self.handle_trap(
                                        Trap::IllegalInstruction(insn_raw as u64),
                                        pc,
                                        Some(insn_raw),
                                    );
                                }
                            }
                        }
                    }
                    // Zicsr: CSRRW/CSRRS/CSRRC
                    1 | 2 | 3 | 5 | 6 | 7 => {
                        let csr_addr = (imm & 0xFFF) as u16;
                        // Dynamic read for time CSR to reflect CLINT MTIME.
                        let old = if csr_addr == CSR_TIME {
                            bus.read64(CLINT_BASE + MTIME_OFFSET).unwrap_or(0)
                        } else {
                            match self.read_csr(csr_addr) {
                                Ok(v) => v,
                                Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                            }
                        };

                        let mut write_new = None::<u64>;
                        match funct3 {
                            // CSRRW: write rs1, rd = old
                            1 => {
                                let rs1_val = self.read_reg(rs1);
                                write_new = Some(rs1_val);
                            }
                            // CSRRS: set bits in CSR with rs1
                            2 => {
                                let rs1_val = self.read_reg(rs1);
                                if rs1 != Register::X0 {
                                    write_new = Some(old | rs1_val);
                                }
                            }
                            // CSRRC: clear bits in CSR with rs1
                            3 => {
                                let rs1_val = self.read_reg(rs1);
                                if rs1 != Register::X0 {
                                    write_new = Some(old & !rs1_val);
                                }
                            }
                            // CSRRWI: write zero-extended zimm, rd = old
                            5 => {
                                let zimm = rs1.to_usize() as u64;
                                write_new = Some(zimm);
                            }
                            // CSRRSI: set bits using zimm (if non-zero)
                            6 => {
                                let zimm = rs1.to_usize() as u64;
                                if zimm != 0 {
                                    write_new = Some(old | zimm);
                                }
                            }
                            // CSRRCI: clear bits using zimm (if non-zero)
                            7 => {
                                let zimm = rs1.to_usize() as u64;
                                if zimm != 0 {
                                    write_new = Some(old & !zimm);
                                }
                            }
                            _ => {}
                        }

                        if let Some(new_val) = write_new {
                            if let Err(e) = self.write_csr(csr_addr, new_val) {
                                return self.handle_trap(e, pc, Some(insn_raw));
                            }
                            // Invalidate decode cache if SATP changed (address space switch)
                            if csr_addr == CSR_SATP {
                                self.tlb.flush();
                                self.invalidate_decode_cache();
                            }
                        }

                        if rd != Register::X0 {
                            self.write_reg(rd, old);
                        }
                    }
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        );
                    }
                }
            }
            Op::Fence => {
                // NOP
            }
        }

        self.pc = next_pc;
        Ok(())
    }
}
