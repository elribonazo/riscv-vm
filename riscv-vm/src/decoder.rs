use crate::Trap;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Register {
    X0 = 0,
    X1 = 1,
    X2 = 2,
    X3 = 3,
    X4 = 4,
    X5 = 5,
    X6 = 6,
    X7 = 7,
    X8 = 8,
    X9 = 9,
    X10 = 10,
    X11 = 11,
    X12 = 12,
    X13 = 13,
    X14 = 14,
    X15 = 15,
    X16 = 16,
    X17 = 17,
    X18 = 18,
    X19 = 19,
    X20 = 20,
    X21 = 21,
    X22 = 22,
    X23 = 23,
    X24 = 24,
    X25 = 25,
    X26 = 26,
    X27 = 27,
    X28 = 28,
    X29 = 29,
    X30 = 30,
    X31 = 31,
}

impl Register {
    #[inline(always)]
    pub fn from_u32(v: u32) -> Self {
        // SAFETY: v & 0x1F is always in range 0..=31, matching our enum variants
        unsafe { std::mem::transmute((v & 0x1F) as u8) }
    }

    #[inline(always)]
    pub fn to_usize(&self) -> usize {
        *self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Lui {
        rd: Register,
        imm: i64,
    },
    Auipc {
        rd: Register,
        imm: i64,
    },
    Jal {
        rd: Register,
        imm: i64,
    },
    Jalr {
        rd: Register,
        rs1: Register,
        imm: i64,
    },
    Branch {
        rs1: Register,
        rs2: Register,
        imm: i64,
        funct3: u32,
    },
    Load {
        rd: Register,
        rs1: Register,
        imm: i64,
        funct3: u32,
    },
    Store {
        rs1: Register,
        rs2: Register,
        imm: i64,
        funct3: u32,
    },
    OpImm {
        rd: Register,
        rs1: Register,
        imm: i64,
        funct3: u32,
        funct7: u32,
    }, // I-type ALU (ADDI etc)
    Op {
        rd: Register,
        rs1: Register,
        rs2: Register,
        funct3: u32,
        funct7: u32,
    }, // R-type ALU
    OpImm32 {
        rd: Register,
        rs1: Register,
        imm: i64,
        funct3: u32,
        funct7: u32,
    }, // ADDIW etc
    Op32 {
        rd: Register,
        rs1: Register,
        rs2: Register,
        funct3: u32,
        funct7: u32,
    }, // ADDW etc
    System {
        rd: Register,
        rs1: Register,
        funct3: u32,
        imm: u32,
    }, // CSRs / Ecall (imm used for csr addr usually)
    Amo {
        rd: Register,
        rs1: Register,
        rs2: Register,
        funct3: u32,
        funct5: u32,
        aq: bool,
        rl: bool,
    }, // RV64A atomics (LR/SC/AMO*)
    Fence, // FENCE / FENCE.I
}

#[inline]
pub fn decode(insn: u32) -> Result<Op, Trap> {
    let opcode = insn & 0x7F;
    let rd = Register::from_u32((insn >> 7) & 0x1F);
    let funct3 = (insn >> 12) & 0x7;
    let rs1 = Register::from_u32((insn >> 15) & 0x1F);
    let rs2 = Register::from_u32((insn >> 20) & 0x1F);
    let funct7 = (insn >> 25) & 0x7F;

    // Sign extension helpers
    let imm_i = ((insn as i32) >> 20) as i64;
    let imm_s = (((insn as i32) >> 25) << 5) as i64 | (((insn >> 7) & 0x1F) as i64);
    // B-type: imm[12|10:5|4:1|11]
    let imm_b = {
        let bit31 = (insn >> 31) & 1;
        let bit30_25 = (insn >> 25) & 0x3F;
        let bit11_8 = (insn >> 8) & 0xF;
        let bit7 = (insn >> 7) & 1;
        let val = (bit31 << 12) | (bit7 << 11) | (bit30_25 << 5) | (bit11_8 << 1);
        // Sign extend from bit 12
        ((val as i32) << 19 >> 19) as i64
    };
    // U-type: imm[31:12]
    let imm_u = ((insn as i32) & 0xFFFFF000u32 as i32) as i64;
    // J-type: imm[20|10:1|11|19:12]
    let imm_j = {
        let bit31 = (insn >> 31) & 1;
        let bit30_21 = (insn >> 21) & 0x3FF;
        let bit20 = (insn >> 20) & 1;
        let bit19_12 = (insn >> 12) & 0xFF;
        let val = (bit31 << 20) | (bit19_12 << 12) | (bit20 << 11) | (bit30_21 << 1);
        ((val as i32) << 11 >> 11) as i64
    };

    match opcode {
        0x37 => Ok(Op::Lui { rd, imm: imm_u }),
        0x17 => Ok(Op::Auipc { rd, imm: imm_u }),
        0x6F => Ok(Op::Jal { rd, imm: imm_j }),
        0x67 => Ok(Op::Jalr {
            rd,
            rs1,
            imm: imm_i,
        }),
        0x63 => Ok(Op::Branch {
            rs1,
            rs2,
            imm: imm_b,
            funct3,
        }),
        0x03 => Ok(Op::Load {
            rd,
            rs1,
            imm: imm_i,
            funct3,
        }),
        0x23 => Ok(Op::Store {
            rs1,
            rs2,
            imm: imm_s,
            funct3,
        }),
        0x13 => Ok(Op::OpImm {
            rd,
            rs1,
            imm: imm_i,
            funct3,
            funct7,
        }),
        0x33 => Ok(Op::Op {
            rd,
            rs1,
            rs2,
            funct3,
            funct7,
        }),
        0x1B => Ok(Op::OpImm32 {
            rd,
            rs1,
            imm: imm_i,
            funct3,
            funct7,
        }),
        0x3B => Ok(Op::Op32 {
            rd,
            rs1,
            rs2,
            funct3,
            funct7,
        }),
        0x2F => {
            // A-extension (atomics)
            let funct5 = (insn >> 27) & 0x1F;
            let aq = ((insn >> 26) & 1) != 0;
            let rl = ((insn >> 25) & 1) != 0;
            Ok(Op::Amo {
                rd,
                rs1,
                rs2,
                funct3,
                funct5,
                aq,
                rl,
            })
        }
        0x73 => {
            let i_imm = (insn >> 20) & 0xFFF;
            Ok(Op::System {
                rd,
                rs1,
                funct3,
                imm: i_imm,
            })
        }
        0x0F => Ok(Op::Fence),

        _ => Err(Trap::IllegalInstruction(insn as u64)),
    }
}

// -------- Compressed (C) extension expansion ---------------------------------
//
// These helpers expand 16-bit compressed instructions into canonical 32-bit
// encodings, which are then fed through the normal `decode()` function.

#[inline(always)]
fn encode_i(imm: i32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
    let imm12 = (imm as u32) & 0xFFF;
    (imm12 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
}

#[inline(always)]
fn encode_u(imm: i32, rd: u32, opcode: u32) -> u32 {
    // U-type: imm[31:12] in bits[31:12], low 12 bits zero.
    let imm20 = ((imm as u32) >> 12) & 0xFFFFF;
    (imm20 << 12) | (rd << 7) | opcode
}

#[inline(always)]
fn encode_r(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
    (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
}

#[inline(always)]
fn encode_s(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
    let imm12 = (imm as u32) & 0xFFF;
    let imm11_5 = (imm12 >> 5) & 0x7F;
    let imm4_0 = imm12 & 0x1F;
    (imm11_5 << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (imm4_0 << 7)
        | opcode
}

#[inline(always)]
fn encode_j(imm: i32, rd: u32) -> u32 {
    // J-type immediate, imm is already the signed byte offset.
    let imm20 = ((imm >> 20) & 0x1) as u32;
    let imm10_1 = ((imm >> 1) & 0x3FF) as u32;
    let imm11 = ((imm >> 11) & 0x1) as u32;
    let imm19_12 = ((imm >> 12) & 0xFF) as u32;
    (imm20 << 31) | (imm19_12 << 12) | (imm11 << 20) | (imm10_1 << 21) | (rd << 7) | 0x6F
}

#[inline(always)]
fn encode_b(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
    // B-type immediate, imm is signed byte offset (multiple of 2).
    let imm13 = (imm as u32) & 0x1FFF;
    let imm12 = (imm13 >> 12) & 0x1;
    let imm10_5 = (imm13 >> 5) & 0x3F;
    let imm4_1 = (imm13 >> 1) & 0xF;
    let imm11 = (imm13 >> 11) & 0x1;
    (imm12 << 31)
        | (imm10_5 << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (imm4_1 << 8)
        | (imm11 << 7)
        | opcode
}

#[inline(always)]
fn sext(value: u32, bits: u8) -> i32 {
    let shift = 32 - bits as i32;
    ((value << shift) as i32) >> shift
}

#[inline]
pub fn expand_compressed(insn: u16) -> Result<u32, Trap> {
    let opcode = insn & 0x3;
    let funct3 = (insn >> 13) & 0x7;

    match opcode {
        0b00 => expand_q0(insn, funct3),
        0b01 => expand_q1(insn, funct3),
        0b10 => expand_q2(insn, funct3),
        _ => Err(Trap::IllegalInstruction(insn as u64)),
    }
}

#[inline]
fn expand_q0(insn: u16, funct3: u16) -> Result<u32, Trap> {
    let insn_u = insn as u32;
    match funct3 {
        // C.ADDI4SPN -> ADDI rd', x2, nzuimm
        0b000 => {
            let nzuimm = (((insn_u >> 6) & 0x1) << 2)
                | (((insn_u >> 5) & 0x1) << 3)
                | (((insn_u >> 11) & 0x3) << 4)
                | (((insn_u >> 7) & 0xF) << 6);
            if nzuimm == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            let rd_prime = 8 + ((insn_u >> 2) & 0x7);
            Ok(encode_i(nzuimm as i32, 2, 0x0, rd_prime, 0x13))
        }
        // C.LW -> LW rd', uimm(rs1')
        0b010 => {
            let uimm =
                (((insn_u >> 6) & 0x1) << 2) | (((insn_u >> 10) & 0x7) << 3) | (((insn_u >> 5) & 0x1) << 6);
            let rd_prime = 8 + ((insn_u >> 2) & 0x7);
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            Ok(encode_i(uimm as i32, rs1_prime, 0x2, rd_prime, 0x03))
        }
        // C.LD -> LD rd', uimm(rs1')
        0b011 => {
            let uimm = (((insn_u >> 10) & 0x7) << 3) | (((insn_u >> 5) & 0x3) << 6);
            let rd_prime = 8 + ((insn_u >> 2) & 0x7);
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            Ok(encode_i(uimm as i32, rs1_prime, 0x3, rd_prime, 0x03))
        }
        // C.SW -> SW rs2', uimm(rs1')
        0b110 => {
            let uimm =
                (((insn_u >> 6) & 0x1) << 2) | (((insn_u >> 10) & 0x7) << 3) | (((insn_u >> 5) & 0x1) << 6);
            let rs2_prime = 8 + ((insn_u >> 2) & 0x7);
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            Ok(encode_s(uimm as i32, rs2_prime, rs1_prime, 0x2, 0x23))
        }
        // C.SD -> SD rs2', uimm(rs1')
        0b111 => {
            let uimm = (((insn_u >> 10) & 0x7) << 3) | (((insn_u >> 5) & 0x3) << 6);
            let rs2_prime = 8 + ((insn_u >> 2) & 0x7);
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            Ok(encode_s(uimm as i32, rs2_prime, rs1_prime, 0x3, 0x23))
        }
        _ => Err(Trap::IllegalInstruction(insn as u64)),
    }
}

#[inline]
fn expand_q1(insn: u16, funct3: u16) -> Result<u32, Trap> {
    let insn_u = insn as u32;
    match funct3 {
        // C.NOP / C.ADDI
        0b000 => {
            let rd = (insn_u >> 7) & 0x1F;
            let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
            let imm = sext(imm_bits, 6);
            if rd == 0 {
                if imm == 0 {
                    // C.NOP
                    return Ok(encode_i(0, 0, 0x0, 0, 0x13));
                } else {
                    return Err(Trap::IllegalInstruction(insn as u64));
                }
            }
            Ok(encode_i(imm, rd, 0x0, rd, 0x13)) // ADDI rd, rd, imm
        }
        // RV64: C.ADDIW
        0b001 => {
            let rd = (insn_u >> 7) & 0x1F;
            let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
            let imm = sext(imm_bits, 6);
            if rd == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            Ok(encode_i(imm, rd, 0x0, rd, 0x1B)) // ADDIW rd, rd, imm
        }
        // C.LI -> ADDI rd, x0, imm
        0b010 => {
            let rd = (insn_u >> 7) & 0x1F;
            let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
            let imm = sext(imm_bits, 6);
            if rd == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            Ok(encode_i(imm, 0, 0x0, rd, 0x13))
        }
        // C.ADDI16SP / C.LUI
        0b011 => {
            let rd = (insn_u >> 7) & 0x1F;
            if rd == 2 {
                // C.ADDI16SP
                let mut nz = 0u32;
                nz |= ((insn_u >> 12) & 0x1) << 9;
                nz |= ((insn_u >> 3) & 0x3) << 7;
                nz |= ((insn_u >> 5) & 0x1) << 6;
                nz |= ((insn_u >> 2) & 0x1) << 5;
                nz |= ((insn_u >> 6) & 0x1) << 4;
                if nz == 0 {
                    return Err(Trap::IllegalInstruction(insn as u64));
                }
                let imm = sext(nz, 10);
                Ok(encode_i(imm, 2, 0x0, 2, 0x13)) // ADDI x2,x2,imm
            } else {
                // C.LUI -> LUI rd, imm
                let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
                if rd == 0 || imm_bits == 0 {
                    return Err(Trap::IllegalInstruction(insn as u64));
                }
                let imm = sext(imm_bits, 6);
                Ok(encode_u(imm << 12, rd, 0x37))
            }
        }
        // C.SRLI / C.SRAI / C.ANDI / C.SUB/XOR/OR/AND
        0b100 => {
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            let rs2_prime = 8 + ((insn_u >> 2) & 0x7);
            let op = (insn_u >> 10) & 0x3;
            match op {
                // C.SRLI
                0b00 => {
                    let shamt_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
                    let shamt = shamt_bits & 0x3F; // RV64: 6-bit shamt
                    Ok(encode_i(shamt as i32, rs1_prime, 0x5, rs1_prime, 0x13)) // SRLI
                }
                // C.SRAI
                0b01 => {
                    let shamt_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
                    let shamt = shamt_bits & 0x3F;
                    // SRAI encoding: funct7=0b0100000, funct3=101
                    Ok(encode_i((0x20 << 6) | (shamt as i32), rs1_prime, 0x5, rs1_prime, 0x13))
                }
                // C.ANDI
                0b10 => {
                    let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
                    let imm = sext(imm_bits, 6);
                    Ok(encode_i(imm, rs1_prime, 0x7, rs1_prime, 0x13))
                }
                // C.SUB / C.XOR / C.OR / C.AND (bit12=0) or C.SUBW / C.ADDW (bit12=1, RV64)
                0b11 => {
                    let bit12 = (insn_u >> 12) & 0x1;
                    let funct2 = (insn_u >> 5) & 0x3;
                    
                    if bit12 == 0 {
                        // C.SUB / C.XOR / C.OR / C.AND -> R-type with opcode 0x33
                        let (funct3, funct7) = match funct2 {
                            0b00 => (0x0, 0x20), // SUB
                            0b01 => (0x4, 0x00), // XOR
                            0b10 => (0x6, 0x00), // OR
                            0b11 => (0x7, 0x00), // AND
                            _ => unreachable!(),
                        };
                        Ok(
                            (funct7 << 25)
                                | (rs2_prime << 20)
                                | (rs1_prime << 15)
                                | (funct3 << 12)
                                | (rs1_prime << 7)
                                | 0x33,
                        )
                    } else {
                        // RV64C: C.SUBW / C.ADDW -> R-type with opcode 0x3B (Op32)
                        match funct2 {
                            0b00 => {
                                // C.SUBW -> SUBW rd', rd', rs2'
                                Ok(encode_r(0x20, rs2_prime, rs1_prime, 0x0, rs1_prime, 0x3B))
                            }
                            0b01 => {
                                // C.ADDW -> ADDW rd', rd', rs2'
                                Ok(encode_r(0x00, rs2_prime, rs1_prime, 0x0, rs1_prime, 0x3B))
                            }
                            // funct2 = 0b10, 0b11 are reserved in RV64C
                            _ => Err(Trap::IllegalInstruction(insn as u64)),
                        }
                    }
                }
                _ => Err(Trap::IllegalInstruction(insn as u64)),
            }
        }
        // C.J (unconditional jump)
        0b101 => {
            // C.J immediate: imm[11|4|9:8|10|6|7|3:1|5] with bit 0 implicitly 0
            let mut off = 0u32;
            off |= ((insn_u >> 12) & 0x1) << 11;
            off |= ((insn_u >> 11) & 0x1) << 4;
            off |= ((insn_u >> 9) & 0x3) << 8;
            off |= ((insn_u >> 8) & 0x1) << 10;
            off |= ((insn_u >> 7) & 0x1) << 6;
            off |= ((insn_u >> 6) & 0x1) << 7;
            off |= ((insn_u >> 3) & 0x7) << 1;
            off |= ((insn_u >> 2) & 0x1) << 5;
            // off already has bit 0 = 0 implicitly; sign-extend from bit 11
            let imm = sext(off, 12);
            Ok(encode_j(imm, 0)) // JAL x0, imm
        }
        // C.BEQZ
        0b110 => {
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            // C.BEQZ immediate: imm[8|4:3|7:6|2:1|5] with bit 0 implicitly 0
            let mut off = 0u32;
            off |= ((insn_u >> 12) & 0x1) << 8;
            off |= ((insn_u >> 10) & 0x3) << 3;
            off |= ((insn_u >> 5) & 0x3) << 6;
            off |= ((insn_u >> 3) & 0x3) << 1;
            off |= ((insn_u >> 2) & 0x1) << 5;
            // off already has bit 0 = 0 implicitly; sign-extend from bit 8
            let imm = sext(off, 9);
            Ok(encode_b(imm, 0, rs1_prime, 0x0, 0x63)) // BEQ rs1', x0, imm
        }
        // C.BNEZ
        0b111 => {
            let rs1_prime = 8 + ((insn_u >> 7) & 0x7);
            // C.BNEZ immediate: imm[8|4:3|7:6|2:1|5] with bit 0 implicitly 0
            let mut off = 0u32;
            off |= ((insn_u >> 12) & 0x1) << 8;
            off |= ((insn_u >> 10) & 0x3) << 3;
            off |= ((insn_u >> 5) & 0x3) << 6;
            off |= ((insn_u >> 3) & 0x3) << 1;
            off |= ((insn_u >> 2) & 0x1) << 5;
            // off already has bit 0 = 0 implicitly; sign-extend from bit 8
            let imm = sext(off, 9);
            Ok(encode_b(imm, 0, rs1_prime, 0x1, 0x63)) // BNE rs1', x0, imm
        }
        _ => Err(Trap::IllegalInstruction(insn as u64)),
    }
}

#[inline]
fn expand_q2(insn: u16, funct3: u16) -> Result<u32, Trap> {
    let insn_u = insn as u32;
    match funct3 {
        // C.SLLI
        0b000 => {
            let rd = (insn_u >> 7) & 0x1F;
            let imm_bits = ((insn_u >> 2) & 0x1F) | (((insn_u >> 12) & 0x1) << 5);
            let imm = imm_bits & 0x3F; // RV64: 6-bit shamt
            if rd == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            Ok(encode_i(imm as i32, rd, 0x1, rd, 0x13))
        }
        // C.LWSP
        0b010 => {
            let rd = (insn_u >> 7) & 0x1F;
            if rd == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            let uimm = (((insn_u >> 4) & 0x7) << 2)
                | (((insn_u >> 12) & 0x1) << 5)
                | (((insn_u >> 2) & 0x3) << 6);
            Ok(encode_i(uimm as i32, 2, 0x2, rd, 0x03))
        }
        // C.LDSP: LD rd, uimm(sp) - uimm[5|4:3|8:6] scaled by 8
        0b011 => {
            let rd = (insn_u >> 7) & 0x1F;
            if rd == 0 {
                return Err(Trap::IllegalInstruction(insn as u64));
            }
            // bit 12 -> uimm[5], bits [6:5] -> uimm[4:3], bits [4:2] -> uimm[8:6]
            let uimm = (((insn_u >> 12) & 0x1) << 5)
                | (((insn_u >> 5) & 0x3) << 3)
                | (((insn_u >> 2) & 0x7) << 6);
            Ok(encode_i(uimm as i32, 2, 0x3, rd, 0x03))
        }
        // C.JR / C.MV / C.EBREAK / C.JALR / C.ADD
        0b100 => {
            let rd = (insn_u >> 7) & 0x1F;
            let rs2 = (insn_u >> 2) & 0x1F;
            let bit12 = (insn_u >> 12) & 0x1;
            match (bit12, rs2, rd) {
                // C.JR: rs2=0, bit12=0, rd!=0
                (0, 0, rd) if rd != 0 => {
                    Ok(encode_i(0, rd, 0x0, 0, 0x67)) // JALR x0, rd, 0
                }
                // C.MV: bit12=0, rs2!=0, rd!=0
                (0, rs2, rd) if rs2 != 0 && rd != 0 => {
                    Ok(encode_r(0x00, rs2, 0, 0x0, rd, 0x33)) // ADD rd, x0, rs2
                }
                // C.EBREAK: bit12=1, rd=0, rs2=0
                (1, 0, 0) => Ok(0x0010_0073),
                // C.JALR: bit12=1, rs2=0, rd!=0
                (1, 0, rd) if rd != 0 => {
                    Ok(encode_i(0, rd, 0x0, 1, 0x67)) // JALR x1, rd, 0
                }
                // C.ADD: bit12=1, rs2!=0, rd!=0
                (1, rs2, rd) if rs2 != 0 && rd != 0 => {
                    Ok(encode_r(0x00, rs2, rd, 0x0, rd, 0x33)) // ADD rd, rd, rs2
                }
                _ => Err(Trap::IllegalInstruction(insn as u64)),
            }
        }
        // C.SWSP: SW rs2, uimm(sp) - uimm[5:2|7:6] scaled by 4
        0b110 => {
            let rs2 = (insn_u >> 2) & 0x1F;
            // bits [12:9] -> uimm[5:2], bits [8:7] -> uimm[7:6]
            let uimm = (((insn_u >> 9) & 0xF) << 2) | (((insn_u >> 7) & 0x3) << 6);
            Ok(encode_s(uimm as i32, rs2, 2, 0x2, 0x23))
        }
        // C.SDSP: SD rs2, uimm(sp) - uimm[5:3|8:6] scaled by 8
        0b111 => {
            let rs2 = (insn_u >> 2) & 0x1F;
            // bits [12:10] -> uimm[5:3], bits [9:7] -> uimm[8:6]
            let uimm = (((insn_u >> 10) & 0x7) << 3) | (((insn_u >> 7) & 0x7) << 6);
            Ok(encode_s(uimm as i32, rs2, 2, 0x3, 0x23))
        }
        _ => Err(Trap::IllegalInstruction(insn as u64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_lui_and_jal() {
        // LUI x2, 0x12345
        let lui_insn: u32 = 0x12345137;
        let op = decode(lui_insn).unwrap();
        match op {
            Op::Lui { rd, imm } => {
                assert_eq!(rd, Register::X2);
                assert_eq!(imm, 0x0000_0000_1234_5000);
            }
            _ => panic!("Expected LUI op"),
        }

        // JAL x1, 8 (matches cpu::tests::test_jal)
        let jal_insn: u32 = (4 << 21) | (1 << 7) | 0x6F;
        let op = decode(jal_insn).unwrap();
        match op {
            Op::Jal { rd, imm } => {
                assert_eq!(rd, Register::X1);
                assert_eq!(imm, 8);
            }
            _ => panic!("Expected JAL op"),
        }
    }

    #[test]
    fn decode_illegal_opcode() {
        // Opcode with bits[6:0] not matching any valid RV64I opcode we implement.
        let bad: u32 = 0x0000_0000;
        let res = decode(bad);
        match res {
            Err(Trap::IllegalInstruction(bits)) => assert_eq!(bits, bad as u64),
            _ => panic!("Expected IllegalInstruction trap"),
        }
    }

    #[test]
    fn expand_compressed_basic_integer_ops() {
        // These 16-bit encodings come from assembling with rv64imac:
        //   addi x8, x2, 16          # C.ADDI4SPN
        //   addi x11,x11,1           # C.ADDI
        //   addiw x12,x12,1          # C.ADDIW
        //   addi x13,x0,-1           # C.LI
        //   addi x2, x2, 16          # C.ADDI16SP
        //   lui  x14,1               # C.LUI
        let c_addi4spn: u16 = 0x0800;
        let c_addi: u16 = 0x0585;
        let c_addiw: u16 = 0x2605;
        let c_li: u16 = 0x56FD;
        let c_addi16sp: u16 = 0x0141;
        let c_lui: u16 = 0x6705;

        // C.ADDI4SPN -> ADDI x8, x2, 16
        let op = decode(expand_compressed(c_addi4spn).unwrap()).unwrap();
        match op {
            Op::OpImm { rd, rs1, imm, funct3, .. } => {
                assert_eq!(rd, Register::X8);
                assert_eq!(rs1, Register::X2);
                assert_eq!(imm, 16);
                assert_eq!(funct3, 0);
            }
            _ => panic!("Expected OpImm from C.ADDI4SPN"),
        }

        // C.ADDI -> ADDI x11, x11, 1
        let op = decode(expand_compressed(c_addi).unwrap()).unwrap();
        match op {
            Op::OpImm { rd, rs1, imm, .. } => {
                assert_eq!(rd, Register::X11);
                assert_eq!(rs1, Register::X11);
                assert_eq!(imm, 1);
            }
            _ => panic!("Expected OpImm from C.ADDI"),
        }

        // C.ADDIW -> ADDIW x12, x12, 1
        let op = decode(expand_compressed(c_addiw).unwrap()).unwrap();
        match op {
            Op::OpImm32 { rd, rs1, imm, .. } => {
                assert_eq!(rd, Register::X12);
                assert_eq!(rs1, Register::X12);
                assert_eq!(imm, 1);
            }
            _ => panic!("Expected OpImm32 from C.ADDIW"),
        }

        // C.LI -> ADDI x13, x0, -1
        let op = decode(expand_compressed(c_li).unwrap()).unwrap();
        match op {
            Op::OpImm { rd, rs1, imm, .. } => {
                assert_eq!(rd, Register::X13);
                assert_eq!(rs1, Register::X0);
                assert_eq!(imm, -1);
            }
            _ => panic!("Expected OpImm from C.LI"),
        }

        // C.ADDI16SP -> ADDI x2, x2, 16
        let op = decode(expand_compressed(c_addi16sp).unwrap()).unwrap();
        match op {
            Op::OpImm { rd, rs1, imm, .. } => {
                assert_eq!(rd, Register::X2);
                assert_eq!(rs1, Register::X2);
                assert_eq!(imm, 16);
            }
            _ => panic!("Expected OpImm from C.ADDI16SP"),
        }

        // C.LUI -> LUI x14, 1
        let op = decode(expand_compressed(c_lui).unwrap()).unwrap();
        match op {
            Op::Lui { rd, imm } => {
                assert_eq!(rd, Register::X14);
                assert_eq!(imm, 0x0000_0000_0000_1000);
            }
            _ => panic!("Expected Lui from C.LUI"),
        }
    }
}
