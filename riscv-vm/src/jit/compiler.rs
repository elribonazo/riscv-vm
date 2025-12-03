//! JIT Compiler: Block to WASM Translation
//!
//! This module translates `Block` (containing `MicroOp`s) into WASM binary.

use super::encoder::{imports, WasmModuleBuilder};
use super::state::{offsets, JIT_STATE_OFFSET};
use super::types::{exit_codes, CompilationResult, JitConfig};
use crate::engine::block::Block;
use crate::engine::microop::MicroOp;
use wasm_encoder::{BlockType, Instruction as I, MemArg, ValType};

/// JIT compiler for RISC-V blocks.
pub struct JitCompiler {
    config: JitConfig,
    /// Tracks if the current block uses memory operations (loads/stores)
    /// which require helper function imports.
    uses_memory_ops: bool,
}

impl JitCompiler {
    /// Create a new JIT compiler with the given configuration.
    pub fn new(config: JitConfig) -> Self {
        Self {
            config,
            uses_memory_ops: false,
        }
    }

    /// Compile a block to WASM bytes.
    ///
    /// # Arguments
    /// * `block` - The pre-decoded basic block to compile
    ///
    /// # Returns
    /// * `CompilationResult::Success(bytes)` - Valid WASM module bytes
    /// * `CompilationResult::Unsuitable` - Block cannot be JIT'd
    /// * `CompilationResult::Error(msg)` - Internal compilation error
    pub fn compile(&mut self, block: &Block) -> CompilationResult {
        // Reset state for this compilation
        self.uses_memory_ops = false;

        // Check if block is worth JIT'ing
        if (block.len as usize) < self.config.min_block_size {
            return CompilationResult::Unsuitable;
        }

        let mut builder = WasmModuleBuilder::new();

        // Reserve locals for common temporaries
        // Local 0 = cpu_state_ptr (parameter)
        // Local 1 = temp_addr (for memory operations)
        // Local 2 = temp_val (for computed values)
        let _temp_addr = builder.add_local(ValType::I64);
        let _temp_val = builder.add_local(ValType::I64);

        // First pass: check if any ops use memory
        for op in block.ops() {
            if op.may_trap() {
                self.uses_memory_ops = true;
                break;
            }
        }

        // Compile each MicroOp with interrupt checks
        for (i, op) in block.ops().iter().enumerate() {
            // Get the PC offset for this op (used in interrupt check exit codes)
            // For ops without pc_offset, use index * 4 as estimate (typical RISC-V insn size)
            let pc_offset = op.pc_offset().unwrap_or((i as u16) * 4);

            // Check if we should insert an interrupt check before this op
            if self.should_check_interrupts(block, i, op) {
                self.emit_interrupt_check(&mut builder, pc_offset);
            }

            if !self.compile_microop(&mut builder, op, block.start_pc) {
                return CompilationResult::Unsuitable;
            }
        }

        // Default return: success with block's total byte length
        self.emit_exit_normal(&mut builder, block.byte_len as u16);

        let bytes = if self.uses_memory_ops {
            builder.build_with_imports()
        } else {
            builder.build()
        };

        if self.config.debug_wat {
            eprintln!(
                "[JIT] Compiled block at {:#x}, {} ops, {} bytes WASM (mem_ops={})",
                block.start_pc,
                block.len,
                bytes.len(),
                self.uses_memory_ops
            );
        }

        CompilationResult::Success(bytes)
    }

