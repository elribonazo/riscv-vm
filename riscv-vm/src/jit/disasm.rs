//! WAT (WebAssembly Text) disassembler for debugging JIT output.
//!
//! This module generates human-readable WAT representations of JIT-compiled
//! blocks. Useful for debugging JIT issues and understanding generated code.

use crate::engine::block::Block;
use crate::engine::microop::MicroOp;

/// Configuration for WAT output.
#[derive(Debug, Clone)]
pub struct DisasmConfig {
    /// Include comments with original RISC-V instructions
    pub include_source: bool,
    /// Pretty-print with indentation
    pub pretty: bool,
    /// Include instruction addresses
    pub include_addresses: bool,
}

impl Default for DisasmConfig {
    fn default() -> Self {
        Self {
            include_source: true,
            pretty: true,
            include_addresses: true,
        }
    }
}

/// Generate human-readable WAT representation of a JIT'd block.
pub fn disassemble_block(block: &Block, wasm_bytes: &[u8], config: &DisasmConfig) -> String {
    let mut output = String::new();

    // Header
    output.push_str(&format!(";; JIT Block @ 0x{:016x}\n", block.start_pc));
    output.push_str(&format!(
        ";; {} instructions, {} bytes WASM\n",
        block.len,
        wasm_bytes.len()
    ));
    output.push_str(";;\n");

    // Module header
    output.push_str("(module\n");

    // Imports section
    output.push_str("  ;; ═══════ Imports ═══════\n");
    output.push_str("  (import \"env\" \"memory\" (memory 1))\n");
    output.push_str("  (import \"host\" \"read_u64\" (func $read_u64 (param i64) (result i64)))\n");
    output
        .push_str("  (import \"host\" \"write_u64\" (func $write_u64 (param i64 i64) (result i32)))\n");
    output.push_str("  (import \"host\" \"read_u32\" (func $read_u32 (param i64) (result i32)))\n");
    output
        .push_str("  (import \"host\" \"write_u32\" (func $write_u32 (param i64 i32) (result i32)))\n");
    output.push_str("  (import \"host\" \"read_u16\" (func $read_u16 (param i64) (result i32)))\n");
    output
        .push_str("  (import \"host\" \"write_u16\" (func $write_u16 (param i64 i32) (result i32)))\n");
    output.push_str("  (import \"host\" \"read_u8\" (func $read_u8 (param i64) (result i32)))\n");
    output
        .push_str("  (import \"host\" \"write_u8\" (func $write_u8 (param i64 i32) (result i32)))\n");
    output.push_str("\n");

    // Main function
    output.push_str("  ;; ═══════ Execute Block ═══════\n");
    output.push_str("  (func $execute_block (param $state_ptr i32) (result i64)\n");

    // Disassemble each MicroOp
    let mut pc = block.start_pc;
    for (idx, op) in block.ops().iter().enumerate() {
        if config.include_source {
            output.push_str(&format!("\n    ;; {:016x}: {:?}\n", pc, op));
        }

        output.push_str(&disassemble_microop(op, pc, config));

        // Advance PC based on instruction type
        let insn_len = estimate_instruction_length(op, idx, block);
        pc = pc.wrapping_add(insn_len as u64);
    }

    // Epilogue
    output.push_str("\n    ;; Return next PC\n");
    output.push_str(&format!("    (i64.const 0x{:016x})  ;; next_pc\n", pc));
    output.push_str("  )\n");

    // Export
    output.push_str("\n  (export \"execute\" (func $execute_block))\n");
    output.push_str(")\n");

    output
}

/// Estimate instruction length for PC advancement.
fn estimate_instruction_length(op: &MicroOp, _idx: usize, _block: &Block) -> u8 {
    // Most RISC-V instructions are 4 bytes, compressed are 2 bytes
    // For accurate tracking, we'd need the actual instruction length stored
    match op {
        MicroOp::Jal { insn_len, .. }
        | MicroOp::Jalr { insn_len, .. }
        | MicroOp::Beq { insn_len, .. }
        | MicroOp::Bne { insn_len, .. }
        | MicroOp::Blt { insn_len, .. }
        | MicroOp::Bge { insn_len, .. }
        | MicroOp::Bltu { insn_len, .. }
        | MicroOp::Bgeu { insn_len, .. } => *insn_len,
        _ => 4, // Default to 4 bytes
    }
}

