use crate::bus::Bus;
use crate::bus::DRAM_BASE;

pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    pub bus: Bus,
    pub csrs: [u64; 4096],
    pub last_pc: u64,
    pub last_inst: u32,
}

impl Cpu {
    pub fn new(bus: Bus) -> Self {
        Self {
            regs: [0; 32],
            pc: DRAM_BASE,
            bus,
            csrs: [0; 4096],
            last_pc: DRAM_BASE,
            last_inst: 0,
        }
    }

    pub fn fetch(&mut self) -> Result<u32, String> {
        match self.bus.load(self.pc, 4) {
            Ok(val) => Ok(val as u32),
            Err(_) => {
                // Optimization for edge case: if 32-bit load failed (e.g. end of memory),
                // try 16-bit load for compressed instructions.
                let val = self
                    .bus
                    .load(self.pc, 2)
                    .map_err(|e| format!("Instruction access fault at {:#x}: {}", self.pc, e))?;
                let val = val as u32;
                // If it's a 32-bit instruction (ends in 11) but we could only load 16 bits, fail.
                if val & 0x3 == 0x3 {
                    return Err(format!(
                        "Instruction access fault (32-bit instruction at end of memory) at {:#x}",
                        self.pc
                    ));
                }
                Ok(val)
            }
        }
    }

