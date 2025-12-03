//! JIT Worker Thread Communication
//!
//! Handles message passing between main thread and JIT compilation worker.
//! This module provides serializable message types for compile requests/responses
//! and MicroOp serialization for cross-thread/cross-process transfer.

use super::types::CompilationResult;
use crate::engine::block::Block;
use crate::engine::microop::MicroOp;
use serde::{Deserialize, Serialize};

/// Message sent to JIT worker to request compilation.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CompileRequest {
    /// Starting PC of the block
    pub pc: u64,
    /// Physical address (for cache key)
    pub pa: u64,
    /// Serialized MicroOps
    pub ops: Vec<SerializedMicroOp>,
    /// Block byte length
    pub byte_len: u16,
}

/// Message received from JIT worker with compilation result.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CompileResponse {
    /// Starting PC of the compiled block
    pub pc: u64,
    /// Result status
    pub status: CompileStatus,
    /// Compiled WASM bytes (if successful)
    #[serde(with = "serde_bytes")]
    pub wasm_bytes: Vec<u8>,
    /// Compilation time in microseconds
    pub compile_time_us: u64,
}

/// Compilation status returned by the worker.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileStatus {
    /// Compilation succeeded, WASM bytes are valid
    Success,
    /// Block is unsuitable for JIT (e.g., MMIO-heavy, unsupported ops)
    Unsuitable,
    /// Compilation error (internal bug)
    Error,
}

impl From<&CompilationResult> for CompileStatus {
    fn from(result: &CompilationResult) -> Self {
        match result {
            CompilationResult::Success(_) => CompileStatus::Success,
            CompilationResult::Unsuitable => CompileStatus::Unsuitable,
            CompilationResult::Error(_) => CompileStatus::Error,
        }
    }
}

/// Serializable MicroOp representation for worker thread transfer.
///
/// We use a compact tagged representation since the `MicroOp` enum itself
/// cannot be directly serialized across thread boundaries efficiently.
///
/// ## Tag Encoding Strategy
///
/// Tags are organized by instruction category for easy extension:
/// - **0-9**: ALU register-immediate operations
/// - **10-19**: ALU register-register operations
/// - **20-29**: 32-bit ALU operations (*W variants)
/// - **30-39**: M-Extension (multiply/divide)
/// - **40-49**: Load operations
/// - **50-59**: Store operations
/// - **60-69**: Branch operations
/// - **70-79**: Jump operations
/// - **80-89**: CSR operations
/// - **90-99**: System operations
/// - **100-109**: Atomic operations (LR/SC)
/// - **110-119**: AMO operations
/// - **254**: Fence (no-op marker)
/// - **255**: Unknown/unsupported
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SerializedMicroOp {
    /// Tag identifying the operation type
    pub tag: u8,
    /// Packed operand data (up to 16 bytes)
    pub data: [u8; 16],
}

impl SerializedMicroOp {
    /// Serialize a MicroOp for worker transfer.
    pub fn from_microop(op: &MicroOp) -> Self {
        let mut data = [0u8; 16];

        let tag = match op {
            // ═══════════════════════════════════════════════════════════════════
            // ALU Register-Immediate (tags 0-9)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Addi { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                0
            }
            MicroOp::Xori { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                1
            }
            MicroOp::Ori { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                2
            }
            MicroOp::Andi { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                3
            }
            MicroOp::Slti { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                4
            }
            MicroOp::Sltiu { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                5
            }
            MicroOp::Slli { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                6
            }
            MicroOp::Srli { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                7
            }
            MicroOp::Srai { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                8
            }

            // ═══════════════════════════════════════════════════════════════════
            // ALU Register-Register (tags 10-19)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Add { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                10
            }
            MicroOp::Sub { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                11
            }
            MicroOp::Xor { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                12
            }
            MicroOp::Or { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                13
            }
            MicroOp::And { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                14
            }
            MicroOp::Sll { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                15
            }
            MicroOp::Srl { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                16
            }
            MicroOp::Sra { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                17
            }
            MicroOp::Slt { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                18
            }
            MicroOp::Sltu { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                19
            }