/// Disassemble a single MicroOp to WAT.
fn disassemble_microop(op: &MicroOp, pc: u64, config: &DisasmConfig) -> String {
    let indent = if config.pretty { "    " } else { "" };

    match *op {
        // ALU Register-Immediate Operations
        MicroOp::Addi { rd, rs1, imm } => disasm_alu_imm(indent, "add", rd, rs1, imm),
        MicroOp::Xori { rd, rs1, imm } => disasm_alu_imm(indent, "xor", rd, rs1, imm),
        MicroOp::Ori { rd, rs1, imm } => disasm_alu_imm(indent, "or", rd, rs1, imm),
        MicroOp::Andi { rd, rs1, imm } => disasm_alu_imm(indent, "and", rd, rs1, imm),
        MicroOp::Slti { rd, rs1, imm } => disasm_slti(indent, rd, rs1, imm, true),
        MicroOp::Sltiu { rd, rs1, imm } => disasm_slti(indent, rd, rs1, imm, false),
        MicroOp::Slli { rd, rs1, shamt } => disasm_shift_imm(indent, "shl", rd, rs1, shamt),
        MicroOp::Srli { rd, rs1, shamt } => disasm_shift_imm(indent, "shr_u", rd, rs1, shamt),
        MicroOp::Srai { rd, rs1, shamt } => disasm_shift_imm(indent, "shr_s", rd, rs1, shamt),

        // ALU Register-Register Operations
        MicroOp::Add { rd, rs1, rs2 } => disasm_alu_reg(indent, "add", rd, rs1, rs2),
        MicroOp::Sub { rd, rs1, rs2 } => disasm_alu_reg(indent, "sub", rd, rs1, rs2),
        MicroOp::Xor { rd, rs1, rs2 } => disasm_alu_reg(indent, "xor", rd, rs1, rs2),
        MicroOp::Or { rd, rs1, rs2 } => disasm_alu_reg(indent, "or", rd, rs1, rs2),
        MicroOp::And { rd, rs1, rs2 } => disasm_alu_reg(indent, "and", rd, rs1, rs2),
        MicroOp::Sll { rd, rs1, rs2 } => disasm_shift_reg(indent, "shl", rd, rs1, rs2),
        MicroOp::Srl { rd, rs1, rs2 } => disasm_shift_reg(indent, "shr_u", rd, rs1, rs2),
        MicroOp::Sra { rd, rs1, rs2 } => disasm_shift_reg(indent, "shr_s", rd, rs1, rs2),
        MicroOp::Slt { rd, rs1, rs2 } => disasm_slt(indent, rd, rs1, rs2, true),
        MicroOp::Sltu { rd, rs1, rs2 } => disasm_slt(indent, rd, rs1, rs2, false),

        // 32-bit ALU Operations
        MicroOp::Addiw { rd, rs1, imm } => disasm_alu_imm_w(indent, "add", rd, rs1, imm as i64),
        MicroOp::Slliw { rd, rs1, shamt } => disasm_shift_imm_w(indent, "shl", rd, rs1, shamt),
        MicroOp::Srliw { rd, rs1, shamt } => disasm_shift_imm_w(indent, "shr_u", rd, rs1, shamt),
        MicroOp::Sraiw { rd, rs1, shamt } => disasm_shift_imm_w(indent, "shr_s", rd, rs1, shamt),
        MicroOp::Addw { rd, rs1, rs2 } => disasm_alu_reg_w(indent, "add", rd, rs1, rs2),
        MicroOp::Subw { rd, rs1, rs2 } => disasm_alu_reg_w(indent, "sub", rd, rs1, rs2),
        MicroOp::Sllw { rd, rs1, rs2 } => disasm_shift_reg_w(indent, "shl", rd, rs1, rs2),
        MicroOp::Srlw { rd, rs1, rs2 } => disasm_shift_reg_w(indent, "shr_u", rd, rs1, rs2),
        MicroOp::Sraw { rd, rs1, rs2 } => disasm_shift_reg_w(indent, "shr_s", rd, rs1, rs2),

        // M-Extension
        MicroOp::Mul { rd, rs1, rs2 } => disasm_alu_reg(indent, "mul", rd, rs1, rs2),
        MicroOp::Mulh { rd, rs1, rs2 } => disasm_mulh(indent, rd, rs1, rs2, "mulh"),
        MicroOp::Mulhsu { rd, rs1, rs2 } => disasm_mulh(indent, rd, rs1, rs2, "mulhsu"),
        MicroOp::Mulhu { rd, rs1, rs2 } => disasm_mulh(indent, rd, rs1, rs2, "mulhu"),
        MicroOp::Div { rd, rs1, rs2 } => disasm_div(indent, rd, rs1, rs2, true, false),
        MicroOp::Divu { rd, rs1, rs2 } => disasm_div(indent, rd, rs1, rs2, false, false),
        MicroOp::Rem { rd, rs1, rs2 } => disasm_rem(indent, rd, rs1, rs2, true, false),
        MicroOp::Remu { rd, rs1, rs2 } => disasm_rem(indent, rd, rs1, rs2, false, false),
        MicroOp::Mulw { rd, rs1, rs2 } => disasm_alu_reg_w(indent, "mul", rd, rs1, rs2),
        MicroOp::Divw { rd, rs1, rs2 } => disasm_div(indent, rd, rs1, rs2, true, true),
        MicroOp::Divuw { rd, rs1, rs2 } => disasm_div(indent, rd, rs1, rs2, false, true),
        MicroOp::Remw { rd, rs1, rs2 } => disasm_rem(indent, rd, rs1, rs2, true, true),
        MicroOp::Remuw { rd, rs1, rs2 } => disasm_rem(indent, rd, rs1, rs2, false, true),

        // Upper Immediate
        MicroOp::Lui { rd, imm } => disasm_lui(indent, rd, imm),
        MicroOp::Auipc { rd, imm, pc_offset } => disasm_auipc(indent, rd, imm, pc_offset, pc),

        // Load Operations
        MicroOp::Lb { rd, rs1, imm, .. } => disasm_load(indent, "i8", "s", rd, rs1, imm),
        MicroOp::Lbu { rd, rs1, imm, .. } => disasm_load(indent, "u8", "u", rd, rs1, imm),
        MicroOp::Lh { rd, rs1, imm, .. } => disasm_load(indent, "i16", "s", rd, rs1, imm),
        MicroOp::Lhu { rd, rs1, imm, .. } => disasm_load(indent, "u16", "u", rd, rs1, imm),
        MicroOp::Lw { rd, rs1, imm, .. } => disasm_load(indent, "i32", "s", rd, rs1, imm),
        MicroOp::Lwu { rd, rs1, imm, .. } => disasm_load(indent, "u32", "u", rd, rs1, imm),
        MicroOp::Ld { rd, rs1, imm, .. } => disasm_load(indent, "i64", "", rd, rs1, imm),

        // Store Operations
        MicroOp::Sb { rs1, rs2, imm, .. } => disasm_store(indent, "8", rs1, rs2, imm),
        MicroOp::Sh { rs1, rs2, imm, .. } => disasm_store(indent, "16", rs1, rs2, imm),
        MicroOp::Sw { rs1, rs2, imm, .. } => disasm_store(indent, "32", rs1, rs2, imm),
        MicroOp::Sd { rs1, rs2, imm, .. } => disasm_store(indent, "64", rs1, rs2, imm),

        // Branches
        MicroOp::Beq {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "eq", rs1, rs2, imm, pc_offset, pc),
        MicroOp::Bne {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "ne", rs1, rs2, imm, pc_offset, pc),
        MicroOp::Blt {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "lt_s", rs1, rs2, imm, pc_offset, pc),
        MicroOp::Bge {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "ge_s", rs1, rs2, imm, pc_offset, pc),
        MicroOp::Bltu {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "lt_u", rs1, rs2, imm, pc_offset, pc),
        MicroOp::Bgeu {
            rs1,
            rs2,
            imm,
            pc_offset,
            ..
        } => disasm_branch(indent, "ge_u", rs1, rs2, imm, pc_offset, pc),

        // Jumps
        MicroOp::Jal {
            rd,
            imm,
            pc_offset,
            insn_len,
        } => disasm_jal(indent, rd, imm, pc_offset, insn_len, pc),
        MicroOp::Jalr {
            rd,
            rs1,
            imm,
            pc_offset,
            insn_len,
        } => disasm_jalr(indent, rd, rs1, imm, pc_offset, insn_len, pc),

        // System Operations
        MicroOp::Ecall { pc_offset } => disasm_system(indent, "ecall", pc_offset, pc),
        MicroOp::Ebreak { pc_offset } => disasm_system(indent, "ebreak", pc_offset, pc),
        MicroOp::Mret { pc_offset } => disasm_system(indent, "mret", pc_offset, pc),
        MicroOp::Sret { pc_offset } => disasm_system(indent, "sret", pc_offset, pc),
        MicroOp::Wfi { pc_offset } => disasm_system(indent, "wfi", pc_offset, pc),
        MicroOp::SfenceVma { pc_offset } => disasm_system(indent, "sfence.vma", pc_offset, pc),
        MicroOp::Fence => format!("{};; fence (no-op)\n{}(nop)\n", indent, indent),

        // CSR Operations
        MicroOp::Csrrw {
            rd,
            rs1,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrw", rd, rs1 as u16, csr, pc_offset, pc),
        MicroOp::Csrrs {
            rd,
            rs1,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrs", rd, rs1 as u16, csr, pc_offset, pc),
        MicroOp::Csrrc {
            rd,
            rs1,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrc", rd, rs1 as u16, csr, pc_offset, pc),
        MicroOp::Csrrwi {
            rd,
            zimm,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrwi", rd, zimm as u16, csr, pc_offset, pc),
        MicroOp::Csrrsi {
            rd,
            zimm,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrsi", rd, zimm as u16, csr, pc_offset, pc),
        MicroOp::Csrrci {
            rd,
            zimm,
            csr,
            pc_offset,
        } => disasm_csr(indent, "csrrci", rd, zimm as u16, csr, pc_offset, pc),

        // Atomic Operations
        MicroOp::LrW { rd, rs1, pc_offset } => disasm_lr(indent, rd, rs1, pc_offset, pc, true),
        MicroOp::LrD { rd, rs1, pc_offset } => disasm_lr(indent, rd, rs1, pc_offset, pc, false),
        MicroOp::ScW {
            rd,
            rs1,
            rs2,
            pc_offset,
        } => disasm_sc(indent, rd, rs1, rs2, pc_offset, pc, true),
        MicroOp::ScD {
            rd,
            rs1,
            rs2,
            pc_offset,
        } => disasm_sc(indent, rd, rs1, rs2, pc_offset, pc, false),
        MicroOp::AmoSwap {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "swap", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoAdd {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "add", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoXor {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "xor", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoAnd {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "and", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoOr {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "or", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoMin {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "min", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoMax {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "max", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoMinu {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "minu", rd, rs1, rs2, is_word, pc_offset, pc),
        MicroOp::AmoMaxu {
            rd,
            rs1,
            rs2,
            is_word,
            pc_offset,
        } => disasm_amo(indent, "maxu", rd, rs1, rs2, is_word, pc_offset, pc),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helper Functions for WAT Generation
// ═══════════════════════════════════════════════════════════════════════════

/// Generate WAT for ALU register-immediate operations.
fn disasm_alu_imm(indent: &str, op: &str, rd: u8, rs1: u8, imm: i64) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = x{} {} {}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const {})\n\
         {}(i64.{})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        imm,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        imm,
        indent,
        op,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for ALU register-register operations.
fn disasm_alu_reg(indent: &str, op: &str, rd: u8, rs1: u8, rs2: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = x{} {} x{}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.{})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        rs2,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        op,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for shift with immediate.
fn disasm_shift_imm(indent: &str, op: &str, rd: u8, rs1: u8, shamt: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = x{} {} {}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const {})\n\
         {}(i64.{})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        shamt,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        shamt,
        indent,
        op,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for shift with register.
fn disasm_shift_reg(indent: &str, op: &str, rd: u8, rs1: u8, rs2: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = x{} {} (x{} & 0x3F)\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const 63)\n\
         {}(i64.and)  ;; mask shift amount\n\
         {}(i64.{})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        rs2,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        indent,
        indent,
        op,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for set-less-than with immediate.
fn disasm_slti(indent: &str, rd: u8, rs1: u8, imm: i64, signed: bool) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    let cmp = if signed { "lt_s" } else { "lt_u" };
    format!(
        "{};; x{} = (x{} < {})? 1 : 0\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const {})\n\
         {}(i64.{})\n\
         {}(i64.extend_i32_u)\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        imm,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        imm,
        indent,
        cmp,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for set-less-than with register.
fn disasm_slt(indent: &str, rd: u8, rs1: u8, rs2: u8, signed: bool) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    let cmp = if signed { "lt_s" } else { "lt_u" };
    format!(
        "{};; x{} = (x{} < x{}) ? 1 : 0\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.{})\n\
         {}(i64.extend_i32_u)\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        rs2,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        cmp,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for 32-bit ALU immediate operations (sign-extended).
fn disasm_alu_imm_w(indent: &str, op: &str, rd: u8, rs1: u8, imm: i64) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = sext32(x{} {} {})\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i32.const {})\n\
         {}(i32.{})\n\
         {}(i64.extend_i32_s)  ;; sign-extend to 64-bit\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        imm,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        indent,
        imm as i32,
        indent,
        op,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for 32-bit ALU register operations (sign-extended).
fn disasm_alu_reg_w(indent: &str, op: &str, rd: u8, rs1: u8, rs2: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = sext32(x{} {} x{})\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i32.{})\n\
         {}(i64.extend_i32_s)  ;; sign-extend to 64-bit\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        rs2,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        indent,
        op,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for 32-bit shift with immediate.
fn disasm_shift_imm_w(indent: &str, op: &str, rd: u8, rs1: u8, shamt: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = sext32(x{} {} {})\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i32.const {})\n\
         {}(i32.{})\n\
         {}(i64.extend_i32_s)  ;; sign-extend to 64-bit\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        shamt,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        indent,
        shamt,
        indent,
        op,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for 32-bit shift with register.
fn disasm_shift_reg_w(indent: &str, op: &str, rd: u8, rs1: u8, rs2: u8) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = sext32(x{} {} (x{} & 0x1F))\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i32.wrap_i64)\n\
         {}(i32.const 31)\n\
         {}(i32.and)  ;; mask shift amount\n\
         {}(i32.{})\n\
         {}(i64.extend_i32_s)  ;; sign-extend to 64-bit\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        op,
        rs2,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        indent,
        indent,
        indent,
        op,
        indent,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for high-multiplication instructions.
fn disasm_mulh(indent: &str, rd: u8, rs1: u8, rs2: u8, name: &str) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = {} x{}, x{} (high bits - requires host call)\n\
         {}(call $host_{} (local.get $state_ptr) (i32.const {}) (i32.const {}))\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent, rd, name, rs1, rs2, indent, name, rs1, rs2, indent, rd as u32 * 8, rd
    )
}

/// Generate WAT for division.
fn disasm_div(indent: &str, rd: u8, rs1: u8, rs2: u8, signed: bool, is_word: bool) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    let op = if signed { "div_s" } else { "div_u" };
    let suffix = if is_word { "w" } else { "" };
    let typ = if is_word { "i32" } else { "i64" };

    if is_word {
        format!(
            "{};; x{} = sext32(x{} / x{}) ({})\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i32.wrap_i64)\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i32.wrap_i64)\n\
             {};; TODO: division by zero check\n\
             {}({}.{})\n\
             {}(i64.extend_i32_s)\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent, rd, rs1, rs2, suffix, indent, rs1 as u32 * 8, rs1, indent, indent,
            rs2 as u32 * 8, rs2, indent, indent, indent, typ, op, indent, indent,
            rd as u32 * 8, rd
        )
    } else {
        format!(
            "{};; x{} = x{} / x{}\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {};; TODO: division by zero check\n\
             {}({}.{})\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent, rd, rs1, rs2, indent, rs1 as u32 * 8, rs1, indent, rs2 as u32 * 8, rs2, indent,
            indent, typ, op, indent, rd as u32 * 8, rd
        )
    }
}

/// Generate WAT for remainder.
fn disasm_rem(indent: &str, rd: u8, rs1: u8, rs2: u8, signed: bool, is_word: bool) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    let op = if signed { "rem_s" } else { "rem_u" };
    let suffix = if is_word { "w" } else { "" };
    let typ = if is_word { "i32" } else { "i64" };

    if is_word {
        format!(
            "{};; x{} = sext32(x{} %% x{}) ({})\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i32.wrap_i64)\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i32.wrap_i64)\n\
             {};; TODO: division by zero check\n\
             {}({}.{})\n\
             {}(i64.extend_i32_s)\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent, rd, rs1, rs2, suffix, indent, rs1 as u32 * 8, rs1, indent, indent,
            rs2 as u32 * 8, rs2, indent, indent, indent, typ, op, indent, indent,
            rd as u32 * 8, rd
        )
    } else {
        format!(
            "{};; x{} = x{} %% x{}\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {};; TODO: division by zero check\n\
             {}({}.{})\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent, rd, rs1, rs2, indent, rs1 as u32 * 8, rs1, indent, rs2 as u32 * 8, rs2, indent,
            indent, typ, op, indent, rd as u32 * 8, rd
        )
    }
}

/// Generate WAT for LUI.
fn disasm_lui(indent: &str, rd: u8, imm: i64) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    format!(
        "{};; x{} = 0x{:x}\n\
         {}(i64.const {})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        imm,
        indent,
        imm,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for AUIPC.
fn disasm_auipc(indent: &str, rd: u8, imm: i64, pc_offset: u16, block_pc: u64) -> String {
    if rd == 0 {
        return format!("{}(nop)  ;; write to x0 ignored\n", indent);
    }
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let result = insn_pc.wrapping_add(imm as u64);
    format!(
        "{};; x{} = PC(0x{:x}) + 0x{:x} = 0x{:x}\n\
         {}(i64.const {})\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        insn_pc,
        imm,
        result,
        indent,
        result as i64,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for load operations.
fn disasm_load(indent: &str, size: &str, ext: &str, rd: u8, rs1: u8, imm: i64) -> String {
    if rd == 0 {
        return format!(
            "{};; load to x0 ignored, but may have side effects\n\
             {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
             {}(i64.const {})\n\
             {}(i64.add)  ;; vaddr\n\
             {}(call $read_u64)  ;; perform read for side effects\n\
             {}(drop)\n",
            indent, indent, rs1 as u32 * 8, rs1, indent, imm, indent, indent, indent
        );
    }
    let read_func = match size {
        "i8" | "u8" => "$read_u8",
        "i16" | "u16" => "$read_u16",
        "i32" | "u32" => "$read_u32",
        _ => "$read_u64",
    };
    let extend = match (size, ext) {
        ("i8", "s") => "(i64.extend_i32_s) (i64.shl (i64.const 56)) (i64.shr_s (i64.const 56))",
        ("u8", "u") => "(i64.extend_i32_u)",
        ("i16", "s") => "(i64.extend_i32_s) (i64.shl (i64.const 48)) (i64.shr_s (i64.const 48))",
        ("u16", "u") => "(i64.extend_i32_u)",
        ("i32", "s") => "(i64.extend_i32_s)",
        ("u32", "u") => "(i64.extend_i32_u)",
        _ => "", // i64 needs no extension
    };

    format!(
        "{};; x{} = mem[x{} + {}] ({}{})\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const {})\n\
         {}(i64.add)  ;; vaddr\n\
         {}(call {})  ;; read\n\
         {}{}\n\
         {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
        indent,
        rd,
        rs1,
        imm,
        size,
        ext,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        imm,
        indent,
        indent,
        read_func,
        indent,
        extend,
        indent,
        rd as u32 * 8,
        rd
    )
}

/// Generate WAT for store operations.
fn disasm_store(indent: &str, size: &str, rs1: u8, rs2: u8, imm: i64) -> String {
    let write_func = match size {
        "8" => "$write_u8",
        "16" => "$write_u16",
        "32" => "$write_u32",
        _ => "$write_u64",
    };
    let wrap = if size == "64" {
        ""
    } else {
        "\n        (i32.wrap_i64)  ;; truncate value"
    };

    format!(
        "{};; mem[x{} + {}] = x{}[{}:0]\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{} (base)\n\
         {}(i64.const {})\n\
         {}(i64.add)  ;; vaddr\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{} (value){}\n\
         {}(call {})  ;; write\n\
         {}(drop)  ;; ignore result\n",
        indent,
        rs1,
        imm,
        rs2,
        size,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        imm,
        indent,
        indent,
        rs2 as u32 * 8,
        rs2,
        wrap,
        indent,
        write_func,
        indent
    )
}

/// Generate WAT for branch operations.
fn disasm_branch(
    indent: &str,
    cmp: &str,
    rs1: u8,
    rs2: u8,
    imm: i64,
    pc_offset: u16,
    block_pc: u64,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let target_pc = insn_pc.wrapping_add(imm as u64);
    format!(
        "{};; if x{} {} x{} then jump to 0x{:x}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.{})\n\
         {}(if\n\
         {}  (then\n\
         {}    (i64.const 0x{:x})  ;; target PC\n\
         {}    (return)\n\
         {}  )\n\
         {})\n",
        indent,
        rs1,
        cmp,
        rs2,
        target_pc,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        rs2 as u32 * 8,
        rs2,
        indent,
        cmp,
        indent,
        indent,
        indent,
        target_pc,
        indent,
        indent,
        indent
    )
}

/// Generate WAT for JAL.
fn disasm_jal(
    indent: &str,
    rd: u8,
    imm: i64,
    pc_offset: u16,
    insn_len: u8,
    block_pc: u64,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let return_addr = insn_pc.wrapping_add(insn_len as u64);
    let target_pc = insn_pc.wrapping_add(imm as u64);

    let store_link = if rd != 0 {
        format!(
            "{}(i64.const 0x{:x})  ;; return address\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent,
            return_addr,
            indent,
            rd as u32 * 8,
            rd
        )
    } else {
        String::new()
    };

    format!(
        "{};; x{} = 0x{:x}; jump to 0x{:x}\n\
         {}\
         {}(i64.const 0x{:x})  ;; target PC\n\
         {}(return)\n",
        indent, rd, return_addr, target_pc, store_link, indent, target_pc, indent
    )
}

/// Generate WAT for JALR.
fn disasm_jalr(
    indent: &str,
    rd: u8,
    rs1: u8,
    imm: i64,
    pc_offset: u16,
    insn_len: u8,
    block_pc: u64,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let return_addr = insn_pc.wrapping_add(insn_len as u64);

    let store_link = if rd != 0 {
        format!(
            "{}(i64.const 0x{:x})  ;; return address\n\
             {}(i64.store (i32.add (local.get $state_ptr) (i32.const {})))  ;; store x{}\n",
            indent,
            return_addr,
            indent,
            rd as u32 * 8,
            rd
        )
    } else {
        String::new()
    };

    format!(
        "{};; x{} = 0x{:x}; jump to (x{} + {}) & ~1\n\
         {}\
         {}(i64.load (i32.add (local.get $state_ptr) (i32.const {})))  ;; load x{}\n\
         {}(i64.const {})\n\
         {}(i64.add)\n\
         {}(i64.const -2)  ;; mask off low bit\n\
         {}(i64.and)\n\
         {}(return)  ;; exit with computed target\n",
        indent,
        rd,
        return_addr,
        rs1,
        imm,
        store_link,
        indent,
        rs1 as u32 * 8,
        rs1,
        indent,
        imm,
        indent,
        indent,
        indent,
        indent
    )
}

/// Generate WAT for system instructions (ecall, ebreak, etc.).
fn disasm_system(indent: &str, name: &str, pc_offset: u16, block_pc: u64) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    format!(
        "{};; {} @ 0x{:x} (exit to interpreter)\n\
         {}(i64.const 0x{:x})  ;; PC of this instruction\n\
         {}(return)\n",
        indent, name, insn_pc, indent, insn_pc, indent
    )
}

/// Generate WAT for CSR operations.
fn disasm_csr(
    indent: &str,
    name: &str,
    rd: u8,
    rs1_or_zimm: u16,
    csr: u16,
    pc_offset: u16,
    block_pc: u64,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    format!(
        "{};; {} x{}, 0x{:03x}, {} @ 0x{:x} (exit to interpreter)\n\
         {};; CSR operations require full CPU context\n\
         {}(i64.const 0x{:x})  ;; PC of this instruction\n\
         {}(return)\n",
        indent, name, rd, csr, rs1_or_zimm, insn_pc, indent, indent, insn_pc, indent
    )
}

/// Generate WAT for LR (load-reserved).
fn disasm_lr(indent: &str, rd: u8, rs1: u8, pc_offset: u16, block_pc: u64, is_word: bool) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let suffix = if is_word { ".w" } else { ".d" };
    format!(
        "{};; lr{} x{}, (x{}) @ 0x{:x} (exit to interpreter for atomics)\n\
         {}(i64.const 0x{:x})\n\
         {}(return)\n",
        indent, suffix, rd, rs1, insn_pc, indent, insn_pc, indent
    )
}

/// Generate WAT for SC (store-conditional).
fn disasm_sc(
    indent: &str,
    rd: u8,
    rs1: u8,
    rs2: u8,
    pc_offset: u16,
    block_pc: u64,
    is_word: bool,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let suffix = if is_word { ".w" } else { ".d" };
    format!(
        "{};; sc{} x{}, x{}, (x{}) @ 0x{:x} (exit to interpreter for atomics)\n\
         {}(i64.const 0x{:x})\n\
         {}(return)\n",
        indent, suffix, rd, rs2, rs1, insn_pc, indent, insn_pc, indent
    )
}

/// Generate WAT for AMO operations.
fn disasm_amo(
    indent: &str,
    op: &str,
    rd: u8,
    rs1: u8,
    rs2: u8,
    is_word: bool,
    pc_offset: u16,
    block_pc: u64,
) -> String {
    let insn_pc = block_pc.wrapping_add(pc_offset as u64);
    let suffix = if is_word { ".w" } else { ".d" };
    format!(
        "{};; amo{}{} x{}, x{}, (x{}) @ 0x{:x} (exit to interpreter for atomics)\n\
         {}(i64.const 0x{:x})\n\
         {}(return)\n",
        indent, op, suffix, rd, rs2, rs1, insn_pc, indent, insn_pc, indent
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// Logging and Short Format
// ═══════════════════════════════════════════════════════════════════════════

/// Log JIT compilation event with WAT.
pub fn log_jit_compilation(block: &Block, wasm_bytes: &[u8], config: &DisasmConfig) {
    #[cfg(target_arch = "wasm32")]
    {
        let wat = disassemble_block(block, wasm_bytes, config);
        web_sys::console::group_collapsed_1(
            &format!("🔧 JIT: Block @ 0x{:016x}", block.start_pc).into(),
        );
        web_sys::console::log_1(&format!("Instructions: {}", block.len).into());
        web_sys::console::log_1(&format!("WASM bytes: {}", wasm_bytes.len()).into());
        web_sys::console::log_1(&"\n--- WAT ---".into());
        web_sys::console::log_1(&wat.into());
        web_sys::console::group_end();
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let wat = disassemble_block(block, wasm_bytes, config);
        eprintln!("═══ JIT: Block @ 0x{:016x} ═══", block.start_pc);
        eprintln!("{}", wat);
    }
}

/// Format a single instruction for one-line debug output.
pub fn format_instruction_short(op: &MicroOp) -> String {
    use MicroOp::*;
    match *op {
        // ALU Immediate
        Addi { rd, rs1, imm } => format!("addi x{}, x{}, {}", rd, rs1, imm),
        Xori { rd, rs1, imm } => format!("xori x{}, x{}, {}", rd, rs1, imm),
        Ori { rd, rs1, imm } => format!("ori x{}, x{}, {}", rd, rs1, imm),
        Andi { rd, rs1, imm } => format!("andi x{}, x{}, {}", rd, rs1, imm),
        Slti { rd, rs1, imm } => format!("slti x{}, x{}, {}", rd, rs1, imm),
        Sltiu { rd, rs1, imm } => format!("sltiu x{}, x{}, {}", rd, rs1, imm),
        Slli { rd, rs1, shamt } => format!("slli x{}, x{}, {}", rd, rs1, shamt),
        Srli { rd, rs1, shamt } => format!("srli x{}, x{}, {}", rd, rs1, shamt),
        Srai { rd, rs1, shamt } => format!("srai x{}, x{}, {}", rd, rs1, shamt),

        // ALU Register
        Add { rd, rs1, rs2 } => format!("add x{}, x{}, x{}", rd, rs1, rs2),
        Sub { rd, rs1, rs2 } => format!("sub x{}, x{}, x{}", rd, rs1, rs2),
        Xor { rd, rs1, rs2 } => format!("xor x{}, x{}, x{}", rd, rs1, rs2),
        Or { rd, rs1, rs2 } => format!("or x{}, x{}, x{}", rd, rs1, rs2),
        And { rd, rs1, rs2 } => format!("and x{}, x{}, x{}", rd, rs1, rs2),
        Sll { rd, rs1, rs2 } => format!("sll x{}, x{}, x{}", rd, rs1, rs2),
        Srl { rd, rs1, rs2 } => format!("srl x{}, x{}, x{}", rd, rs1, rs2),
        Sra { rd, rs1, rs2 } => format!("sra x{}, x{}, x{}", rd, rs1, rs2),
        Slt { rd, rs1, rs2 } => format!("slt x{}, x{}, x{}", rd, rs1, rs2),
        Sltu { rd, rs1, rs2 } => format!("sltu x{}, x{}, x{}", rd, rs1, rs2),

        // 32-bit ALU
        Addiw { rd, rs1, imm } => format!("addiw x{}, x{}, {}", rd, rs1, imm),
        Slliw { rd, rs1, shamt } => format!("slliw x{}, x{}, {}", rd, rs1, shamt),
        Srliw { rd, rs1, shamt } => format!("srliw x{}, x{}, {}", rd, rs1, shamt),
        Sraiw { rd, rs1, shamt } => format!("sraiw x{}, x{}, {}", rd, rs1, shamt),
        Addw { rd, rs1, rs2 } => format!("addw x{}, x{}, x{}", rd, rs1, rs2),
        Subw { rd, rs1, rs2 } => format!("subw x{}, x{}, x{}", rd, rs1, rs2),
        Sllw { rd, rs1, rs2 } => format!("sllw x{}, x{}, x{}", rd, rs1, rs2),
        Srlw { rd, rs1, rs2 } => format!("srlw x{}, x{}, x{}", rd, rs1, rs2),
        Sraw { rd, rs1, rs2 } => format!("sraw x{}, x{}, x{}", rd, rs1, rs2),

        // M-Extension
        Mul { rd, rs1, rs2 } => format!("mul x{}, x{}, x{}", rd, rs1, rs2),
        Mulh { rd, rs1, rs2 } => format!("mulh x{}, x{}, x{}", rd, rs1, rs2),
        Mulhsu { rd, rs1, rs2 } => format!("mulhsu x{}, x{}, x{}", rd, rs1, rs2),
        Mulhu { rd, rs1, rs2 } => format!("mulhu x{}, x{}, x{}", rd, rs1, rs2),
        Div { rd, rs1, rs2 } => format!("div x{}, x{}, x{}", rd, rs1, rs2),
        Divu { rd, rs1, rs2 } => format!("divu x{}, x{}, x{}", rd, rs1, rs2),
        Rem { rd, rs1, rs2 } => format!("rem x{}, x{}, x{}", rd, rs1, rs2),
        Remu { rd, rs1, rs2 } => format!("remu x{}, x{}, x{}", rd, rs1, rs2),
        Mulw { rd, rs1, rs2 } => format!("mulw x{}, x{}, x{}", rd, rs1, rs2),
        Divw { rd, rs1, rs2 } => format!("divw x{}, x{}, x{}", rd, rs1, rs2),
        Divuw { rd, rs1, rs2 } => format!("divuw x{}, x{}, x{}", rd, rs1, rs2),
        Remw { rd, rs1, rs2 } => format!("remw x{}, x{}, x{}", rd, rs1, rs2),
        Remuw { rd, rs1, rs2 } => format!("remuw x{}, x{}, x{}", rd, rs1, rs2),

        // Upper Immediate
        Lui { rd, imm } => format!("lui x{}, {:#x}", rd, (imm as u64) >> 12),
        Auipc { rd, imm, .. } => format!("auipc x{}, {:#x}", rd, (imm as u64) >> 12),

        // Loads
        Lb { rd, rs1, imm, .. } => format!("lb x{}, {}(x{})", rd, imm, rs1),
        Lbu { rd, rs1, imm, .. } => format!("lbu x{}, {}(x{})", rd, imm, rs1),
        Lh { rd, rs1, imm, .. } => format!("lh x{}, {}(x{})", rd, imm, rs1),
        Lhu { rd, rs1, imm, .. } => format!("lhu x{}, {}(x{})", rd, imm, rs1),
        Lw { rd, rs1, imm, .. } => format!("lw x{}, {}(x{})", rd, imm, rs1),
        Lwu { rd, rs1, imm, .. } => format!("lwu x{}, {}(x{})", rd, imm, rs1),
        Ld { rd, rs1, imm, .. } => format!("ld x{}, {}(x{})", rd, imm, rs1),

        // Stores
        Sb { rs1, rs2, imm, .. } => format!("sb x{}, {}(x{})", rs2, imm, rs1),
        Sh { rs1, rs2, imm, .. } => format!("sh x{}, {}(x{})", rs2, imm, rs1),
        Sw { rs1, rs2, imm, .. } => format!("sw x{}, {}(x{})", rs2, imm, rs1),
        Sd { rs1, rs2, imm, .. } => format!("sd x{}, {}(x{})", rs2, imm, rs1),

        // Branches
        Beq { rs1, rs2, imm, .. } => format!("beq x{}, x{}, {}", rs1, rs2, imm),
        Bne { rs1, rs2, imm, .. } => format!("bne x{}, x{}, {}", rs1, rs2, imm),
        Blt { rs1, rs2, imm, .. } => format!("blt x{}, x{}, {}", rs1, rs2, imm),
        Bge { rs1, rs2, imm, .. } => format!("bge x{}, x{}, {}", rs1, rs2, imm),
        Bltu { rs1, rs2, imm, .. } => format!("bltu x{}, x{}, {}", rs1, rs2, imm),
        Bgeu { rs1, rs2, imm, .. } => format!("bgeu x{}, x{}, {}", rs1, rs2, imm),

        // Jumps
        Jal { rd, imm, .. } => format!("jal x{}, {}", rd, imm),
        Jalr { rd, rs1, imm, .. } => format!("jalr x{}, {}(x{})", rd, imm, rs1),

        // System
        Ecall { .. } => "ecall".to_string(),
        Ebreak { .. } => "ebreak".to_string(),
        Mret { .. } => "mret".to_string(),
        Sret { .. } => "sret".to_string(),
        Wfi { .. } => "wfi".to_string(),
        SfenceVma { .. } => "sfence.vma".to_string(),
        Fence => "fence".to_string(),

        // CSR
        Csrrw { rd, rs1, csr, .. } => format!("csrrw x{}, 0x{:03x}, x{}", rd, csr, rs1),
        Csrrs { rd, rs1, csr, .. } => format!("csrrs x{}, 0x{:03x}, x{}", rd, csr, rs1),
        Csrrc { rd, rs1, csr, .. } => format!("csrrc x{}, 0x{:03x}, x{}", rd, csr, rs1),
        Csrrwi { rd, zimm, csr, .. } => format!("csrrwi x{}, 0x{:03x}, {}", rd, csr, zimm),
        Csrrsi { rd, zimm, csr, .. } => format!("csrrsi x{}, 0x{:03x}, {}", rd, csr, zimm),
        Csrrci { rd, zimm, csr, .. } => format!("csrrci x{}, 0x{:03x}, {}", rd, csr, zimm),

        // Atomics
        LrW { rd, rs1, .. } => format!("lr.w x{}, (x{})", rd, rs1),
        LrD { rd, rs1, .. } => format!("lr.d x{}, (x{})", rd, rs1),
        ScW { rd, rs1, rs2, .. } => format!("sc.w x{}, x{}, (x{})", rd, rs2, rs1),
        ScD { rd, rs1, rs2, .. } => format!("sc.d x{}, x{}, (x{})", rd, rs2, rs1),
        AmoSwap { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amoswap.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoAdd { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amoadd.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoXor { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amoxor.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoAnd { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amoand.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoOr { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amoor.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoMin { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amomin.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoMax { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amomax.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoMinu { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amominu.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
        AmoMaxu { rd, rs1, rs2, is_word, .. } => {
            let suffix = if is_word { "w" } else { "d" };
            format!("amomaxu.{} x{}, x{}, (x{})", suffix, rd, rs2, rs1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_instruction_short() {
        assert_eq!(
            format_instruction_short(&MicroOp::Addi {
                rd: 10,
                rs1: 0,
                imm: 42
            }),
            "addi x10, x0, 42"
        );
        assert_eq!(
            format_instruction_short(&MicroOp::Add {
                rd: 1,
                rs1: 2,
                rs2: 3
            }),
            "add x1, x2, x3"
        );
        assert_eq!(
            format_instruction_short(&MicroOp::Ld {
                rd: 5,
                rs1: 10,
                imm: 8,
                pc_offset: 0
            }),
            "ld x5, 8(x10)"
        );
    }

    #[test]
    fn test_disasm_config_default() {
        let config = DisasmConfig::default();
        assert!(config.include_source);
        assert!(config.pretty);
        assert!(config.include_addresses);
    }

    #[test]
    fn test_disasm_x0_write_ignored() {
        let _config = DisasmConfig::default();
        let wat = disasm_alu_imm("    ", "add", 0, 1, 5);
        assert!(wat.contains("x0 ignored"));
    }
}