    /// Compile a single MicroOp to WASM instructions.
    ///
    /// Returns false if the op cannot be JIT'd (must fall back to interpreter).
    fn compile_microop(&self, builder: &mut WasmModuleBuilder, op: &MicroOp, base_pc: u64) -> bool {
        match *op {
            // ═══════════════════════════════════════════════════════════════
            // ALU Operations - These are straightforward to JIT
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Addi { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                } // NOP

                // regs[rd] = regs[rs1] + imm
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Add { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Add);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sub { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Sub);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Xori { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Xor);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Ori { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Or);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Andi { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64And);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Slti { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64LtS);
                builder.emit(I::I64ExtendI32U);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sltiu { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64LtU);
                builder.emit(I::I64ExtendI32U);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Slli { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(shamt as i64));
                builder.emit(I::I64Shl);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Srli { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(shamt as i64));
                builder.emit(I::I64ShrU);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Srai { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(shamt as i64));
                builder.emit(I::I64ShrS);
                self.emit_store_reg(builder, rd);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // Register-Register ALU (complete set)
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Xor { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Xor);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Or { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Or);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::And { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64And);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sll { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Const(0x3F)); // Mask to 6 bits for RV64
                builder.emit(I::I64And);
                builder.emit(I::I64Shl);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Srl { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Const(0x3F));
                builder.emit(I::I64And);
                builder.emit(I::I64ShrU);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sra { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Const(0x3F));
                builder.emit(I::I64And);
                builder.emit(I::I64ShrS);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Slt { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64LtS);
                builder.emit(I::I64ExtendI32U);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sltu { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64LtU);
                builder.emit(I::I64ExtendI32U);
                self.emit_store_reg(builder, rd);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // 32-bit Word Operations (RV64I)
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Addiw { rd, rs1, imm } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm as i64));
                builder.emit(I::I64Add);
                // Truncate to 32 bits and sign-extend
                builder.emit(I::I32WrapI64);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Slliw { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(shamt as i32));
                builder.emit(I::I32Shl);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Srliw { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(shamt as i32));
                builder.emit(I::I32ShrU);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sraiw { rd, rs1, shamt } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(shamt as i32));
                builder.emit(I::I32ShrS);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Addw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Add);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Subw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Sub);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sllw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(0x1F)); // Mask to 5 bits for 32-bit
                builder.emit(I::I32And);
                builder.emit(I::I32Shl);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Srlw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(0x1F));
                builder.emit(I::I32And);
                builder.emit(I::I32ShrU);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Sraw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Const(0x1F));
                builder.emit(I::I32And);
                builder.emit(I::I32ShrS);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // M-Extension: Multiply/Divide
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Mul { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Mul);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Mulw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I32WrapI64);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Mul);
                builder.emit(I::I64ExtendI32S);
                self.emit_store_reg(builder, rd);
                true
            }

            // MULH, MULHSU, MULHU need 128-bit math - fall back to interpreter
            MicroOp::Mulh { .. } | MicroOp::Mulhsu { .. } | MicroOp::Mulhu { .. } => false,

            MicroOp::Div { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                // Handle division by zero: result = -1
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    // Division by zero: return -1
                    builder.emit(I::I64Const(-1i64));
                }
                builder.emit(I::Else);
                {
                    // Also check overflow: MIN_INT / -1 = MIN_INT (no trap in RISC-V)
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I64Const(i64::MIN));
                    builder.emit(I::I64Eq);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I64Const(-1i64));
                    builder.emit(I::I64Eq);
                    builder.emit(I::I32And);
                    builder.emit(I::If(BlockType::Result(ValType::I64)));
                    {
                        // Overflow: return MIN_INT
                        builder.emit(I::I64Const(i64::MIN));
                    }
                    builder.emit(I::Else);
                    {
                        // Normal division
                        self.emit_load_reg(builder, rs1);
                        self.emit_load_reg(builder, rs2);
                        builder.emit(I::I64DivS);
                    }
                    builder.emit(I::End);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Divu { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                // Handle division by zero: result = MAX_UINT (all 1s)
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    builder.emit(I::I64Const(-1i64)); // 0xFFFF_FFFF_FFFF_FFFF
                }
                builder.emit(I::Else);
                {
                    self.emit_load_reg(builder, rs1);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I64DivU);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Rem { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                // Handle division by zero: result = dividend
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    self.emit_load_reg(builder, rs1);
                }
                builder.emit(I::Else);
                {
                    // Also check overflow: MIN_INT % -1 = 0
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I64Const(i64::MIN));
                    builder.emit(I::I64Eq);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I64Const(-1i64));
                    builder.emit(I::I64Eq);
                    builder.emit(I::I32And);
                    builder.emit(I::If(BlockType::Result(ValType::I64)));
                    {
                        builder.emit(I::I64Const(0));
                    }
                    builder.emit(I::Else);
                    {
                        self.emit_load_reg(builder, rs1);
                        self.emit_load_reg(builder, rs2);
                        builder.emit(I::I64RemS);
                    }
                    builder.emit(I::End);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Remu { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                // Handle division by zero: result = dividend
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    self.emit_load_reg(builder, rs1);
                }
                builder.emit(I::Else);
                {
                    self.emit_load_reg(builder, rs1);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I64RemU);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            // 32-bit divide/remainder need careful 32-bit semantics
            MicroOp::Divw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                // Get low 32 bits of rs2
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    // Division by zero: return -1 sign-extended
                    builder.emit(I::I64Const(-1i64));
                }
                builder.emit(I::Else);
                {
                    // Check for overflow: MIN_INT32 / -1
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32Const(i32::MIN));
                    builder.emit(I::I32Eq);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32Const(-1i32));
                    builder.emit(I::I32Eq);
                    builder.emit(I::I32And);
                    builder.emit(I::If(BlockType::Result(ValType::I64)));
                    {
                        // Overflow: return MIN_INT32 sign-extended
                        builder.emit(I::I64Const(i32::MIN as i64));
                    }
                    builder.emit(I::Else);
                    {
                        self.emit_load_reg(builder, rs1);
                        builder.emit(I::I32WrapI64);
                        self.emit_load_reg(builder, rs2);
                        builder.emit(I::I32WrapI64);
                        builder.emit(I::I32DivS);
                        builder.emit(I::I64ExtendI32S);
                    }
                    builder.emit(I::End);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Divuw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    // Division by zero: return -1 (all 1s, sign-extended is still -1)
                    builder.emit(I::I64Const(-1i64));
                }
                builder.emit(I::Else);
                {
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32DivU);
                    builder.emit(I::I64ExtendI32S);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Remw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    // Remainder by zero: return dividend sign-extended
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I64ExtendI32S);
                }
                builder.emit(I::Else);
                {
                    // Check for overflow: MIN_INT32 % -1 = 0
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32Const(i32::MIN));
                    builder.emit(I::I32Eq);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32Const(-1i32));
                    builder.emit(I::I32Eq);
                    builder.emit(I::I32And);
                    builder.emit(I::If(BlockType::Result(ValType::I64)));
                    {
                        builder.emit(I::I64Const(0));
                    }
                    builder.emit(I::Else);
                    {
                        self.emit_load_reg(builder, rs1);
                        builder.emit(I::I32WrapI64);
                        self.emit_load_reg(builder, rs2);
                        builder.emit(I::I32WrapI64);
                        builder.emit(I::I32RemS);
                        builder.emit(I::I64ExtendI32S);
                    }
                    builder.emit(I::End);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            MicroOp::Remuw { rd, rs1, rs2 } => {
                if rd == 0 {
                    return true;
                }
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);
                builder.emit(I::I32Eqz);
                builder.emit(I::If(BlockType::Result(ValType::I64)));
                {
                    // Remainder by zero: return dividend sign-extended
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I64ExtendI32S);
                }
                builder.emit(I::Else);
                {
                    self.emit_load_reg(builder, rs1);
                    builder.emit(I::I32WrapI64);
                    self.emit_load_reg(builder, rs2);
                    builder.emit(I::I32WrapI64);
                    builder.emit(I::I32RemU);
                    builder.emit(I::I64ExtendI32S);
                }
                builder.emit(I::End);
                self.emit_store_reg(builder, rd);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // Upper Immediate Operations
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Lui { rd, imm } => {
                if rd == 0 {
                    return true;
                }

                // Store imm directly to register
                builder.emit(I::I32Const(0)); // base for memory access
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Store(MemArg {
                    offset: (JIT_STATE_OFFSET as u64) + (offsets::reg(rd) as u64),
                    align: 3,
                    memory_index: 0,
                }));
                true
            }

            MicroOp::Auipc { rd, imm, pc_offset } => {
                if rd == 0 {
                    return true;
                }

                // rd = pc + imm, where pc is base_pc + pc_offset
                let result = (base_pc as i64) + (pc_offset as i64) + imm;
                builder.emit(I::I32Const(0)); // base for memory access
                builder.emit(I::I64Const(result));
                builder.emit(I::I64Store(MemArg {
                    offset: (JIT_STATE_OFFSET as u64) + (offsets::reg(rd) as u64),
                    align: 3,
                    memory_index: 0,
                }));
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // LOAD Operations - Use imported MMU helper functions
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Ld {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                } // Write to x0 is no-op

                // Calculate virtual address: vaddr = regs[rs1] + imm
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Call read_u64(vaddr) -> i64
                builder.emit(I::Call(imports::READ_U64));

                // Store result to rd
                self.emit_store_reg(builder, rd);

                // Check for trap and exit if set
                self.emit_trap_check(builder, pc_offset);

                true
            }

            MicroOp::Lw {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                // Calculate vaddr
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Call read_u32(vaddr) -> i32
                builder.emit(I::Call(imports::READ_U32));

                // Sign-extend i32 to i64
                builder.emit(I::I64ExtendI32S);

                // Store to rd
                self.emit_store_reg(builder, rd);

                self.emit_trap_check(builder, pc_offset);
                true
            }

            MicroOp::Lwu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                builder.emit(I::Call(imports::READ_U32));

                // Zero-extend i32 to i64
                builder.emit(I::I64ExtendI32U);

                self.emit_store_reg(builder, rd);
                self.emit_trap_check(builder, pc_offset);
                true
            }

            MicroOp::Lh {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                builder.emit(I::Call(imports::READ_U16));

                // Sign-extend i16 (in i32) to i64
                // First sign-extend 16->32, then 32->64
                builder.emit(I::I32Const(16));
                builder.emit(I::I32Shl);
                builder.emit(I::I32Const(16));
                builder.emit(I::I32ShrS);
                builder.emit(I::I64ExtendI32S);

                self.emit_store_reg(builder, rd);
                self.emit_trap_check(builder, pc_offset);
                true
            }

            MicroOp::Lhu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                builder.emit(I::Call(imports::READ_U16));

                // Zero-extend i16 (in i32) to i64
                // Result is already zero-extended in i32, just extend to i64
                builder.emit(I::I64ExtendI32U);

                self.emit_store_reg(builder, rd);
                self.emit_trap_check(builder, pc_offset);
                true
            }

            MicroOp::Lb {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                builder.emit(I::Call(imports::READ_U8));

                // Sign-extend i8 (in i32) to i64
                builder.emit(I::I32Const(24));
                builder.emit(I::I32Shl);
                builder.emit(I::I32Const(24));
                builder.emit(I::I32ShrS);
                builder.emit(I::I64ExtendI32S);

                self.emit_store_reg(builder, rd);
                self.emit_trap_check(builder, pc_offset);
                true
            }

            MicroOp::Lbu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                if rd == 0 {
                    return true;
                }

                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                builder.emit(I::Call(imports::READ_U8));

                // Zero-extend i8 (in i32) to i64
                builder.emit(I::I64ExtendI32U);

                self.emit_store_reg(builder, rd);
                self.emit_trap_check(builder, pc_offset);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // STORE Operations - Use imported MMU helper functions
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Sd {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                // Calculate virtual address
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Get value to store
                self.emit_load_reg(builder, rs2);

                // Call write_u64(vaddr, value) -> i32
                builder.emit(I::Call(imports::WRITE_U64));

                // Check result (0 = success, non-zero = trap code)
                builder.emit(I::If(BlockType::Empty));
                self.emit_exit_trap(builder, pc_offset);
                builder.emit(I::End);

                true
            }

            MicroOp::Sw {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                // Calculate virtual address
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Get value to store (truncate to i32)
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);

                // Call write_u32(vaddr, value) -> i32
                builder.emit(I::Call(imports::WRITE_U32));

                // Check result
                builder.emit(I::If(BlockType::Empty));
                self.emit_exit_trap(builder, pc_offset);
                builder.emit(I::End);

                true
            }

            MicroOp::Sh {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                // Calculate virtual address
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Get value to store (truncate to i32, only low 16 bits matter)
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);

                // Call write_u16(vaddr, value) -> i32
                builder.emit(I::Call(imports::WRITE_U16));

                // Check result
                builder.emit(I::If(BlockType::Empty));
                self.emit_exit_trap(builder, pc_offset);
                builder.emit(I::End);

                true
            }

            MicroOp::Sb {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                // Calculate virtual address
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);

                // Get value to store (truncate to i32, only low 8 bits matter)
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I32WrapI64);

                // Call write_u8(vaddr, value) -> i32
                builder.emit(I::Call(imports::WRITE_U8));

                // Check result
                builder.emit(I::If(BlockType::Empty));
                self.emit_exit_trap(builder, pc_offset);
                builder.emit(I::End);

                true
            }

            // ═══════════════════════════════════════════════════════════════
            // Branch Instructions
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Beq {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                // if (regs[rs1] == regs[rs2]) pc = base_pc + pc_offset + imm
                // else pc = base_pc + pc_offset + insn_len

                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Eq);

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            MicroOp::Bne {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64Ne);

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            MicroOp::Blt {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64LtS); // Signed comparison

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            MicroOp::Bge {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64GeS); // Signed comparison

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            MicroOp::Bltu {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64LtU); // Unsigned comparison

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            MicroOp::Bgeu {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                self.emit_load_reg(builder, rs1);
                self.emit_load_reg(builder, rs2);
                builder.emit(I::I64GeU); // Unsigned comparison

                self.emit_branch_result(builder, pc_offset, imm, insn_len);
                true
            }

            // ═══════════════════════════════════════════════════════════════
            // Jump Instructions
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Jal {
                rd,
                imm,
                pc_offset,
                insn_len,
            } => {
                // rd = pc + insn_len (link address)
                // pc = pc + imm (jump target)

                if rd != 0 {
                    // Store link address: base_pc + pc_offset + insn_len
                    let link = (base_pc as i64) + (pc_offset as i64) + (insn_len as i64);
                    builder.emit(I::I64Const(link));
                    self.emit_store_reg(builder, rd);
                }

                // Return target PC offset
                let target = (pc_offset as i64) + imm;
                self.emit_exit_branch(builder, target as u16);

                true
            }

            MicroOp::Jalr {
                rd,
                rs1,
                imm,
                pc_offset,
                insn_len,
            } => {
                // rd = pc + insn_len
                // pc = (rs1 + imm) & ~1

                // Compute target: (rs1 + imm) & ~1
                self.emit_load_reg(builder, rs1);
                builder.emit(I::I64Const(imm));
                builder.emit(I::I64Add);
                builder.emit(I::I64Const(!1i64)); // Mask off LSB
                builder.emit(I::I64And);

                // Store in temp local (local 1 = temp_addr)
                let target_local = 1;
                builder.emit(I::LocalSet(target_local));

                // Store link address in rd
                if rd != 0 {
                    let link = (base_pc as i64) + (pc_offset as i64) + (insn_len as i64);
                    builder.emit(I::I64Const(link));
                    self.emit_store_reg(builder, rd);
                }

                // Return target - base_pc (so caller can compute absolute PC)
                builder.emit(I::LocalGet(target_local));
                builder.emit(I::I64Const(base_pc as i64));
                builder.emit(I::I64Sub);

                // Pack with EXIT_BRANCH and return
                // Since target can be any 64-bit value, we truncate to 32-bit offset
                builder.emit(I::I32WrapI64);
                builder.emit(I::I64ExtendI32U);
                let exit_code = (exit_codes::EXIT_BRANCH as i64) << 32;
                builder.emit(I::I64Const(exit_code));
                builder.emit(I::I64Or);
                builder.emit(I::Return);

                true
            }

            // ═══════════════════════════════════════════════════════════════
            // Operations that require interpreter fallback
            // ═══════════════════════════════════════════════════════════════

            MicroOp::Ecall { .. }
            | MicroOp::Ebreak { .. }
            | MicroOp::Mret { .. }
            | MicroOp::Sret { .. }
            | MicroOp::Wfi { .. }
            | MicroOp::SfenceVma { .. }
            | MicroOp::Fence
            | MicroOp::Csrrw { .. }
            | MicroOp::Csrrs { .. }
            | MicroOp::Csrrc { .. }
            | MicroOp::Csrrwi { .. }
            | MicroOp::Csrrsi { .. }
            | MicroOp::Csrrci { .. } => {
                // System operations always need interpreter
                false
            }

            // Atomic operations need interpreter (would require helper imports)
            MicroOp::LrW { .. }
            | MicroOp::LrD { .. }
            | MicroOp::ScW { .. }
            | MicroOp::ScD { .. }
            | MicroOp::AmoSwap { .. }
            | MicroOp::AmoAdd { .. }
            | MicroOp::AmoXor { .. }
            | MicroOp::AmoAnd { .. }
            | MicroOp::AmoOr { .. }
            | MicroOp::AmoMin { .. }
            | MicroOp::AmoMax { .. }
            | MicroOp::AmoMinu { .. }
            | MicroOp::AmoMaxu { .. } => false,
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Register Access Helpers
    // ═══════════════════════════════════════════════════════════════════════════

    /// Emit WASM to load a register value onto the stack.
    /// Uses fixed offset from base 0 into shared memory at JIT_STATE_OFFSET.
    fn emit_load_reg(&self, builder: &mut WasmModuleBuilder, reg: u8) {
        if reg == 0 {
            // x0 is always 0
            builder.emit(I::I64Const(0));
        } else {
            // Load from shared memory: JIT_STATE_OFFSET + reg * 8
            builder.emit(I::I32Const(0)); // base for memory access
            builder.emit(I::I64Load(MemArg {
                offset: (JIT_STATE_OFFSET as u64) + (offsets::reg(reg) as u64),
                align: 3, // 8-byte alignment
                memory_index: 0,
            }));
        }
    }

    /// Emit WASM to store top of stack into a register.
    /// Assumes value is on stack, consumes it.
    fn emit_store_reg(&self, builder: &mut WasmModuleBuilder, rd: u8) {
        if rd == 0 {
            // x0 is read-only, just drop the value
            builder.emit(I::Drop);
            return;
        }

        // Stack has: [value]
        // WASM store expects: [base_addr, value]
        // Use a local to reorder the stack

        let temp_local = 2; // Reserved in compile()
        builder.emit(I::LocalSet(temp_local));
        builder.emit(I::I32Const(0)); // base address
        builder.emit(I::LocalGet(temp_local));
        builder.emit(I::I64Store(MemArg {
            offset: (JIT_STATE_OFFSET as u64) + (offsets::reg(rd) as u64),
            align: 3,
            memory_index: 0,
        }));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Trap and Exit Helpers
    // ═══════════════════════════════════════════════════════════════════════════

    /// Emit code to check the trap flag and exit if set.
    fn emit_trap_check(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        // Load trap_pending from shared memory
        builder.emit(I::I32Const(0)); // base
        builder.emit(I::I32Load(MemArg {
            offset: (JIT_STATE_OFFSET as u64) + (offsets::TRAP_PENDING as u64),
            align: 2, // 4-byte alignment
            memory_index: 0,
        }));

        // If non-zero, exit with trap status
        builder.emit(I::If(BlockType::Empty));
        self.emit_exit_trap(builder, pc_offset);
        builder.emit(I::End);
    }

    /// Emit code to exit with trap status.
    /// Returns: (EXIT_TRAP << 32) | pc_offset
    fn emit_exit_trap(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        let exit_val = ((exit_codes::EXIT_TRAP as i64) << 32) | (pc_offset as i64);
        builder.emit(I::I64Const(exit_val));
        builder.emit(I::Return);
    }

    /// Emit code to exit for interpreter fallback.
    /// Returns: (EXIT_INTERPRETER << 32) | pc_offset
    #[allow(dead_code)]
    fn emit_exit_interpreter(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        let exit_val = ((exit_codes::EXIT_INTERPRETER as i64) << 32) | (pc_offset as i64);
        builder.emit(I::I64Const(exit_val));
        builder.emit(I::Return);
    }

    /// Emit code to exit normally at end of block.
    /// Returns: (EXIT_NORMAL << 32) | total_bytes
    fn emit_exit_normal(&self, builder: &mut WasmModuleBuilder, total_bytes: u16) {
        let exit_val = ((exit_codes::EXIT_NORMAL as i64) << 32) | (total_bytes as i64);
        builder.emit(I::I64Const(exit_val));
        // No return here - this is the end of the function
    }

    /// Emit code to exit after a branch.
    /// Returns: (EXIT_BRANCH << 32) | pc_offset
    fn emit_exit_branch(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        let exit_val = ((exit_codes::EXIT_BRANCH as i64) << 32) | (pc_offset as i64);
        builder.emit(I::I64Const(exit_val));
        builder.emit(I::Return);
    }

    /// Emit code to exit for interrupt check.
    /// Returns: (EXIT_INTERRUPT_CHECK << 32) | pc_offset
    fn emit_exit_interrupt_check(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        let exit_val = ((exit_codes::EXIT_INTERRUPT_CHECK as i64) << 32) | (pc_offset as i64);
        builder.emit(I::I64Const(exit_val));
        builder.emit(I::Return);
    }

    /// Emit interrupt pending check.
    ///
    /// Generates WASM code that:
    /// 1. Loads the interrupt_pending flag from shared memory
    /// 2. If non-zero, exits with EXIT_INTERRUPT_CHECK
    ///
    /// This allows the host to handle pending interrupts (timer, I/O, etc.)
    /// even during long-running JIT code execution.
    fn emit_interrupt_check(&self, builder: &mut WasmModuleBuilder, pc_offset: u16) {
        // Load interrupt_pending flag from shared state
        // Address = JIT_STATE_OFFSET + INTERRUPT_PENDING offset
        builder.emit(I::I32Const(JIT_STATE_OFFSET as i32));
        builder.emit(I::I32Load(MemArg {
            offset: offsets::INTERRUPT_PENDING as u64,
            align: 2, // 4-byte aligned
            memory_index: 0,
        }));
        // If non-zero, exit for interrupt handling
        builder.emit(I::If(BlockType::Empty));
        self.emit_exit_interrupt_check(builder, pc_offset);
        builder.emit(I::End);
    }

    /// Determine if an interrupt check should be inserted at a given op index.
    ///
    /// Interrupt checks are inserted:
    /// 1. At block entry for blocks >= threshold ops
    /// 2. Periodically every N ops for long blocks
    /// 3. Before backward branches (loop back-edges)
    fn should_check_interrupts(&self, block: &Block, op_index: usize, op: &MicroOp) -> bool {
        let block_len = block.len as usize;

        // Check at entry for long blocks
        if op_index == 0
            && self.config.interrupt_check_on_entry
            && block_len >= self.config.interrupt_check_block_threshold
        {
            return true;
        }

        // Periodic check every N ops
        if self.config.interrupt_check_interval > 0
            && op_index > 0
            && op_index % self.config.interrupt_check_interval == 0
            && block_len >= self.config.interrupt_check_block_threshold * 2
        {
            return true;
        }

        // Check before backward branches (loop back-edges)
        if is_backward_branch(op) {
            return true;
        }

        false
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Branch Helpers
    // ═══════════════════════════════════════════════════════════════════════════

    /// Emit common branch result pattern.
    /// Stack before: [comparison_result (i32, 0 or 1)]
    /// This generates an if/else that returns the appropriate PC offset.
    fn emit_branch_result(
        &self,
        builder: &mut WasmModuleBuilder,
        pc_offset: u16,
        imm: i64,
        insn_len: u8,
    ) {
        // Stack has comparison result (0 or 1)
        builder.emit(I::If(BlockType::Result(ValType::I64)));
        {
            // Taken: return (EXIT_BRANCH << 32) | (pc_offset + imm)
            let target = (pc_offset as i64) + imm;
            let exit_val = ((exit_codes::EXIT_BRANCH as i64) << 32) | (target & 0xFFFF_FFFF);
            builder.emit(I::I64Const(exit_val));
        }
        builder.emit(I::Else);
        {
            // Not taken: return (EXIT_BRANCH << 32) | (pc_offset + insn_len)
            let fallthrough = (pc_offset as i64) + (insn_len as i64);
            let exit_val = ((exit_codes::EXIT_BRANCH as i64) << 32) | (fallthrough & 0xFFFF_FFFF);
            builder.emit(I::I64Const(exit_val));
        }
        builder.emit(I::End);
        builder.emit(I::Return);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // TLB Fast-Path Helpers (Optional Optimization)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Emit inlined TLB fast-path for 64-bit memory load.
    /// Falls back to helper function on TLB miss.
    ///
    /// # Arguments
    /// * `builder` - WASM module builder
    /// * `vaddr_local` - Local variable index containing the virtual address
    /// * `rd` - Destination register for the loaded value
    /// * `pc_offset` - PC offset for trap reporting
    ///
    /// # TLB Lookup Algorithm
    /// 1. Compute VPN: vaddr >> 12
    /// 2. TLB index: VPN & TLB_MASK (64 entries)
    /// 3. Check TLB[index].valid
    /// 4. Check TLB[index].vpn == VPN
    /// 5. If hit: PA = (TLB[index].ppn << 12) | (vaddr & 0xFFF)
    /// 6. If miss: call helper
    ///
    /// # Control Flow
    /// Uses nested blocks:
    /// ```text
    /// block $after_load        ; outer block - jump here after any load path
    ///   block $slow_path       ; inner block - jump here on TLB miss
    ///     ; fast path checks
    ///     br_if $slow_path     ; on miss
    ///     ; TLB hit - direct load
    ///     br $after_load       ; skip slow path
    ///   end
    ///   ; slow path - call helper
    /// end
    /// ; continue execution
    /// ```
    fn emit_tlb_fast_path_load(
        &self,
        builder: &mut WasmModuleBuilder,
        vaddr_local: u32,
        rd: u8,
        pc_offset: u16,
    ) {
        use super::state::{
            tlb_entry, DRAM_BASE, JIT_STATE_OFFSET, TLB_ENTRY_SIZE, TLB_MASK, TLB_REGION_OFFSET,
        };
        use crate::shared_mem::HEADER_SIZE;

        // Allocate locals for TLB lookup
        let vpn_local = builder.add_local(ValType::I64);
        let tlb_idx_local = builder.add_local(ValType::I32);
        let entry_ptr_local = builder.add_local(ValType::I32);

        // Outer block - after_load: jump here after any load path completes
        builder.emit(I::Block(BlockType::Empty));
        {
            // Inner block - slow_path: jump here on TLB miss
            builder.emit(I::Block(BlockType::Empty));
            {
                // ─────────────────────────────────────────────────────────────
                // Step 1: Compute VPN = vaddr >> 12
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::LocalGet(vaddr_local));
                builder.emit(I::I64Const(12));
                builder.emit(I::I64ShrU);
                builder.emit(I::LocalTee(vpn_local));

                // ─────────────────────────────────────────────────────────────
                // Step 2: TLB index = VPN & TLB_MASK
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::I64Const(TLB_MASK as i64));
                builder.emit(I::I64And);
                builder.emit(I::I32WrapI64);
                builder.emit(I::LocalSet(tlb_idx_local));

                // ─────────────────────────────────────────────────────────────
                // Step 3: Compute TLB entry pointer
                // entry_ptr = JIT_STATE_OFFSET + TLB_REGION_OFFSET + (tlb_idx * TLB_ENTRY_SIZE)
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::LocalGet(tlb_idx_local));
                builder.emit(I::I32Const(TLB_ENTRY_SIZE as i32));
                builder.emit(I::I32Mul);
                builder.emit(I::I32Const((JIT_STATE_OFFSET + TLB_REGION_OFFSET) as i32));
                builder.emit(I::I32Add);
                builder.emit(I::LocalTee(entry_ptr_local));

                // ─────────────────────────────────────────────────────────────
                // Step 4: Check valid flag (byte at offset 20)
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::I32Load8U(MemArg {
                    offset: tlb_entry::VALID as u64,
                    align: 0,
                    memory_index: 0,
                }));
                builder.emit(I::I32Eqz);
                builder.emit(I::BrIf(0)); // br $slow_path if not valid

                // ─────────────────────────────────────────────────────────────
                // Step 5: Check VPN match
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::LocalGet(entry_ptr_local));
                builder.emit(I::I64Load(MemArg {
                    offset: tlb_entry::VPN as u64,
                    align: 3,
                    memory_index: 0,
                }));
                builder.emit(I::LocalGet(vpn_local));
                builder.emit(I::I64Ne);
                builder.emit(I::BrIf(0)); // br $slow_path if VPN mismatch

                // ─────────────────────────────────────────────────────────────
                // TLB HIT! Compute physical address:
                // PA = (ppn << 12) | (vaddr & 0xFFF)
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::LocalGet(entry_ptr_local));
                builder.emit(I::I64Load(MemArg {
                    offset: tlb_entry::PPN as u64,
                    align: 3,
                    memory_index: 0,
                }));
                builder.emit(I::I64Const(12));
                builder.emit(I::I64Shl);

                builder.emit(I::LocalGet(vaddr_local));
                builder.emit(I::I64Const(0xFFF));
                builder.emit(I::I64And);
                builder.emit(I::I64Or);

                // ─────────────────────────────────────────────────────────────
                // Convert PA to WASM memory offset:
                // WASM_offset = PA - DRAM_BASE
                // (HEADER_SIZE is added via load instruction offset)
                // ─────────────────────────────────────────────────────────────
                builder.emit(I::I64Const(DRAM_BASE as i64));
                builder.emit(I::I64Sub);
                builder.emit(I::I32WrapI64);

                // Direct memory load from DRAM region
                builder.emit(I::I64Load(MemArg {
                    offset: HEADER_SIZE as u64,
                    align: 3,
                    memory_index: 0,
                }));

                // Store result to rd
                self.emit_store_reg(builder, rd);

                // Skip slow path - branch to after_load (outer block, index 1)
                builder.emit(I::Br(1));
            }
            builder.emit(I::End); // end $slow_path

            // ─────────────────────────────────────────────────────────────────
            // Slow path: TLB miss - call helper function
            // ─────────────────────────────────────────────────────────────────
            builder.emit(I::LocalGet(vaddr_local));
            builder.emit(I::Call(imports::READ_U64));
            self.emit_store_reg(builder, rd);
            self.emit_trap_check(builder, pc_offset);
        }
        builder.emit(I::End); // end $after_load
    }

    /// Emit inlined TLB fast-path for 32-bit memory load.
    /// Falls back to helper function on TLB miss.
    fn emit_tlb_fast_path_load_w(
        &self,
        builder: &mut WasmModuleBuilder,
        vaddr_local: u32,
        rd: u8,
        pc_offset: u16,
        sign_extend: bool,
    ) {
        use super::state::{
            tlb_entry, DRAM_BASE, JIT_STATE_OFFSET, TLB_ENTRY_SIZE, TLB_MASK, TLB_REGION_OFFSET,
        };
        use crate::shared_mem::HEADER_SIZE;

        let vpn_local = builder.add_local(ValType::I64);
        let tlb_idx_local = builder.add_local(ValType::I32);
        let entry_ptr_local = builder.add_local(ValType::I32);

        // Outer block - after_load
        builder.emit(I::Block(BlockType::Empty));
        {
            // Inner block - slow_path
            builder.emit(I::Block(BlockType::Empty));
            {
                // Compute VPN
                builder.emit(I::LocalGet(vaddr_local));
                builder.emit(I::I64Const(12));
                builder.emit(I::I64ShrU);
                builder.emit(I::LocalTee(vpn_local));

                // TLB index
                builder.emit(I::I64Const(TLB_MASK as i64));
                builder.emit(I::I64And);
                builder.emit(I::I32WrapI64);
                builder.emit(I::LocalSet(tlb_idx_local));

                // Entry pointer
                builder.emit(I::LocalGet(tlb_idx_local));
                builder.emit(I::I32Const(TLB_ENTRY_SIZE as i32));
                builder.emit(I::I32Mul);
                builder.emit(I::I32Const((JIT_STATE_OFFSET + TLB_REGION_OFFSET) as i32));
                builder.emit(I::I32Add);
                builder.emit(I::LocalTee(entry_ptr_local));

                // Check valid
                builder.emit(I::I32Load8U(MemArg {
                    offset: tlb_entry::VALID as u64,
                    align: 0,
                    memory_index: 0,
                }));
                builder.emit(I::I32Eqz);
                builder.emit(I::BrIf(0)); // br $slow_path

                // Check VPN match
                builder.emit(I::LocalGet(entry_ptr_local));
                builder.emit(I::I64Load(MemArg {
                    offset: tlb_entry::VPN as u64,
                    align: 3,
                    memory_index: 0,
                }));
                builder.emit(I::LocalGet(vpn_local));
                builder.emit(I::I64Ne);
                builder.emit(I::BrIf(0)); // br $slow_path

                // TLB HIT - compute PA
                builder.emit(I::LocalGet(entry_ptr_local));
                builder.emit(I::I64Load(MemArg {
                    offset: tlb_entry::PPN as u64,
                    align: 3,
                    memory_index: 0,
                }));
                builder.emit(I::I64Const(12));
                builder.emit(I::I64Shl);
                builder.emit(I::LocalGet(vaddr_local));
                builder.emit(I::I64Const(0xFFF));
                builder.emit(I::I64And);
                builder.emit(I::I64Or);

                // Convert to WASM offset
                builder.emit(I::I64Const(DRAM_BASE as i64));
                builder.emit(I::I64Sub);
                builder.emit(I::I32WrapI64);

                // Load 32-bit value
                builder.emit(I::I32Load(MemArg {
                    offset: HEADER_SIZE as u64,
                    align: 2,
                    memory_index: 0,
                }));

                // Sign/zero extend to i64
                if sign_extend {
                    builder.emit(I::I64ExtendI32S);
                } else {
                    builder.emit(I::I64ExtendI32U);
                }

                self.emit_store_reg(builder, rd);
                builder.emit(I::Br(1)); // br $after_load
            }
            builder.emit(I::End); // end $slow_path

            // Slow path - call helper
            builder.emit(I::LocalGet(vaddr_local));
            builder.emit(I::Call(imports::READ_U32));
            if sign_extend {
                builder.emit(I::I64ExtendI32S);
            } else {
                builder.emit(I::I64ExtendI32U);
            }
            self.emit_store_reg(builder, rd);
            self.emit_trap_check(builder, pc_offset);
        }
        builder.emit(I::End); // end $after_load
    }
}