    pub fn execute(&mut self, inst: u32) -> Result<(), String> {
        // Check for Compressed Instruction (16-bit)
        // Standard instructions have bits [1:0] == 11
        if inst & 0x3 != 0x3 {
            return self.execute_compressed(inst as u16);
        }

        let opcode = inst & 0x7f;
        let rd = ((inst >> 7) & 0x1f) as usize;
        let rs1 = ((inst >> 15) & 0x1f) as usize;
        let rs2 = ((inst >> 20) & 0x1f) as usize;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;

        // Register x0 is always 0. We can enforce this by writing 0 to it after every instruction
        // or by handling it in write_reg.
        // Writing 0 to it effectively discards the result.

        match opcode {
            // LUI (Load Upper Immediate)
            0x37 => {
                let imm = (inst as u64) & 0xfffff000; // Bits 12-31
                // Sign-extension for 64-bit: result is sign-extended 32-bit value
                let imm = imm as i32 as i64 as u64;
                self.write_reg(rd, imm);
                self.pc = self.pc.wrapping_add(4);
            }
            // AUIPC (Add Upper Immediate to PC)
            0x17 => {
                let imm = (inst as u64) & 0xfffff000;
                let imm = imm as i32 as i64 as u64;
                self.write_reg(rd, self.pc.wrapping_add(imm));
                self.pc = self.pc.wrapping_add(4);
            }
            // JAL (Jump and Link)
            0x6f => {
                // J-immediate
                let imm20 = ((inst >> 31) & 0x1) as u64;
                let imm10_1 = ((inst >> 21) & 0x3ff) as u64;
                let imm11 = ((inst >> 20) & 0x1) as u64;
                let imm19_12 = ((inst >> 12) & 0xff) as u64;

                let mut imm = (imm20 << 20) | (imm19_12 << 12) | (imm11 << 11) | (imm10_1 << 1);
                // Sign extend from bit 20
                if (imm >> 20) & 1 == 1 {
                    imm |= 0xffffffff_ffe00000; // Sign extension for bits 21-63
                }

                // Store return address
                self.write_reg(rd, self.pc.wrapping_add(4));

                // Jump
                self.pc = self.pc.wrapping_add(imm);
            }
            // JALR (Jump and Link Register)
            0x67 => {
                // I-immediate
                let imm = ((inst as i32) >> 20) as i64 as u64;

                let t = self.pc.wrapping_add(4);
                let target = self.read_reg(rs1).wrapping_add(imm) & !1;

                self.write_reg(rd, t);
                self.pc = target;
            }
            // Branch Instructions (B-Type)
            0x63 => {
                let offset = Self::decode_branch_offset(inst);
                let rs1_val = self.read_reg(rs1);
                let rs2_val = self.read_reg(rs2);

                let take_branch = match funct3 {
                    0x0 => rs1_val == rs2_val,                   // BEQ
                    0x1 => rs1_val != rs2_val,                   // BNE
                    0x4 => (rs1_val as i64) < (rs2_val as i64),  // BLT
                    0x5 => (rs1_val as i64) >= (rs2_val as i64), // BGE
                    0x6 => rs1_val < rs2_val,                    // BLTU
                    0x7 => rs1_val >= rs2_val,                   // BGEU
                    _ => return Err(format!("Unimplemented branch funct3: {:x}", funct3)),
                };

                if take_branch {
                    self.pc = self.pc.wrapping_add(offset as u64);
                } else {
                    self.pc = self.pc.wrapping_add(4);
                }
            }
            // Load Instructions (I-Type)
            0x03 => {
                let imm = ((inst as i32) >> 20) as i64 as u64;
                let addr = self.read_reg(rs1).wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        // LB
                        let val = self
                            .bus
                            .load(addr, 1)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        let val = (val as i8) as i64 as u64;
                        self.write_reg(rd, val);
                    }
                    0x1 => {
                        // LH
                        let val = self
                            .bus
                            .load(addr, 2)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        let val = (val as i16) as i32 as i64 as u64;
                        self.write_reg(rd, val);
                    }
                    0x2 => {
                        // LW
                        let val = self
                            .bus
                            .load(addr, 4)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        // LW loads a 32-bit value and sign-extends it to 64-bits
                        let val = (val as i32) as i64 as u64;
                        self.write_reg(rd, val);
                    }
                    0x3 => {
                        // LD
                        let val = self
                            .bus
                            .load(addr, 8)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        self.write_reg(rd, val);
                    }
                    0x4 => {
                        // LBU
                        let val = self
                            .bus
                            .load(addr, 1)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        self.write_reg(rd, val);
                    }
                    0x5 => {
                        // LHU
                        let val = self
                            .bus
                            .load(addr, 2)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        self.write_reg(rd, val);
                    }
                    0x6 => {
                        // LWU
                        let val = self
                            .bus
                            .load(addr, 4)
                            .map_err(|e| format!("Load fault: {}", e))?;
                        self.write_reg(rd, val);
                    }
                    // Others like LB, LH, LBU, LHU not requested yet
                    _ => return Err(format!("Unimplemented load funct3: {:x}", funct3)),
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // Store Instructions (S-Type)
            0x23 => {
                // S-immediate
                let imm11_5 = (inst >> 25) & 0x7f;
                let imm4_0 = (inst >> 7) & 0x1f;
                let imm = (imm11_5 << 5) | imm4_0;
                // Sign extend 12-bit immediate
                let imm = if (imm >> 11) & 1 == 1 {
                    imm | 0xfffff000
                } else {
                    imm
                };
                let imm = (imm as i32) as i64 as u64; // Sign extend to 64 bits

                let addr = self.read_reg(rs1).wrapping_add(imm);
                let val = self.read_reg(rs2);

                match funct3 {
                    0x0 => {
                        // SB
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 1, val)
                            .map_err(|e| format!("Store fault: {}", e))?;
                    }
                    0x1 => {
                        // SH
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 2, val)
                            .map_err(|e| format!("Store fault: {}", e))?;
                    }
                    0x2 => {
                        // SW
                        self.bus
                            .store(addr, 4, val)
                            .map_err(|e| format!("Store fault: {}", e))?;
                    }
                    0x3 => {
                        // SD
                        self.bus
                            .store(addr, 8, val)
                            .map_err(|e| format!("Store fault: {}", e))?;
                    }
                    // Others like SB, SH not requested yet
                    _ => return Err(format!("Unimplemented store funct3: {:x}", funct3)),
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // Integer Register-Immediate Instructions (I-Type, RV64I)
            0x13 => {
                let imm = ((inst as i32) >> 20) as i64 as u64;
                match funct3 {
                    0x0 => {
                        // ADDI
                        let val = self.read_reg(rs1).wrapping_add(imm);
                        self.write_reg(rd, val);
                    }
                    0x1 => {
                        // SLLI
                        let shamt = imm & 0x3f;
                        let val = self.read_reg(rs1) << shamt;
                        self.write_reg(rd, val);
                    }
                    0x2 => {
                        // SLTI
                        let rs1_val = self.read_reg(rs1) as i64;
                        let imm_val = imm as i64;
                        let result = if rs1_val < imm_val { 1 } else { 0 };
                        self.write_reg(rd, result);
                    }
                    0x3 => {
                        // SLTIU
                        let rs1_val = self.read_reg(rs1);
                        let imm_val = imm;
                        let result = if rs1_val < imm_val { 1 } else { 0 };
                        self.write_reg(rd, result);
                    }
                    0x4 => {
                        // XORI
                        let val = self.read_reg(rs1) ^ imm;
                        self.write_reg(rd, val);
                    }
                    0x5 => {
                        // SRLI and SRAI
                        let shamt = imm & 0x3f;
                        if (inst >> 30) & 1 == 0 {
                            // SRLI
                            let val = self.read_reg(rs1) >> shamt;
                            self.write_reg(rd, val);
                        } else {
                            // SRAI
                            let val = (self.read_reg(rs1) as i64) >> shamt;
                            self.write_reg(rd, val as u64);
                        }
                    }
                    0x6 => {
                        // ORI
                        let val = self.read_reg(rs1) | imm;
                        self.write_reg(rd, val);
                    }
                    0x7 => {
                        // ANDI
                        let val = self.read_reg(rs1) & imm;
                        self.write_reg(rd, val);
                    }
                    _ => return Err(format!("Unimplemented OP-IMM funct3: {:x}", funct3)),
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // Integer Register-Immediate Instructions (I-Type, 32-bit ops for RV64I)
            0x1b => {
                let imm = ((inst as i32) >> 20) as i32;
                match funct3 {
                    0x0 => {
                        // ADDIW
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = rs1_val.wrapping_add(imm);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    0x1 => {
                        // SLLIW
                        let shamt = ((inst >> 20) & 0x1f) as u32;
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = rs1_val.wrapping_shl(shamt);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    0x5 => {
                        // SRLIW and SRAIW
                        let shamt = ((inst >> 20) & 0x1f) as u32;
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = if (inst >> 30) & 1 == 0 {
                            // SRLIW (logical right shift)
                            ((rs1_val as u32) >> shamt) as i32
                        } else {
                            // SRAIW (arithmetic right shift)
                            rs1_val.wrapping_shr(shamt)
                        };
                        self.write_reg(rd, result as i64 as u64);
                    }
                    _ => return Err(format!("Unimplemented OP-IMM-32 funct3: {:x}", funct3)),
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // Integer Register-Register Instructions (R-Type, RV64I)
            0x33 => {
                match (funct3, funct7) {
                    (0x0, 0x00) => {
                        // ADD
                        let val = self.read_reg(rs1).wrapping_add(self.read_reg(rs2));
                        self.write_reg(rd, val);
                    }
                    (0x0, 0x20) => {
                        // SUB
                        let val = self.read_reg(rs1).wrapping_sub(self.read_reg(rs2));
                        self.write_reg(rd, val);
                    }
                    (0x1, 0x00) => {
                        // SLL
                        let shamt = self.read_reg(rs2) & 0x3f;
                        let val = self.read_reg(rs1) << shamt;
                        self.write_reg(rd, val);
                    }
                    (0x2, 0x00) => {
                        // SLT
                        let rs1_val = self.read_reg(rs1) as i64;
                        let rs2_val = self.read_reg(rs2) as i64;
                        let result = if rs1_val < rs2_val { 1 } else { 0 };
                        self.write_reg(rd, result);
                    }
                    (0x3, 0x00) => {
                        // SLTU
                        let rs1_val = self.read_reg(rs1);
                        let rs2_val = self.read_reg(rs2);
                        let result = if rs1_val < rs2_val { 1 } else { 0 };
                        self.write_reg(rd, result);
                    }
                    (0x4, 0x00) => {
                        // XOR
                        let val = self.read_reg(rs1) ^ self.read_reg(rs2);
                        self.write_reg(rd, val);
                    }
                    (0x5, 0x00) => {
                        // SRL
                        let shamt = self.read_reg(rs2) & 0x3f;
                        let val = self.read_reg(rs1) >> shamt;
                        self.write_reg(rd, val);
                    }
                    (0x5, 0x20) => {
                        // SRA
                        let shamt = self.read_reg(rs2) & 0x3f;
                        let val = (self.read_reg(rs1) as i64) >> shamt;
                        self.write_reg(rd, val as u64);
                    }
                    (0x6, 0x00) => {
                        // OR
                        let val = self.read_reg(rs1) | self.read_reg(rs2);
                        self.write_reg(rd, val);
                    }
                    (0x7, 0x00) => {
                        // AND
                        let val = self.read_reg(rs1) & self.read_reg(rs2);
                        self.write_reg(rd, val);
                    }
                    // M Extension
                    (0x0, 0x01) => {
                        // MUL
                        let val = self.read_reg(rs1).wrapping_mul(self.read_reg(rs2));
                        self.write_reg(rd, val);
                    }
                    (0x1, 0x01) => {
                        // MULH
                        let val = (self.read_reg(rs1) as i128 * self.read_reg(rs2) as i128) >> 64;
                        self.write_reg(rd, val as u64);
                    }
                    (0x2, 0x01) => {
                        // MULHSU
                        let val =
                            (self.read_reg(rs1) as i128 * self.read_reg(rs2) as u128 as i128) >> 64;
                        self.write_reg(rd, val as u64);
                    }
                    (0x3, 0x01) => {
                        // MULHU
                        let val = (self.read_reg(rs1) as u128 * self.read_reg(rs2) as u128) >> 64;
                        self.write_reg(rd, val as u64);
                    }
                    (0x4, 0x01) => {
                        // DIV
                        let rs1_val = self.read_reg(rs1) as i64;
                        let rs2_val = self.read_reg(rs2) as i64;
                        if rs2_val == 0 {
                            self.write_reg(rd, (-1i64) as u64); // Division by zero -> -1
                        } else if rs1_val == i64::MIN && rs2_val == -1 {
                            self.write_reg(rd, rs1_val as u64); // Overflow -> MIN
                        } else {
                            self.write_reg(rd, (rs1_val / rs2_val) as u64);
                        }
                    }
                    (0x5, 0x01) => {
                        // DIVU
                        let rs2_val = self.read_reg(rs2);
                        if rs2_val == 0 {
                            self.write_reg(rd, u64::MAX);
                        } else {
                            self.write_reg(rd, self.read_reg(rs1) / rs2_val);
                        }
                    }
                    (0x6, 0x01) => {
                        // REM
                        let rs1_val = self.read_reg(rs1) as i64;
                        let rs2_val = self.read_reg(rs2) as i64;
                        if rs2_val == 0 {
                            self.write_reg(rd, rs1_val as u64);
                        } else if rs1_val == i64::MIN && rs2_val == -1 {
                            self.write_reg(rd, 0);
                        } else {
                            self.write_reg(rd, (rs1_val % rs2_val) as u64);
                        }
                    }
                    (0x7, 0x01) => {
                        // REMU
                        let rs2_val = self.read_reg(rs2);
                        if rs2_val == 0 {
                            self.write_reg(rd, self.read_reg(rs1));
                        } else {
                            self.write_reg(rd, self.read_reg(rs1) % rs2_val);
                        }
                    }
                    _ => {
                        return Err(format!(
                            "Unimplemented OP funct3: {:x} funct7: {:x}",
                            funct3, funct7
                        ));
                    }
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // Integer Register-Register Instructions (R-Type, 32-bit ops for RV64I)
            0x3b => {
                match (funct3, funct7) {
                    (0x0, 0x00) => {
                        // ADDW
                        let rs1_val = self.read_reg(rs1) as i32;
                        let rs2_val = self.read_reg(rs2) as i32;
                        let result = rs1_val.wrapping_add(rs2_val);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    (0x0, 0x20) => {
                        // SUBW
                        let rs1_val = self.read_reg(rs1) as i32;
                        let rs2_val = self.read_reg(rs2) as i32;
                        let result = rs1_val.wrapping_sub(rs2_val);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    (0x1, 0x00) => {
                        // SLLW
                        let shamt = (self.read_reg(rs2) & 0x1f) as u32;
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = rs1_val.wrapping_shl(shamt);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    (0x5, 0x00) => {
                        // SRLW
                        let shamt = (self.read_reg(rs2) & 0x1f) as u32;
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = ((rs1_val as u32) >> shamt) as i32;
                        self.write_reg(rd, result as i64 as u64);
                    }
                    (0x5, 0x20) => {
                        // SRAW
                        let shamt = (self.read_reg(rs2) & 0x1f) as u32;
                        let rs1_val = self.read_reg(rs1) as i32;
                        let result = rs1_val.wrapping_shr(shamt);
                        self.write_reg(rd, result as i64 as u64);
                    }
                    _ => {
                        return Err(format!(
                            "Unimplemented OP-32 funct3: {:x} funct7: {:x}",
                            funct3, funct7
                        ));
                    }
                }
                self.pc = self.pc.wrapping_add(4);
            }
            // SYSTEM Instructions (CSRs, etc.)
            0x73 => {
                let csr_addr = (inst >> 20) as usize;
                let uimm = rs1 as u64; // For immediate forms

                match funct3 {
                    0x1 => {
                        // CSRRW (Atomic Read/Write CSR)
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, self.read_reg(rs1));
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x2 => {
                        // CSRRS (Atomic Read and Set Bits in CSR)
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, old_val | self.read_reg(rs1));
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x3 => {
                        // CSRRC (Atomic Read and Clear Bits in CSR)
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, old_val & !self.read_reg(rs1));
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x5 => {
                        // CSRRWI
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, uimm);
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x6 => {
                        // CSRRSI
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, old_val | uimm);
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x7 => {
                        // CSRRCI
                        let old_val = self.read_csr(csr_addr);
                        self.write_csr(csr_addr, old_val & !uimm);
                        self.write_reg(rd, old_val);
                        self.pc = self.pc.wrapping_add(4);
                    }
                    0x0 => {
                        // PRIV (MRET, etc.)
                        // Check funct12 (bits 31-20)
                        // MRET: 0x302
                        match csr_addr {
                            // reusing csr_addr var which extracts top 12 bits
                            0x302 => {
                                // MRET
                                // pc = mepc
                                let mepc = self.read_csr(0x341);
                                self.pc = mepc;
                                // Don't increment PC
                            }
                            0x000 => {
                                // ECALL
                                return Err("ECALL encountered".to_string());
                            }
                            0x001 => {
                                // EBREAK
                                return Err("EBREAK encountered".to_string());
                            }
                            0x105 => {
                                // WFI (Wait for Interrupt)
                                // Treat as NOP for now, or halt until interrupt (not implemented)
                                self.pc = self.pc.wrapping_add(4);
                            }
                            _ => {
                                return Err(format!("Unimplemented PRIV instruction: {:#x}", inst));
                            }
                        }
                    }
                    _ => return Err(format!("Unimplemented SYSTEM funct3: {:x}", funct3)),
                }
            }
            _ => return Err(format!("Unimplemented opcode: {:#x}", opcode)),
        }
        Ok(())
    }

    fn execute_compressed(&mut self, inst: u16) -> Result<(), String> {
        let op = inst & 0x3;
        let funct3 = (inst >> 13) & 0x7;

        match op {
            0 => {
                // Quadrant 0
                match funct3 {
                    // C.ADDI4SPN (Add Immediate to SP, store in rd')
                    // Format: 000 | nzuimm[5:4|9:6|2|3] | rd' | 00
                    0x0 => {
                        // nzuimm bits:
                        // inst[12:5] = nzuimm[5:4|9:6|2|3]
                        // inst[12] -> 5
                        // inst[11] -> 4
                        // inst[10:7] -> 9:6
                        // inst[6] -> 2
                        // inst[5] -> 3
                        let imm = ((inst >> 6) & 1) << 2
                            | ((inst >> 5) & 1) << 3
                            | ((inst >> 11) & 0x3) << 4
                            | ((inst >> 7) & 0xf) << 6;

                        if imm == 0 {
                            // Reserved encoding – treat as NOP
                            self.pc = self.pc.wrapping_add(2);
                            return Ok(());
                        }

                        let rd_prime = ((inst >> 2) & 0x7) as usize;
                        let rd = 8 + rd_prime; // x8 to x15

                        // rd = sp + imm
                        let sp = self.read_reg(2);
                        let val = sp.wrapping_add(imm as u64);
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x2 => {
                        // C.LW
                        // inst[12:10] -> imm[5:3]
                        // inst[6] -> imm[2]
                        // inst[5] -> imm[6]
                        let imm = (((inst >> 10) & 0x7) << 3)
                            | (((inst >> 6) & 1) << 2)
                            | (((inst >> 5) & 1) << 6);

                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rd_prime = ((inst >> 2) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime;
                        let rd = 8 + rd_prime;

                        let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                        let val = self
                            .bus
                            .load(addr, 4)
                            .map_err(|e| format!("C.LW load fault: {}", e))?;
                        let val = (val as i32) as i64 as u64; // Sign-extend
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x3 => {
                        // C.LD
                        // inst[12:10] -> imm[5:3]
                        // inst[6:5] -> imm[7:6]
                        let imm = (((inst >> 10) & 0x7) << 3) | (((inst >> 5) & 0x3) << 6);

                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rd_prime = ((inst >> 2) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime;
                        let rd = 8 + rd_prime;

                        let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                        let val = self
                            .bus
                            .load(addr, 8)
                            .map_err(|e| format!("C.LD load fault: {}", e))?;
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x6 => {
                        // C.SW
                        let imm = (((inst >> 10) & 0x7) << 3)
                            | (((inst >> 6) & 1) << 2)
                            | (((inst >> 5) & 1) << 6);

                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rs2_prime = ((inst >> 2) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime;
                        let rs2 = 8 + rs2_prime;

                        let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 4, val)
                            .map_err(|e| format!("C.SW store fault: {}", e))?;
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x7 => {
                        // C.SD
                        let imm = (((inst >> 10) & 0x7) << 3) | (((inst >> 5) & 0x3) << 6);

                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rs2_prime = ((inst >> 2) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime;
                        let rs2 = 8 + rs2_prime;

                        let addr = self.read_reg(rs1).wrapping_add(imm as u64);
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 8, val)
                            .map_err(|e| format!("C.SD store fault: {}", e))?;
                        self.pc = self.pc.wrapping_add(2);
                    }
                    _ => {
                        // Unknown / unimplemented compressed instruction in quadrant 0 – treat as NOP
                        self.pc = self.pc.wrapping_add(2);
                    }
                }
            }
            1 => {
                // Quadrant 1
                match funct3 {
                    0x0 => {
                        // C.ADDI / C.NOP
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        // imm[5]=12, imm[4:0]=6:2
                        let imm5 = (inst >> 12) & 1;
                        let imm4_0 = (inst >> 2) & 0x1f;
                        let mut imm_val = ((imm5 << 5) | imm4_0) as u32;
                        // Sign extend 6-bit
                        if (imm_val >> 5) & 1 == 1 {
                            imm_val |= 0xffffffc0;
                        }
                        let imm = imm_val as i32 as i64;

                        if rd != 0 {
                            let val = self.read_reg(rd).wrapping_add(imm as u64);
                            self.write_reg(rd, val);
                        }
                        // if rd==0, it's NOP
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x1 => {
                        // C.ADDIW
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd == 0 {
                            // Reserved
                            return Err(format!("Reserved C.ADDIW with rd=0: {:#x}", inst));
                        } else {
                            let imm5 = (inst >> 12) & 1;
                            let imm4_0 = (inst >> 2) & 0x1f;
                            let mut imm_val = ((imm5 << 5) | imm4_0) as u32;
                            if (imm_val >> 5) & 1 == 1 {
                                imm_val |= 0xffffffc0;
                            }
                            let imm = imm_val as i32 as i64;

                            let val = self.read_reg(rd).wrapping_add(imm as u64);
                            let val = (val as i32) as i64 as u64; // Sign-extend 32-bit result
                            self.write_reg(rd, val);
                        }
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x2 => {
                        // C.LI
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd != 0 {
                            let imm5 = (inst >> 12) & 1;
                            let imm4_0 = (inst >> 2) & 0x1f;
                            let mut imm_val = ((imm5 << 5) | imm4_0) as u32;
                            if (imm_val >> 5) & 1 == 1 {
                                imm_val |= 0xffffffc0;
                            }
                            let imm = imm_val as i32 as i64 as u64;
                            self.write_reg(rd, imm);
                        }
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x3 => {
                        // C.LUI / C.ADDI16SP
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd == 2 {
                            // C.ADDI16SP
                            // imm bits in inst: 12, 6, 5, 4, 3, 2.
                            // Mapping:
                            // 12 -> 9
                            // 4:3 -> 8:7
                            // 5 -> 6
                            // 2 -> 5
                            // 6 -> 4
                            let mut imm_val = (((inst >> 12) & 1) << 9
                                | ((inst >> 3) & 0x3) << 7
                                | ((inst >> 5) & 1) << 6
                                | ((inst >> 2) & 1) << 5
                                | ((inst >> 6) & 1) << 4)
                                as u32;
                            // Sign extend from bit 9
                            if (imm_val >> 9) & 1 == 1 {
                                imm_val |= 0xfffffc00;
                            }
                            let imm = imm_val as i32 as i64; // Already scaled (bits are at 9..4)

                            // println!("C.ADDI16SP inst={:#x} imm={}", inst, imm);
                            let val = self.read_reg(2).wrapping_add(imm as u64);
                            self.write_reg(2, val);
                        } else if rd != 0 {
                            // C.LUI
                            let imm5 = (inst >> 12) & 1;
                            let imm4_0 = (inst >> 2) & 0x1f;
                            let mut imm = ((imm5 << 5) | imm4_0) as u32;
                            if (imm >> 5) & 1 == 1 {
                                imm |= 0xffffffc0; // sign extend to 32-bit
                            }
                            let val = (imm as i32 as i64 as u64) << 12;
                            self.write_reg(rd, val);
                        }
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x5 => {
                        // C.J
                        // offset[11]=12, offset[4]=11, offset[9:8]=10:9, offset[10]=8, offset[6]=7, offset[7]=6, offset[3:1]=5:3, offset[5]=2
                        let offset = ((inst >> 12) & 1) << 11
                            | ((inst >> 11) & 1) << 4
                            | ((inst >> 9) & 0x3) << 8
                            | ((inst >> 8) & 1) << 10
                            | ((inst >> 7) & 1) << 6
                            | ((inst >> 6) & 1) << 7
                            | ((inst >> 3) & 0x7) << 1
                            | ((inst >> 2) & 1) << 5;
                        // Sign extend 12-bit
                        let mut off_val = offset as u64;
                        if (offset >> 11) & 1 == 1 {
                            off_val |= 0xfffffffffffff800;
                        }
                        self.pc = self.pc.wrapping_add(off_val);
                    }
                    0x6 => {
                        // C.BEQZ
                        // offset[8]=12, offset[4:3]=11:10, offset[7:6]=6:5, offset[2:1]=4:3, offset[5]=2
                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime; // x8-x15

                        let off_8 = (inst >> 12) & 1;
                        let off_4_3 = (inst >> 10) & 0x3;
                        let off_7_6 = (inst >> 5) & 0x3;
                        let off_2_1 = (inst >> 3) & 0x3;
                        let off_5 = (inst >> 2) & 1;

                        let offset_val = ((off_8 << 8)
                            | (off_7_6 << 6)
                            | (off_5 << 5)
                            | (off_4_3 << 3)
                            | (off_2_1 << 1)) as u32;
                        // Sign extend 9-bit (bit 8 is sign)
                        let offset = if off_8 == 1 {
                            (offset_val | 0xffffff00) as i32 as i64
                        } else {
                            offset_val as i64
                        };

                        if self.read_reg(rs1) == 0 {
                            self.pc = self.pc.wrapping_add(offset as u64);
                        } else {
                            self.pc = self.pc.wrapping_add(2);
                        }
                    }
                    0x7 => {
                        // C.BNEZ
                        let rs1_prime = ((inst >> 7) & 0x7) as usize;
                        let rs1 = 8 + rs1_prime;

                        let off_8 = (inst >> 12) & 1;
                        let off_4_3 = (inst >> 10) & 0x3;
                        let off_7_6 = (inst >> 5) & 0x3;
                        let off_2_1 = (inst >> 3) & 0x3;
                        let off_5 = (inst >> 2) & 1;

                        let offset_val = ((off_8 << 8)
                            | (off_7_6 << 6)
                            | (off_5 << 5)
                            | (off_4_3 << 3)
                            | (off_2_1 << 1)) as u32;
                        let offset = if off_8 == 1 {
                            (offset_val | 0xffffff00) as i32 as i64
                        } else {
                            offset_val as i64
                        };

                        if self.read_reg(rs1) != 0 {
                            self.pc = self.pc.wrapping_add(offset as u64);
                        } else {
                            self.pc = self.pc.wrapping_add(2);
                        }
                    }
                    _ => {
                        // Unknown / unimplemented compressed instruction in quadrant 1 – treat as NOP
                        self.pc = self.pc.wrapping_add(2);
                    }
                }
            }
            2 => {
                // Quadrant 2
                match funct3 {
                    0x0 => {
                        // C.SLLI
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd == 0 {
                            // Reserved encoding – treat as NOP
                            self.pc = self.pc.wrapping_add(2);
                            return Ok(());
                        }
                        let shamt = ((inst >> 2) & 0x1f) | (((inst >> 12) & 1) << 5);
                        if shamt == 0 {
                            // Hint or reserved – treat as NOP
                            self.pc = self.pc.wrapping_add(2);
                            return Ok(());
                        }
                        let val = self.read_reg(rd) << shamt;
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x2 => {
                        // C.LWSP
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd == 0 {
                            // Reserved encoding – treat as NOP
                            self.pc = self.pc.wrapping_add(2);
                            return Ok(());
                        }
                        // offset[5] = inst[12]
                        // offset[4:2] = inst[6:4]
                        // offset[7:6] = inst[3:2]
                        let off_5 = (inst >> 12) & 1;
                        let off_4_2 = (inst >> 4) & 0x7;
                        let off_7_6 = (inst >> 2) & 0x3;
                        let offset = (off_7_6 << 6) | (off_5 << 5) | (off_4_2 << 2);
                        let addr = self.read_reg(2).wrapping_add(offset as u64); // x2 is SP

                        let val = self
                            .bus
                            .load(addr, 4)
                            .map_err(|e| format!("C.LWSP load fault: {}", e))?;
                        // Sign extend 32-bit loaded value
                        let val = (val as i32) as i64 as u64;
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x3 => {
                        // C.LDSP
                        let rd = ((inst >> 7) & 0x1f) as usize;
                        if rd == 0 {
                            // Reserved encoding – treat as NOP
                            self.pc = self.pc.wrapping_add(2);
                            return Ok(());
                        }
                        // offset[5] = inst[12]
                        // offset[4:3] = inst[6:5]
                        // offset[8:6] = inst[4:2]
                        let off_5 = (inst >> 12) & 1;
                        let off_4_3 = (inst >> 5) & 0x3;
                        let off_8_6 = (inst >> 2) & 0x7;
                        let offset = (off_8_6 << 6) | (off_5 << 5) | (off_4_3 << 3);
                        let addr = self.read_reg(2).wrapping_add(offset as u64);

                        let val = self
                            .bus
                            .load(addr, 8)
                            .map_err(|e| format!("C.LDSP load fault: {}", e))?;
                        self.write_reg(rd, val);
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x4 => {
                        // C.JR, C.MV, C.EBREAK, C.JALR, C.ADD
                        let bit12 = (inst >> 12) & 1;
                        let rs1_rd = ((inst >> 7) & 0x1f) as usize; // bits 11-7
                        let rs2 = ((inst >> 2) & 0x1f) as usize; // bits 6-2

                        if bit12 == 0 {
                            if rs2 == 0 {
                                // C.JR
                                if rs1_rd == 0 {
                                    return Err(format!(
                                        "Reserved instruction C.JR with rs1=0: {:#x}",
                                        inst
                                    ));
                                }
                                let target = self.read_reg(rs1_rd);
                                self.pc = target;
                                // Don't increment PC by 2, we jumped.
                            } else {
                                // C.MV
                                if rs1_rd != 0 {
                                    let val = self.read_reg(rs2);
                                    self.write_reg(rs1_rd, val);
                                }
                                self.pc = self.pc.wrapping_add(2);
                            }
                        } else {
                            if rs2 == 0 {
                                // C.EBREAK or C.JALR
                                if rs1_rd == 0 {
                                    // C.EBREAK
                                    return Err(format!("C.EBREAK encountered: {:#x}", inst));
                                } else {
                                    // C.JALR
                                    let target = self.read_reg(rs1_rd);
                                    self.write_reg(1, self.pc.wrapping_add(2)); // link to next instruction
                                    self.pc = target;
                                }
                            } else {
                                // C.ADD
                                if rs1_rd != 0 {
                                    let val =
                                        self.read_reg(rs1_rd).wrapping_add(self.read_reg(rs2));
                                    self.write_reg(rs1_rd, val);
                                }
                                self.pc = self.pc.wrapping_add(2);
                            }
                        }
                    }
                    0x6 => {
                        // C.SWSP
                        // offset[5:2] = inst[12:9]
                        // offset[7:6] = inst[8:7]
                        // rd is actually rs2 bits 6:2
                        let rs2 = ((inst >> 2) & 0x1f) as usize;
                        let off_5_2 = (inst >> 9) & 0xf;
                        let off_7_6 = (inst >> 7) & 0x3;
                        let offset = (off_7_6 << 6) | (off_5_2 << 2);

                        let addr = self.read_reg(2).wrapping_add(offset as u64);
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 4, val)
                            .map_err(|e| format!("C.SWSP store fault: {}", e))?;
                        self.pc = self.pc.wrapping_add(2);
                    }
                    0x7 => {
                        // C.SDSP
                        // offset[5:3] = inst[12:10]
                        // offset[8:6] = inst[9:7]
                        // rs2 is bits 6:2
                        let rs2 = ((inst >> 2) & 0x1f) as usize;
                        let off_5_3 = (inst >> 10) & 0x7;
                        let off_8_6 = (inst >> 7) & 0x7;
                        let offset = (off_8_6 << 6) | (off_5_3 << 3);

                        let addr = self.read_reg(2).wrapping_add(offset as u64);
                        let val = self.read_reg(rs2);
                        self.bus
                            .store(addr, 8, val)
                            .map_err(|e| format!("C.SDSP store fault: {}", e))?;
                        self.pc = self.pc.wrapping_add(2);
                    }
                    _ => {
                        // Unknown / unimplemented compressed instruction in quadrant 2 – treat as NOP
                        self.pc = self.pc.wrapping_add(2);
                    }
                }
            }
            _ => {
                // Unknown compressed instruction – treat as NOP
                self.pc = self.pc.wrapping_add(2);
            }
        }
        Ok(())
    }

    pub fn step(&mut self) -> Result<(), String> {
        let inst = self.fetch()?;
        self.last_pc = self.pc;
        self.last_inst = inst;
        let res = self.execute(inst);
        if res.is_ok() {
            // Increment cycle CSR (0xC00) as a simple cycle counter per executed instruction
            let idx = 0xC00 & 0xFFF;
            self.csrs[idx] = self.csrs[idx].wrapping_add(1);
        }
        res
    }

    fn read_reg(&self, reg: usize) -> u64 {
        if reg == 0 { 0 } else { self.regs[reg] }
    }

    fn write_reg(&mut self, reg: usize, val: u64) {
        if reg != 0 {
            self.regs[reg] = val;
        }
    }

    fn read_csr(&self, addr: usize) -> u64 {
        self.csrs[addr & 0xfff]
    }

    fn write_csr(&mut self, addr: usize, val: u64) {
        self.csrs[addr & 0xfff] = val;
    }

    fn decode_branch_offset(inst: u32) -> i64 {
        let imm12 = ((inst >> 31) & 0x1) as i64;
        let imm11 = ((inst >> 7) & 0x1) as i64;
        let imm10_5 = ((inst >> 25) & 0x3f) as i64;
        let imm4_1 = ((inst >> 8) & 0xf) as i64;

        let mut offset = (imm12 << 12) | (imm11 << 11) | (imm10_5 << 5) | (imm4_1 << 1);
        // Sign-extend the 13-bit immediate to 64 bits
        offset = (offset << 51) >> 51;
        offset
    }

    pub fn dump_regs(&self) {
        println!("PC: {:#x}", self.pc);
        for i in 0..32 {
            println!("x{}: {:#x}", i, self.regs[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Bus, DRAM_BASE};

    #[test]
    fn test_add_instructions() {
        let mut bus = Bus::new();

        // Instructions:
        // addi x1, x0, 1  -> 00100093 (opcode 0x13, rd=1, rs1=0, imm=1, funct3=0)
        // addi x2, x0, 1  -> 00100113 (opcode 0x13, rd=2, rs1=0, imm=1, funct3=0)
        // add x3, x1, x2  -> 002081b3 (opcode 0x33, rd=3, rs1=1, rs2=2, funct3=0, funct7=0)

        // Little endian encoding:
        // 00100093 -> 93 00 10 00
        // 00100113 -> 13 01 10 00
        // 002081b3 -> b3 81 20 00

        let code = vec![
            0x93, 0x00, 0x10, 0x00, 0x13, 0x01, 0x10, 0x00, 0xb3, 0x81, 0x20, 0x00,
        ];

        bus.initialize_dram(&code)
            .expect("Failed to initialize DRAM");

        let mut cpu = Cpu::new(bus);

        // Run 3 steps
        cpu.step().expect("Step 1 failed"); // addi x1, x0, 1
        cpu.step().expect("Step 2 failed"); // addi x2, x0, 1
        cpu.step().expect("Step 3 failed"); // add x3, x1, x2

        assert_eq!(cpu.read_reg(1), 1);
        assert_eq!(cpu.read_reg(2), 1);
        assert_eq!(cpu.read_reg(3), 2);
    }

    #[test]
    fn test_memory_and_jumps() {
        let mut bus = Bus::new();

        // 0: LUI x2, 0x80000     -> x2 = 0xffffffff80000000
        //    0x80000137 -> 37 01 00 80

        // 4: SLLI x2, x2, 32     -> x2 = 0x8000000000000000
        //    0x02011113 -> 13 11 01 02

        // 8: SRLI x2, x2, 32     -> x2 = 0x0000000080000000
        //    0x02015113 -> 13 51 01 02

        // 12: ADDI x1, x0, 42    -> x1 = 42
        //    0x02a00093 -> 93 00 a0 02

        // 16: SW x1, 0(x2)       -> mem[0x80000000] = 42
        //    0x00112023 -> 23 20 11 00

        // 20: LW x3, 0(x2)       -> x3 = 42
        //     0x00012183 -> 83 21 01 00

        // 24: JAL x0, 8          -> pc += 8. Skip next instruction.
        //     0x0080006f -> 6f 00 80 00

        // 28: ADDI x3, x3, 1     (Should be skipped)
        //     0x00118193 -> 93 81 11 00

        // 32: ADDI x3, x3, 2     (Target)
        //     0x00218193 -> 93 81 21 00

        let code = vec![
            0x37, 0x01, 0x00, 0x80, 0x13, 0x11, 0x01, 0x02, 0x13, 0x51, 0x01, 0x02, 0x93, 0x00,
            0xa0, 0x02, 0x23, 0x20, 0x11, 0x00, 0x83, 0x21, 0x01, 0x00, 0x6f, 0x00, 0x80, 0x00,
            0x93, 0x81, 0x11, 0x00, 0x93, 0x81, 0x21, 0x00,
        ];

        bus.initialize_dram(&code)
            .expect("Failed to initialize DRAM");
        let mut cpu = Cpu::new(bus);

        // Execute steps
        cpu.step().expect("Step 1 (LUI) failed");
        assert_eq!(cpu.read_reg(2), 0xffffffff80000000);

        cpu.step().expect("Step 2 (SLLI) failed");
        assert_eq!(cpu.read_reg(2), 0x8000000000000000);

        cpu.step().expect("Step 3 (SRLI) failed");
        assert_eq!(cpu.read_reg(2), 0x80000000);

        cpu.step().expect("Step 4 (ADDI) failed");
        assert_eq!(cpu.read_reg(1), 42);

        cpu.step().expect("Step 5 (SW) failed");

        cpu.step().expect("Step 6 (LW) failed");
        assert_eq!(cpu.read_reg(3), 42);

        cpu.step().expect("Step 7 (JAL) failed");
        // PC should be 24 + 8 = 32. (0x80000020)
        assert_eq!(cpu.pc, DRAM_BASE + 32);

        cpu.step().expect("Step 8 (ADDI target) failed");
        // x3 was 42. +2 = 44.
        assert_eq!(cpu.read_reg(3), 44);
    }

    #[test]
    fn test_branch_instructions() {
        fn encode_branch(funct3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
            assert_eq!(imm % 2, 0, "Branch immediate must be 2-byte aligned");
            assert!(
                (-4096..=4094).contains(&imm),
                "Immediate out of 13-bit branch range"
            );
            let imm_u = (imm as u32) & 0x1fff;
            let imm12 = ((imm_u >> 12) & 0x1) << 31;
            let imm10_5 = ((imm_u >> 5) & 0x3f) << 25;
            let imm4_1 = ((imm_u >> 1) & 0xf) << 8;
            let imm11 = ((imm_u >> 11) & 0x1) << 7;

            imm12 | imm10_5 | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | imm4_1 | imm11 | 0x63
        }

        let bus = Bus::new();
        let mut cpu = Cpu::new(bus);

        // BEQ: taken
        cpu.pc = DRAM_BASE;
        cpu.regs[1] = 5;
        cpu.regs[2] = 5;
        let beq = encode_branch(0x0, 1, 2, 8);
        cpu.execute(beq).expect("BEQ failed");
        assert_eq!(cpu.pc, DRAM_BASE + 8);

        // BNE: taken with negative offset
        cpu.pc = DRAM_BASE + 16;
        cpu.regs[1] = 1;
        cpu.regs[2] = 2;
        let bne = encode_branch(0x1, 1, 2, -8);
        cpu.execute(bne).expect("BNE failed");
        assert_eq!(cpu.pc, DRAM_BASE + 8);

        // BLT: signed comparison
        cpu.pc = DRAM_BASE;
        cpu.regs[3] = (-1i64) as u64;
        cpu.regs[4] = 1;
        let blt = encode_branch(0x4, 3, 4, 12);
        cpu.execute(blt).expect("BLT failed");
        assert_eq!(cpu.pc, DRAM_BASE + 12);

        // BGE: not taken (signed)
        cpu.pc = DRAM_BASE;
        cpu.regs[5] = (-1i64) as u64;
        cpu.regs[6] = 10;
        let bge = encode_branch(0x5, 5, 6, 12);
        cpu.execute(bge).expect("BGE failed");
        assert_eq!(cpu.pc, DRAM_BASE + 4);

        // BLTU: unsigned comparison taken
        cpu.pc = DRAM_BASE;
        cpu.regs[7] = 1;
        cpu.regs[8] = 2;
        let bltu = encode_branch(0x6, 7, 8, 10);
        cpu.execute(bltu).expect("BLTU failed");
        assert_eq!(cpu.pc, DRAM_BASE + 10);

        // BGEU: unsigned comparison taken
        cpu.pc = DRAM_BASE;
        cpu.regs[9] = 10;
        cpu.regs[10] = 1;
        let bgeu = encode_branch(0x7, 9, 10, 14);
        cpu.execute(bgeu).expect("BGEU failed");
        assert_eq!(cpu.pc, DRAM_BASE + 14);
    }
}