            // ═══════════════════════════════════════════════════════════════════
            // 32-bit ALU (*W variants) (tags 20-29)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Addiw { rd, rs1, imm } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..6].copy_from_slice(&imm.to_le_bytes());
                20
            }
            MicroOp::Slliw { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                21
            }
            MicroOp::Srliw { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                22
            }
            MicroOp::Sraiw { rd, rs1, shamt } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *shamt;
                23
            }
            MicroOp::Addw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                24
            }
            MicroOp::Subw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                25
            }
            MicroOp::Sllw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                26
            }
            MicroOp::Srlw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                27
            }
            MicroOp::Sraw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                28
            }

            // ═══════════════════════════════════════════════════════════════════
            // M-Extension (tags 30-39)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Mul { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                30
            }
            MicroOp::Mulh { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                31
            }
            MicroOp::Mulhsu { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                32
            }
            MicroOp::Mulhu { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                33
            }
            MicroOp::Div { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                34
            }
            MicroOp::Divu { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                35
            }
            MicroOp::Rem { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                36
            }
            MicroOp::Remu { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                37
            }
            MicroOp::Mulw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                38
            }
            MicroOp::Divw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                39
            }
            // Using tag 129 for Divuw (overflow from 30-39 range)
            MicroOp::Divuw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                129
            }
            // Using tag 130 for Remw
            MicroOp::Remw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                130
            }
            // Using tag 131 for Remuw
            MicroOp::Remuw { rd, rs1, rs2 } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                131
            }

            // ═══════════════════════════════════════════════════════════════════
            // Upper Immediate (tags 40-49 for LUI/AUIPC)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Lui { rd, imm } => {
                data[0] = *rd;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                40
            }
            MicroOp::Auipc { rd, imm, pc_offset } => {
                data[0] = *rd;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                41
            }

            // ═══════════════════════════════════════════════════════════════════
            // Load Operations (tags 50-59)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Lb {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                50
            }
            MicroOp::Lbu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                51
            }
            MicroOp::Lh {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                52
            }
            MicroOp::Lhu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                53
            }
            MicroOp::Lw {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                54
            }
            MicroOp::Lwu {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                55
            }
            MicroOp::Ld {
                rd,
                rs1,
                imm,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                56
            }

            // ═══════════════════════════════════════════════════════════════════
            // Store Operations (tags 60-69)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Sb {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                60
            }
            MicroOp::Sh {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                61
            }
            MicroOp::Sw {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                62
            }
            MicroOp::Sd {
                rs1,
                rs2,
                imm,
                pc_offset,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                63
            }

            // ═══════════════════════════════════════════════════════════════════
            // Branch Operations (tags 70-79)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Beq {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                70
            }
            MicroOp::Bne {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                71
            }
            MicroOp::Blt {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                72
            }
            MicroOp::Bge {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                73
            }
            MicroOp::Bltu {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                74
            }
            MicroOp::Bgeu {
                rs1,
                rs2,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rs1;
                data[1] = *rs2;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                75
            }

            // ═══════════════════════════════════════════════════════════════════
            // Jump Operations (tags 80-89)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Jal {
                rd,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rd;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                80
            }
            MicroOp::Jalr {
                rd,
                rs1,
                imm,
                pc_offset,
                insn_len,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..10].copy_from_slice(&imm.to_le_bytes());
                data[10..12].copy_from_slice(&pc_offset.to_le_bytes());
                data[12] = *insn_len;
                81
            }

            // ═══════════════════════════════════════════════════════════════════
            // CSR Operations (tags 90-99)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Csrrw {
                rd,
                rs1,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                90
            }
            MicroOp::Csrrs {
                rd,
                rs1,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                91
            }
            MicroOp::Csrrc {
                rd,
                rs1,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                92
            }
            MicroOp::Csrrwi {
                rd,
                zimm,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *zimm;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                93
            }
            MicroOp::Csrrsi {
                rd,
                zimm,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *zimm;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                94
            }
            MicroOp::Csrrci {
                rd,
                zimm,
                csr,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *zimm;
                data[2..4].copy_from_slice(&csr.to_le_bytes());
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                95
            }

            // ═══════════════════════════════════════════════════════════════════
            // System Operations (tags 100-109)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::Ecall { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                100
            }
            MicroOp::Ebreak { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                101
            }
            MicroOp::Mret { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                102
            }
            MicroOp::Sret { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                103
            }
            MicroOp::Wfi { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                104
            }
            MicroOp::SfenceVma { pc_offset } => {
                data[0..2].copy_from_slice(&pc_offset.to_le_bytes());
                105
            }
            MicroOp::Fence => 254,

            // ═══════════════════════════════════════════════════════════════════
            // Atomic Operations - LR/SC (tags 110-119)
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::LrW { rd, rs1, pc_offset } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..4].copy_from_slice(&pc_offset.to_le_bytes());
                110
            }
            MicroOp::LrD { rd, rs1, pc_offset } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2..4].copy_from_slice(&pc_offset.to_le_bytes());
                111
            }
            MicroOp::ScW {
                rd,
                rs1,
                rs2,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3..5].copy_from_slice(&pc_offset.to_le_bytes());
                112
            }
            MicroOp::ScD {
                rd,
                rs1,
                rs2,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3..5].copy_from_slice(&pc_offset.to_le_bytes());
                113
            }

            // ═══════════════════════════════════════════════════════════════════
            // AMO Operations (tags 120-139)
            // Layout: [rd, rs1, rs2, is_word, pc_offset(2)]
            // ═══════════════════════════════════════════════════════════════════
            MicroOp::AmoSwap {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                120
            }
            MicroOp::AmoAdd {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                121
            }
            MicroOp::AmoXor {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                122
            }
            MicroOp::AmoAnd {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                123
            }
            MicroOp::AmoOr {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                124
            }
            MicroOp::AmoMin {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                125
            }
            MicroOp::AmoMax {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                126
            }
            MicroOp::AmoMinu {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                127
            }
            MicroOp::AmoMaxu {
                rd,
                rs1,
                rs2,
                is_word,
                pc_offset,
            } => {
                data[0] = *rd;
                data[1] = *rs1;
                data[2] = *rs2;
                data[3] = *is_word as u8;
                data[4..6].copy_from_slice(&pc_offset.to_le_bytes());
                128
            }
        };

        Self { tag, data }
    }

    /// Deserialize back to MicroOp.
    ///
    /// Returns `None` if the tag is unknown or data is malformed.
    pub fn to_microop(&self) -> Option<MicroOp> {
        match self.tag {
            // ALU Register-Immediate (tags 0-9)
            0 => Some(MicroOp::Addi {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            1 => Some(MicroOp::Xori {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            2 => Some(MicroOp::Ori {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            3 => Some(MicroOp::Andi {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            4 => Some(MicroOp::Slti {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            5 => Some(MicroOp::Sltiu {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            6 => Some(MicroOp::Slli {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),
            7 => Some(MicroOp::Srli {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),
            8 => Some(MicroOp::Srai {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),

            // ALU Register-Register (tags 10-19)
            10 => Some(MicroOp::Add {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            11 => Some(MicroOp::Sub {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            12 => Some(MicroOp::Xor {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            13 => Some(MicroOp::Or {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            14 => Some(MicroOp::And {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            15 => Some(MicroOp::Sll {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            16 => Some(MicroOp::Srl {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            17 => Some(MicroOp::Sra {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            18 => Some(MicroOp::Slt {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            19 => Some(MicroOp::Sltu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),

            // 32-bit ALU (*W variants) (tags 20-29)
            20 => Some(MicroOp::Addiw {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i32::from_le_bytes(self.data[2..6].try_into().ok()?),
            }),
            21 => Some(MicroOp::Slliw {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),
            22 => Some(MicroOp::Srliw {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),
            23 => Some(MicroOp::Sraiw {
                rd: self.data[0],
                rs1: self.data[1],
                shamt: self.data[2],
            }),
            24 => Some(MicroOp::Addw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            25 => Some(MicroOp::Subw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            26 => Some(MicroOp::Sllw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            27 => Some(MicroOp::Srlw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            28 => Some(MicroOp::Sraw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),

            // M-Extension (tags 30-39 + overflow)
            30 => Some(MicroOp::Mul {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            31 => Some(MicroOp::Mulh {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            32 => Some(MicroOp::Mulhsu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            33 => Some(MicroOp::Mulhu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            34 => Some(MicroOp::Div {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            35 => Some(MicroOp::Divu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            36 => Some(MicroOp::Rem {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            37 => Some(MicroOp::Remu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            38 => Some(MicroOp::Mulw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            39 => Some(MicroOp::Divw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            129 => Some(MicroOp::Divuw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            130 => Some(MicroOp::Remw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),
            131 => Some(MicroOp::Remuw {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
            }),

            // Upper Immediate (tags 40-49)
            40 => Some(MicroOp::Lui {
                rd: self.data[0],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
            }),
            41 => Some(MicroOp::Auipc {
                rd: self.data[0],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),

            // Load Operations (tags 50-59)
            50 => Some(MicroOp::Lb {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            51 => Some(MicroOp::Lbu {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            52 => Some(MicroOp::Lh {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            53 => Some(MicroOp::Lhu {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            54 => Some(MicroOp::Lw {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            55 => Some(MicroOp::Lwu {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            56 => Some(MicroOp::Ld {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),

            // Store Operations (tags 60-69)
            60 => Some(MicroOp::Sb {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            61 => Some(MicroOp::Sh {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            62 => Some(MicroOp::Sw {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),
            63 => Some(MicroOp::Sd {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
            }),

            // Branch Operations (tags 70-79)
            70 => Some(MicroOp::Beq {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            71 => Some(MicroOp::Bne {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            72 => Some(MicroOp::Blt {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            73 => Some(MicroOp::Bge {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            74 => Some(MicroOp::Bltu {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            75 => Some(MicroOp::Bgeu {
                rs1: self.data[0],
                rs2: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),

            // Jump Operations (tags 80-89)
            80 => Some(MicroOp::Jal {
                rd: self.data[0],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),
            81 => Some(MicroOp::Jalr {
                rd: self.data[0],
                rs1: self.data[1],
                imm: i64::from_le_bytes(self.data[2..10].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[10..12].try_into().ok()?),
                insn_len: self.data[12],
            }),

            // CSR Operations (tags 90-99)
            90 => Some(MicroOp::Csrrw {
                rd: self.data[0],
                rs1: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            91 => Some(MicroOp::Csrrs {
                rd: self.data[0],
                rs1: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            92 => Some(MicroOp::Csrrc {
                rd: self.data[0],
                rs1: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            93 => Some(MicroOp::Csrrwi {
                rd: self.data[0],
                zimm: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            94 => Some(MicroOp::Csrrsi {
                rd: self.data[0],
                zimm: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            95 => Some(MicroOp::Csrrci {
                rd: self.data[0],
                zimm: self.data[1],
                csr: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),

            // System Operations (tags 100-109)
            100 => Some(MicroOp::Ecall {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),
            101 => Some(MicroOp::Ebreak {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),
            102 => Some(MicroOp::Mret {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),
            103 => Some(MicroOp::Sret {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),
            104 => Some(MicroOp::Wfi {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),
            105 => Some(MicroOp::SfenceVma {
                pc_offset: u16::from_le_bytes(self.data[0..2].try_into().ok()?),
            }),

            // Atomic Operations - LR/SC (tags 110-119)
            110 => Some(MicroOp::LrW {
                rd: self.data[0],
                rs1: self.data[1],
                pc_offset: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
            }),
            111 => Some(MicroOp::LrD {
                rd: self.data[0],
                rs1: self.data[1],
                pc_offset: u16::from_le_bytes(self.data[2..4].try_into().ok()?),
            }),
            112 => Some(MicroOp::ScW {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                pc_offset: u16::from_le_bytes(self.data[3..5].try_into().ok()?),
            }),
            113 => Some(MicroOp::ScD {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                pc_offset: u16::from_le_bytes(self.data[3..5].try_into().ok()?),
            }),

            // AMO Operations (tags 120-139)
            120 => Some(MicroOp::AmoSwap {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            121 => Some(MicroOp::AmoAdd {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            122 => Some(MicroOp::AmoXor {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            123 => Some(MicroOp::AmoAnd {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            124 => Some(MicroOp::AmoOr {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            125 => Some(MicroOp::AmoMin {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            126 => Some(MicroOp::AmoMax {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            127 => Some(MicroOp::AmoMinu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),
            128 => Some(MicroOp::AmoMaxu {
                rd: self.data[0],
                rs1: self.data[1],
                rs2: self.data[2],
                is_word: self.data[3] != 0,
                pc_offset: u16::from_le_bytes(self.data[4..6].try_into().ok()?),
            }),

            // Fence (tag 254)
            254 => Some(MicroOp::Fence),

            // Unknown/unsupported
            _ => None,
        }
    }
}

impl CompileRequest {
    /// Create a compile request from a Block.
    pub fn from_block(block: &Block) -> Self {
        let ops = block
            .ops()
            .iter()
            .map(SerializedMicroOp::from_microop)
            .collect();

        Self {
            pc: block.start_pc,
            pa: block.start_pa,
            ops,
            byte_len: block.byte_len,
        }
    }
}

impl CompileResponse {
    /// Create a successful response.
    pub fn success(pc: u64, wasm_bytes: Vec<u8>, compile_time_us: u64) -> Self {
        Self {
            pc,
            status: CompileStatus::Success,
            wasm_bytes,
            compile_time_us,
        }
    }

    /// Create an unsuitable response.
    pub fn unsuitable(pc: u64) -> Self {
        Self {
            pc,
            status: CompileStatus::Unsuitable,
            wasm_bytes: Vec::new(),
            compile_time_us: 0,
        }
    }

    /// Create an error response.
    pub fn error(pc: u64) -> Self {
        Self {
            pc,
            status: CompileStatus::Error,
            wasm_bytes: Vec::new(),
            compile_time_us: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test round-trip serialization for ALU operations.
    #[test]
    fn test_alu_roundtrip() {
        let ops = vec![
            MicroOp::Addi {
                rd: 1,
                rs1: 2,
                imm: -100,
            },
            MicroOp::Add {
                rd: 3,
                rs1: 4,
                rs2: 5,
            },
            MicroOp::Sub {
                rd: 6,
                rs1: 7,
                rs2: 8,
            },
            MicroOp::Xori {
                rd: 9,
                rs1: 10,
                imm: 0xDEADBEEF,
            },
            MicroOp::Slli {
                rd: 11,
                rs1: 12,
                shamt: 5,
            },
            MicroOp::Lui {
                rd: 13,
                imm: 0x12345 << 12,
            },
        ];

        for op in ops {
            let serialized = SerializedMicroOp::from_microop(&op);
            let deserialized = serialized.to_microop().expect("should deserialize");
            assert!(
                matches_microop(&op, &deserialized),
                "Round-trip failed for {:?}",
                op
            );
        }
    }

    /// Test round-trip for branch/jump operations.
    #[test]
    fn test_branch_roundtrip() {
        let ops = vec![
            MicroOp::Beq {
                rs1: 1,
                rs2: 2,
                imm: 100,
                pc_offset: 4,
                insn_len: 4,
            },
            MicroOp::Bne {
                rs1: 3,
                rs2: 4,
                imm: -200,
                pc_offset: 8,
                insn_len: 2,
            },
            MicroOp::Jal {
                rd: 1,
                imm: 1000,
                pc_offset: 12,
                insn_len: 4,
            },
            MicroOp::Jalr {
                rd: 1,
                rs1: 5,
                imm: 0,
                pc_offset: 16,
                insn_len: 4,
            },
        ];

        for op in ops {
            let serialized = SerializedMicroOp::from_microop(&op);
            let deserialized = serialized.to_microop().expect("should deserialize");
            assert!(
                matches_microop(&op, &deserialized),
                "Round-trip failed for {:?}",
                op
            );
        }
    }

    /// Test round-trip for load/store operations.
    #[test]
    fn test_memory_roundtrip() {
        let ops = vec![
            MicroOp::Ld {
                rd: 1,
                rs1: 2,
                imm: 8,
                pc_offset: 0,
            },
            MicroOp::Sd {
                rs1: 3,
                rs2: 4,
                imm: -16,
                pc_offset: 4,
            },
            MicroOp::Lb {
                rd: 5,
                rs1: 6,
                imm: 1,
                pc_offset: 8,
            },
            MicroOp::Lw {
                rd: 7,
                rs1: 8,
                imm: 4,
                pc_offset: 12,
            },
        ];

        for op in ops {
            let serialized = SerializedMicroOp::from_microop(&op);
            let deserialized = serialized.to_microop().expect("should deserialize");
            assert!(
                matches_microop(&op, &deserialized),
                "Round-trip failed for {:?}",
                op
            );
        }
    }

    /// Test round-trip for system operations.
    #[test]
    fn test_system_roundtrip() {
        let ops = vec![
            MicroOp::Ecall { pc_offset: 0 },
            MicroOp::Ebreak { pc_offset: 4 },
            MicroOp::Mret { pc_offset: 8 },
            MicroOp::Fence,
            MicroOp::Csrrw {
                rd: 1,
                rs1: 2,
                csr: 0x300,
                pc_offset: 12,
            },
        ];

        for op in ops {
            let serialized = SerializedMicroOp::from_microop(&op);
            let deserialized = serialized.to_microop().expect("should deserialize");
            assert!(
                matches_microop(&op, &deserialized),
                "Round-trip failed for {:?}",
                op
            );
        }
    }

    /// Test round-trip for atomic operations.
    #[test]
    fn test_atomic_roundtrip() {
        let ops = vec![
            MicroOp::LrW {
                rd: 1,
                rs1: 2,
                pc_offset: 0,
            },
            MicroOp::ScD {
                rd: 3,
                rs1: 4,
                rs2: 5,
                pc_offset: 4,
            },
            MicroOp::AmoAdd {
                rd: 6,
                rs1: 7,
                rs2: 8,
                is_word: true,
                pc_offset: 8,
            },
            MicroOp::AmoSwap {
                rd: 9,
                rs1: 10,
                rs2: 11,
                is_word: false,
                pc_offset: 12,
            },
        ];

        for op in ops {
            let serialized = SerializedMicroOp::from_microop(&op);
            let deserialized = serialized.to_microop().expect("should deserialize");
            assert!(
                matches_microop(&op, &deserialized),
                "Round-trip failed for {:?}",
                op
            );
        }
    }

    /// Test bincode serialization of CompileRequest.
    #[test]
    fn test_compile_request_bincode() {
        let request = CompileRequest {
            pc: 0x8000_0000,
            pa: 0x8000_0000,
            ops: vec![
                SerializedMicroOp::from_microop(&MicroOp::Addi {
                    rd: 1,
                    rs1: 0,
                    imm: 5,
                }),
                SerializedMicroOp::from_microop(&MicroOp::Add {
                    rd: 2,
                    rs1: 1,
                    rs2: 1,
                }),
            ],
            byte_len: 8,
        };

        let encoded = bincode::serialize(&request).expect("should serialize");
        let decoded: CompileRequest = bincode::deserialize(&encoded).expect("should deserialize");

        assert_eq!(decoded.pc, request.pc);
        assert_eq!(decoded.pa, request.pa);
        assert_eq!(decoded.byte_len, request.byte_len);
        assert_eq!(decoded.ops.len(), request.ops.len());
    }

    /// Test bincode serialization of CompileResponse.
    #[test]
    fn test_compile_response_bincode() {
        let response = CompileResponse::success(0x8000_0000, vec![0x00, 0x61, 0x73, 0x6d], 1234);

        let encoded = bincode::serialize(&response).expect("should serialize");
        let decoded: CompileResponse = bincode::deserialize(&encoded).expect("should deserialize");

        assert_eq!(decoded.pc, response.pc);
        assert_eq!(decoded.status, CompileStatus::Success);
        assert_eq!(decoded.wasm_bytes, response.wasm_bytes);
        assert_eq!(decoded.compile_time_us, 1234);
    }

    /// Helper to compare MicroOps (handles Copy limitation).
    fn matches_microop(a: &MicroOp, b: &MicroOp) -> bool {
        // Use debug representation for comparison
        format!("{:?}", a) == format!("{:?}", b)
    }
}