impl Default for JitCompiler {
    fn default() -> Self {
        Self::new(JitConfig::default())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper Functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if a MicroOp is a backward branch (negative immediate).
///
/// Backward branches are typically loop back-edges, which are good places
/// to insert interrupt checks to ensure loops can be preempted.
fn is_backward_branch(op: &MicroOp) -> bool {
    match op {
        MicroOp::Beq { imm, .. }
        | MicroOp::Bne { imm, .. }
        | MicroOp::Blt { imm, .. }
        | MicroOp::Bge { imm, .. }
        | MicroOp::Bltu { imm, .. }
        | MicroOp::Bgeu { imm, .. } => *imm < 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::block::Block;
    use crate::engine::microop::MicroOp;

    fn make_test_block(ops: &[MicroOp]) -> Block {
        let mut block = Block::new(0x8000_0000, 0x8000_0000, 0);
        for op in ops {
            block.push(*op, 4);
        }
        block
    }

    #[test]
    fn test_compile_addi() {
        let mut compiler = JitCompiler::default();
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 42,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 1,
                imm: 10,
            },
            MicroOp::Add {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Lui {
                rd: 4,
                imm: 0x12345000,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("Compiled {} ops to {} WASM bytes", block.len, bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_load_store() {
        let mut compiler = JitCompiler::default();

        // Block with load and store operations
        let block = make_test_block(&[
            // Load doubleword
            MicroOp::Ld {
                rd: 1,
                rs1: 0,
                imm: 0x8000_0000,
                pc_offset: 0,
            },
            // Store doubleword
            MicroOp::Sd {
                rs1: 0,
                rs2: 1,
                imm: 0x8000_0008,
                pc_offset: 4,
            },
            // Load word (sign-extended)
            MicroOp::Lw {
                rd: 2,
                rs1: 0,
                imm: 0x8000_0000,
                pc_offset: 8,
            },
            // Store word
            MicroOp::Sw {
                rs1: 0,
                rs2: 2,
                imm: 0x8000_0010,
                pc_offset: 12,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!(
                    "Compiled {} memory ops to {} WASM bytes",
                    block.len,
                    bytes.len()
                );
                // Verify WASM magic bytes
                assert_eq!(&bytes[0..4], b"\x00asm");
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_all_loads() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Ld {
                rd: 1,
                rs1: 0,
                imm: 0,
                pc_offset: 0,
            },
            MicroOp::Lw {
                rd: 2,
                rs1: 0,
                imm: 8,
                pc_offset: 4,
            },
            MicroOp::Lwu {
                rd: 3,
                rs1: 0,
                imm: 12,
                pc_offset: 8,
            },
            MicroOp::Lh {
                rd: 4,
                rs1: 0,
                imm: 16,
                pc_offset: 12,
            },
            MicroOp::Lhu {
                rd: 5,
                rs1: 0,
                imm: 18,
                pc_offset: 16,
            },
            MicroOp::Lb {
                rd: 6,
                rs1: 0,
                imm: 20,
                pc_offset: 20,
            },
            MicroOp::Lbu {
                rd: 7,
                rs1: 0,
                imm: 21,
                pc_offset: 24,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("All loads compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_all_stores() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Sd {
                rs1: 0,
                rs2: 1,
                imm: 0,
                pc_offset: 0,
            },
            MicroOp::Sw {
                rs1: 0,
                rs2: 2,
                imm: 8,
                pc_offset: 4,
            },
            MicroOp::Sh {
                rs1: 0,
                rs2: 3,
                imm: 12,
                pc_offset: 8,
            },
            MicroOp::Sb {
                rs1: 0,
                rs2: 4,
                imm: 14,
                pc_offset: 12,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("All stores compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_write_to_x0_is_nop() {
        let mut compiler = JitCompiler::default();

        // Writes to x0 should compile but be no-ops
        let block = make_test_block(&[
            MicroOp::Ld {
                rd: 0,
                rs1: 1,
                imm: 0,
                pc_offset: 0,
            },
            MicroOp::Addi {
                rd: 0,
                rs1: 1,
                imm: 42,
            },
            MicroOp::Add {
                rd: 0,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Lui { rd: 0, imm: 0x1000 },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!(
                    "x0 writes compiled to {} WASM bytes (should be minimal)",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_unsuitable_block() {
        let mut compiler = JitCompiler::default();

        // Block with ecall (not JIT-able)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 17,
                rs1: 0,
                imm: 93,
            }, // a7 = 93 (exit)
            MicroOp::Addi {
                rd: 10,
                rs1: 0,
                imm: 0,
            }, // a0 = 0
            MicroOp::Ecall { pc_offset: 8 },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Unsuitable => {}
            other => panic!("Expected Unsuitable, got {:?}", other),
        }
    }

    #[test]
    fn test_small_block_unsuitable() {
        let mut compiler = JitCompiler::default();

        // Block too small (less than min_block_size)
        let block = make_test_block(&[MicroOp::Addi {
            rd: 1,
            rs1: 0,
            imm: 1,
        }]);

        match compiler.compile(&block) {
            CompilationResult::Unsuitable => {}
            other => panic!("Expected Unsuitable (too small), got {:?}", other),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Branch Instruction Tests
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_compile_beq() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 10,
            },
            MicroOp::Beq {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("BEQ compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_bne() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 10,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Bne {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("BNE compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_blt_signed() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: -5,
            }, // negative
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            }, // positive
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Blt {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("BLT (signed) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_bge_signed() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 10,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Bge {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("BGE (signed) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_bltu_unsigned() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: -1,
            }, // Large unsigned value
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Bltu {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("BLTU (unsigned) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_bgeu_unsigned() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: -1,
            }, // Large unsigned value
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Bgeu {
                rs1: 1,
                rs2: 2,
                imm: 8,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("BGEU (unsigned) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Jump Instruction Tests
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_compile_jal() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Jal {
                rd: 1, // ra
                imm: 100,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("JAL compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_jal_no_link() {
        let mut compiler = JitCompiler::default();

        // JAL with rd=x0 (no link, just jump)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Jal {
                rd: 0, // No link
                imm: 100,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("JAL (no link) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_jalr() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 10,
                rs1: 0,
                imm: 0x1000,
            }, // base addr
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Jalr {
                rd: 1, // ra
                rs1: 10,
                imm: 4,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("JALR compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_jalr_no_link() {
        let mut compiler = JitCompiler::default();

        // JALR with rd=x0 (no link, just indirect jump)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 10,
                rs1: 0,
                imm: 0x1000,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Jalr {
                rd: 0, // No link
                rs1: 10,
                imm: 0,
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("JALR (no link) compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_all_branches() {
        let mut compiler = JitCompiler::default();

        // Test all branch types compile successfully
        for (name, op) in [
            (
                "BEQ",
                MicroOp::Beq {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
            (
                "BNE",
                MicroOp::Bne {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
            (
                "BLT",
                MicroOp::Blt {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
            (
                "BGE",
                MicroOp::Bge {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
            (
                "BLTU",
                MicroOp::Bltu {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
            (
                "BGEU",
                MicroOp::Bgeu {
                    rs1: 1,
                    rs2: 2,
                    imm: 8,
                    pc_offset: 12,
                    insn_len: 4,
                },
            ),
        ] {
            let block = make_test_block(&[
                MicroOp::Addi {
                    rd: 1,
                    rs1: 0,
                    imm: 5,
                },
                MicroOp::Addi {
                    rd: 2,
                    rs1: 0,
                    imm: 10,
                },
                MicroOp::Addi {
                    rd: 3,
                    rs1: 0,
                    imm: 0,
                },
                op,
            ]);

            match compiler.compile(&block) {
                CompilationResult::Success(bytes) => {
                    assert!(!bytes.is_empty());
                    println!("{} compiled to {} bytes", name, bytes.len());
                }
                other => panic!("{}: Expected Success, got {:?}", name, other),
            }
        }
    }

    #[test]
    fn test_branch_negative_offset() {
        let mut compiler = JitCompiler::default();

        // Test backward branch (negative imm)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Beq {
                rs1: 1,
                rs2: 2,
                imm: -8, // Backward branch
                pc_offset: 12,
                insn_len: 4,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("Backward branch compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // ALU Instruction Tests (Task 4.1)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_compile_register_immediate_alu() {
        let mut compiler = JitCompiler::default();

        // Test all register-immediate ALU operations
        let block = make_test_block(&[
            MicroOp::Xori {
                rd: 1,
                rs1: 0,
                imm: 0xFF,
            },
            MicroOp::Ori {
                rd: 2,
                rs1: 1,
                imm: 0x0F,
            },
            MicroOp::Andi {
                rd: 3,
                rs1: 2,
                imm: 0xF0,
            },
            MicroOp::Slti {
                rd: 4,
                rs1: 0,
                imm: 1,
            },
            MicroOp::Sltiu {
                rd: 5,
                rs1: 0,
                imm: 1,
            },
            MicroOp::Slli {
                rd: 6,
                rs1: 1,
                shamt: 4,
            },
            MicroOp::Srli {
                rd: 7,
                rs1: 1,
                shamt: 2,
            },
            MicroOp::Srai {
                rd: 8,
                rs1: 1,
                shamt: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!(
                    "Register-immediate ALU ops compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_register_register_alu() {
        let mut compiler = JitCompiler::default();

        // Test all register-register ALU operations
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 10,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 3,
            },
            MicroOp::Xor {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Or {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::And {
                rd: 5,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sll {
                rd: 6,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Srl {
                rd: 7,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sra {
                rd: 8,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Slt {
                rd: 9,
                rs1: 2,
                rs2: 1,
            },
            MicroOp::Sltu {
                rd: 10,
                rs1: 2,
                rs2: 1,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!(
                    "Register-register ALU ops compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_32bit_word_ops() {
        let mut compiler = JitCompiler::default();

        // Test all 32-bit word operations
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 100,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 5,
            },
            MicroOp::Addiw {
                rd: 3,
                rs1: 1,
                imm: 50,
            },
            MicroOp::Slliw {
                rd: 4,
                rs1: 1,
                shamt: 4,
            },
            MicroOp::Srliw {
                rd: 5,
                rs1: 1,
                shamt: 2,
            },
            MicroOp::Sraiw {
                rd: 6,
                rs1: 1,
                shamt: 2,
            },
            MicroOp::Addw {
                rd: 7,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Subw {
                rd: 8,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sllw {
                rd: 9,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Srlw {
                rd: 10,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sraw {
                rd: 11,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("32-bit word ops compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_multiply() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 7,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 6,
            },
            MicroOp::Mul {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Mulw {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("Multiply ops compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_divide() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 42,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 7,
            },
            MicroOp::Div {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Divu {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Rem {
                rd: 5,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Remu {
                rd: 6,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("Divide/remainder ops compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_divide_32bit() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 42,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 7,
            },
            MicroOp::Divw {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Divuw {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Remw {
                rd: 5,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Remuw {
                rd: 6,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!(
                    "32-bit divide/remainder ops compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_mulh_fallback() {
        let mut compiler = JitCompiler::default();

        // MULH should fall back to interpreter (needs 128-bit math)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 1000,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 1000,
            },
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 0,
            },
            MicroOp::Mulh {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Unsuitable => {}
            other => panic!("Expected Unsuitable for MULH, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_auipc() {
        let mut compiler = JitCompiler::default();

        let block = make_test_block(&[
            MicroOp::Auipc {
                rd: 1,
                imm: 0x12345000,
                pc_offset: 0,
            },
            MicroOp::Auipc {
                rd: 2,
                imm: 0x1000,
                pc_offset: 4,
            },
            MicroOp::Auipc {
                rd: 3,
                imm: -0x1000,
                pc_offset: 8,
            },
            MicroOp::Add {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!("AUIPC compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_x0_writes_are_nops() {
        let mut compiler = JitCompiler::default();

        // All writes to x0 should compile but be no-ops
        let block = make_test_block(&[
            MicroOp::Xori {
                rd: 0,
                rs1: 1,
                imm: 0xFF,
            },
            MicroOp::Ori {
                rd: 0,
                rs1: 1,
                imm: 0xFF,
            },
            MicroOp::Andi {
                rd: 0,
                rs1: 1,
                imm: 0xFF,
            },
            MicroOp::Slti {
                rd: 0,
                rs1: 1,
                imm: 0,
            },
            MicroOp::Sltiu {
                rd: 0,
                rs1: 1,
                imm: 0,
            },
            MicroOp::Slli {
                rd: 0,
                rs1: 1,
                shamt: 1,
            },
            MicroOp::Srli {
                rd: 0,
                rs1: 1,
                shamt: 1,
            },
            MicroOp::Srai {
                rd: 0,
                rs1: 1,
                shamt: 1,
            },
            MicroOp::Mul {
                rd: 0,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Div {
                rd: 0,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!(
                    "x0 writes compiled to {} WASM bytes (should be minimal)",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_shift_masking() {
        let mut compiler = JitCompiler::default();

        // Test that shifts use proper masking (6-bit for 64-bit, 5-bit for 32-bit)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 1,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 65,
            }, // 65 & 0x3F = 1 for 64-bit
            MicroOp::Addi {
                rd: 3,
                rs1: 0,
                imm: 33,
            }, // 33 & 0x1F = 1 for 32-bit
            MicroOp::Sll {
                rd: 4,
                rs1: 1,
                rs2: 2,
            }, // Should shift by 1
            MicroOp::Srl {
                rd: 5,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sra {
                rd: 6,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Sllw {
                rd: 7,
                rs1: 1,
                rs2: 3,
            }, // Should shift by 1
            MicroOp::Srlw {
                rd: 8,
                rs1: 1,
                rs2: 3,
            },
            MicroOp::Sraw {
                rd: 9,
                rs1: 1,
                rs2: 3,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!("Shift masking tests compiled to {} WASM bytes", bytes.len());
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_division_edge_cases() {
        let mut compiler = JitCompiler::default();

        // Division by zero should not cause JIT compilation to fail
        // (the generated code handles it)
        let block = make_test_block(&[
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 42,
            },
            MicroOp::Addi {
                rd: 2,
                rs1: 0,
                imm: 0,
            }, // divisor = 0
            MicroOp::Div {
                rd: 3,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Divu {
                rd: 4,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Rem {
                rd: 5,
                rs1: 1,
                rs2: 2,
            },
            MicroOp::Remu {
                rd: 6,
                rs1: 1,
                rs2: 2,
            },
        ]);

        match compiler.compile(&block) {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                println!(
                    "Division by zero handling compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // TLB Fast-Path Tests (Task 4.3)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_tlb_fast_path_load_compiles() {
        // Test that TLB fast-path code generation produces valid WASM
        let config = JitConfig {
            enable_tlb_fast_path: true,
            ..Default::default()
        };
        let compiler = JitCompiler::new(config);

        // Create a simple block builder to test fast-path emission
        let mut builder = crate::jit::encoder::WasmModuleBuilder::new();

        // Add required locals
        let vaddr_local = builder.add_local(ValType::I64);

        // Store test address in vaddr_local
        builder.emit(I::I64Const(0x8000_1000)); // Test virtual address
        builder.emit(I::LocalSet(vaddr_local));

        // Emit TLB fast-path
        compiler.emit_tlb_fast_path_load(&mut builder, vaddr_local, 1, 0);

        // Return success
        builder.emit(I::I64Const(0));

        // Build with imports (required for slow path helper)
        let bytes = builder.build_with_imports();

        // Verify WASM magic bytes
        assert_eq!(&bytes[0..4], b"\x00asm");
        println!("TLB fast-path load compiled to {} WASM bytes", bytes.len());
    }

    #[test]
    fn test_tlb_fast_path_load_w_compiles() {
        let config = JitConfig {
            enable_tlb_fast_path: true,
            ..Default::default()
        };
        let compiler = JitCompiler::new(config);

        let mut builder = crate::jit::encoder::WasmModuleBuilder::new();

        let vaddr_local = builder.add_local(ValType::I64);

        builder.emit(I::I64Const(0x8000_2000));
        builder.emit(I::LocalSet(vaddr_local));

        // Test sign-extended load
        compiler.emit_tlb_fast_path_load_w(&mut builder, vaddr_local, 2, 0, true);

        // Test zero-extended load
        compiler.emit_tlb_fast_path_load_w(&mut builder, vaddr_local, 3, 4, false);

        builder.emit(I::I64Const(0));

        let bytes = builder.build_with_imports();
        assert_eq!(&bytes[0..4], b"\x00asm");
        println!(
            "TLB fast-path load_w compiled to {} WASM bytes",
            bytes.len()
        );
    }

    #[test]
    fn test_tlb_fast_path_multiple_loads() {
        // Test multiple TLB fast-path loads in sequence
        let config = JitConfig {
            enable_tlb_fast_path: true,
            ..Default::default()
        };
        let compiler = JitCompiler::new(config);

        let mut builder = crate::jit::encoder::WasmModuleBuilder::new();

        // Simulate multiple load operations
        for i in 0..4 {
            let vaddr_local = builder.add_local(ValType::I64);
            builder.emit(I::I64Const(0x8000_0000 + (i * 8) as i64));
            builder.emit(I::LocalSet(vaddr_local));
            compiler.emit_tlb_fast_path_load(&mut builder, vaddr_local, (i + 1) as u8, i * 4);
        }

        builder.emit(I::I64Const(0));

        let bytes = builder.build_with_imports();
        assert_eq!(&bytes[0..4], b"\x00asm");
        println!(
            "Multiple TLB fast-path loads compiled to {} WASM bytes",
            bytes.len()
        );
        // Each fast-path adds significant code size
        assert!(bytes.len() > 200, "Expected substantial code for 4 fast-path loads");
    }

    #[test]
    fn test_tlb_config_defaults() {
        let config = JitConfig::default();
        assert!(!config.enable_tlb_fast_path, "TLB fast-path should be disabled by default");
        assert_eq!(
            config.tlb_fast_path_threshold, 100,
            "TLB fast-path threshold should be 100 by default"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Interrupt Check Tests (Task 4.4)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_interrupt_check_config_defaults() {
        let config = JitConfig::default();
        assert!(
            config.interrupt_check_on_entry,
            "Interrupt check on entry should be enabled by default"
        );
        assert_eq!(
            config.interrupt_check_interval, 32,
            "Interrupt check interval should be 32 by default"
        );
        assert_eq!(
            config.interrupt_check_block_threshold, 16,
            "Interrupt check block threshold should be 16 by default"
        );
    }

    #[test]
    fn test_interrupt_check_emitted_for_long_blocks() {
        // Create a block with enough ops to trigger entry interrupt check
        let config = JitConfig {
            min_block_size: 4,
            interrupt_check_on_entry: true,
            interrupt_check_block_threshold: 8, // Lower threshold for testing
            ..Default::default()
        };
        let mut compiler = JitCompiler::new(config);

        // Build a block with 10 simple ops (above threshold)
        let mut block = Block::new(0x8000_0000, 0x8000_0000, 0);
        for i in 0..10 {
            block.push(
                MicroOp::Addi {
                    rd: ((i % 31) + 1) as u8,
                    rs1: 0,
                    imm: i as i64,
                },
                4,
            );
        }

        let result = compiler.compile(&block);
        match result {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                // Verify WASM magic bytes
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!(
                    "Long block (10 ops) with interrupt check compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_interrupt_check_not_emitted_for_short_blocks() {
        // Create a block too short to trigger interrupt check
        let config = JitConfig {
            min_block_size: 2,
            interrupt_check_on_entry: true,
            interrupt_check_block_threshold: 16, // Default threshold
            ..Default::default()
        };
        let mut compiler_with_check = JitCompiler::new(config.clone());

        // Also compile without interrupt check for size comparison
        let config_no_check = JitConfig {
            interrupt_check_on_entry: false,
            interrupt_check_interval: 0,
            ..config
        };
        let mut compiler_no_check = JitCompiler::new(config_no_check);

        // Build a short block (below threshold)
        let mut block = Block::new(0x8000_0000, 0x8000_0000, 0);
        for i in 0..4 {
            block.push(
                MicroOp::Addi {
                    rd: ((i % 31) + 1) as u8,
                    rs1: 0,
                    imm: i as i64,
                },
                4,
            );
        }

        let result_with = compiler_with_check.compile(&block);
        let result_without = compiler_no_check.compile(&block);

        match (result_with, result_without) {
            (CompilationResult::Success(bytes_with), CompilationResult::Success(bytes_without)) => {
                // Short blocks should have same size (no interrupt check added)
                assert_eq!(
                    bytes_with.len(),
                    bytes_without.len(),
                    "Short blocks should not have interrupt checks added"
                );
                println!(
                    "Short block (4 ops) compiled to {} WASM bytes (no interrupt check)",
                    bytes_with.len()
                );
            }
            other => panic!("Expected both Success, got {:?}", other),
        }
    }

    #[test]
    fn test_interrupt_check_on_backward_branch() {
        // Test that backward branches trigger interrupt checks
        let config = JitConfig {
            min_block_size: 2,
            interrupt_check_on_entry: false, // Disable entry check
            interrupt_check_interval: 0,     // Disable periodic check
            interrupt_check_block_threshold: 100, // High threshold to avoid entry check
            ..Default::default()
        };
        let mut compiler = JitCompiler::new(config);

        // Build a block with a backward branch (loop back-edge)
        let mut block = Block::new(0x8000_0000, 0x8000_0000, 0);
        block.push(
            MicroOp::Addi {
                rd: 1,
                rs1: 0,
                imm: 1,
            },
            4,
        );
        // Backward branch (negative immediate)
        block.push(
            MicroOp::Beq {
                rs1: 1,
                rs2: 0,
                imm: -4, // Jump back
                pc_offset: 4,
                insn_len: 4,
            },
            4,
        );

        let result = compiler.compile(&block);
        match result {
            CompilationResult::Success(bytes) => {
                assert!(!bytes.is_empty());
                assert_eq!(&bytes[0..4], b"\x00asm");
                println!(
                    "Block with backward branch compiled to {} WASM bytes",
                    bytes.len()
                );
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_is_backward_branch() {
        // Test the is_backward_branch helper function
        assert!(is_backward_branch(&MicroOp::Beq {
            rs1: 1,
            rs2: 2,
            imm: -8,
            pc_offset: 12,
            insn_len: 4,
        }));
        assert!(is_backward_branch(&MicroOp::Bne {
            rs1: 1,
            rs2: 2,
            imm: -4,
            pc_offset: 8,
            insn_len: 4,
        }));
        assert!(!is_backward_branch(&MicroOp::Beq {
            rs1: 1,
            rs2: 2,
            imm: 8, // Forward branch
            pc_offset: 12,
            insn_len: 4,
        }));
        assert!(!is_backward_branch(&MicroOp::Addi {
            rd: 1,
            rs1: 0,
            imm: 42,
        }));
    }
}

