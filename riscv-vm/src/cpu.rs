use crate::bus::Bus;
use crate::clint::{CLINT_BASE, MTIME_OFFSET};
use crate::csr::{
    Mode, CSR_MCAUSE, CSR_MEPC, CSR_MISA, CSR_MSTATUS, CSR_MTVEC, CSR_MTVAL, CSR_MIDELEG,
    CSR_MIE, CSR_MIP, CSR_SATP, CSR_MEDELEG, CSR_STVEC, CSR_SEPC, CSR_SCAUSE, CSR_STVAL, CSR_SIE,
    CSR_SSTATUS, CSR_SIP, CSR_TIME, CSR_MENVCFG, CSR_STIMECMP,
};
use crate::decoder::{self, Op, Register};
use crate::mmu::{self, AccessType as MmuAccessType, Tlb};
use crate::Trap;
use std::collections::HashMap;

pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    /// Reservation set address for LR/SC (granule-aligned), or None if no reservation.
    reservation: Option<u64>,
    /// Simple CSR storage for Zicsr (12-bit CSR address space).
    csrs: [u64; 4096],
    /// Current privilege mode (Machine/Supervisor/User).
    pub mode: Mode,
    /// Per-hart TLB for Sv39/Sv48 translation.
    pub tlb: Tlb,
}

impl Cpu {
    pub fn new(pc: u64) -> Self {
        let mut csrs = [0u64; 4096];
        // misa: rv64imac_zicsr_zifencei (value from phase-0.md)
        const MISA_RV64IMAC_ZICSR_ZIFENCEI: u64 = 0x4000_0000_0018_1125;
        csrs[CSR_MISA as usize] = MISA_RV64IMAC_ZICSR_ZIFENCEI;

        // mstatus initial value: all zeros except UXL/SXL can be left as 0 (WARL).
        csrs[CSR_MSTATUS as usize] = 0;

        Self {
            regs: [0; 32],
            pc,
            reservation: None,
            csrs,
            mode: Mode::Machine,
            tlb: Tlb::new(),
        }
    }

    /// Export the current CSR image into a compact map suitable for
    /// serialization in snapshots.
    pub fn export_csrs(&self) -> HashMap<u16, u64> {
        let mut map = HashMap::new();
        for (idx, &val) in self.csrs.iter().enumerate() {
            if val != 0 {
                map.insert(idx as u16, val);
            }
        }
        map
    }

    /// Restore CSRs from a previously exported map.
    ///
    /// Any CSR not present in the map is reset to 0. This is intentionally
    /// low-level and bypasses architectural WARL checks; it is only used for
    /// snapshot/restore.
    pub fn import_csrs(&mut self, map: &HashMap<u16, u64>) {
        self.csrs = [0u64; 4096];
        for (&addr, &val) in map.iter() {
            let idx = addr as usize;
            if idx < self.csrs.len() {
                self.csrs[idx] = val;
            }
        }
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

    fn reservation_granule(addr: u64) -> u64 {
        const GRANULE: u64 = 64;
        addr & !(GRANULE - 1)
    }

    fn clear_reservation_if_conflict(&mut self, addr: u64) {
        if let Some(res) = self.reservation {
            if Self::reservation_granule(res) == Self::reservation_granule(addr) {
                self.reservation = None;
            }
        }
    }

    pub fn read_csr(&self, addr: u16) -> Result<u64, Trap> {
        // Privilege checks per RISC-V privileged spec:
        // CSR address bits [9:8] encode the lowest privilege level that can access:
        //   00 = User, 01 = Supervisor, 10 = Hypervisor (reserved), 11 = Machine
        let required_priv = (addr >> 8) & 0x3;
        let current_priv = match self.mode {
            Mode::User => 0,
            Mode::Supervisor => 1,
            Mode::Machine => 3,
        };
        if current_priv < required_priv {
            return Err(Trap::IllegalInstruction(addr as u64));
        }

        match addr {
            CSR_SSTATUS => {
                let mstatus = self.csrs[CSR_MSTATUS as usize];
                // Mask for sstatus view: SIE(1), SPIE(5), SPP(8), FS(13:14), XS(15:16), SUM(18), MXR(19), UXL(32:33), SD(63)
                // Simplified mask for this emulator:
                let mask = (1 << 1) | (1 << 5) | (1 << 8) | (3 << 13) | (1 << 18) | (1 << 19);
                Ok(mstatus & mask)
            }
            CSR_SIE => {
                let mie = self.csrs[CSR_MIE as usize];
                // Mask delegated interrupts: SSIP(1), STIP(5), SEIP(9)
                let mask = (1 << 1) | (1 << 5) | (1 << 9);
                Ok(mie & mask)
            }
            CSR_SIP => {
                let mip = self.csrs[CSR_MIP as usize];
                let mask = (1 << 1) | (1 << 5) | (1 << 9);
                Ok(mip & mask)
            }
            _ => Ok(self.csrs[addr as usize]),
        }
    }

    pub fn write_csr(&mut self, addr: u16, val: u64) -> Result<(), Trap> {
        // Read-only CSRs have bits [11:10] == 0b11
        let read_only = (addr >> 10) & 0x3 == 0x3;
        if read_only {
            // Writes to read-only CSRs are ignored (WARL behavior for some, illegal for others)
            // For simplicity, we just ignore the write
            return Ok(());
        }
        
        // Privilege checks per RISC-V privileged spec:
        // CSR address bits [9:8] encode the lowest privilege level that can access
        let required_priv = (addr >> 8) & 0x3;
        let current_priv = match self.mode {
            Mode::User => 0,
            Mode::Supervisor => 1,
            Mode::Machine => 3,
        };
        if current_priv < required_priv {
            return Err(Trap::IllegalInstruction(addr as u64));
        }

        match addr {
            CSR_SSTATUS => {
                let mut mstatus = self.csrs[CSR_MSTATUS as usize];
                let mask = (1 << 1) | (1 << 5) | (1 << 8) | (3 << 13) | (1 << 18) | (1 << 19);
                mstatus = (mstatus & !mask) | (val & mask);
                self.csrs[CSR_MSTATUS as usize] = mstatus;
                Ok(())
            }
            CSR_SIE => {
                let mut mie = self.csrs[CSR_MIE as usize];
                let mask = (1 << 1) | (1 << 5) | (1 << 9);
                mie = (mie & !mask) | (val & mask);
                self.csrs[CSR_MIE as usize] = mie;
                Ok(())
            }
            CSR_SIP => {
                let mut mip = self.csrs[CSR_MIP as usize];
                // Only SSIP is writable in SIP
                let mask = 1 << 1;
                mip = (mip & !mask) | (val & mask);
                self.csrs[CSR_MIP as usize] = mip;
                Ok(())
            }
            _ => {
                self.csrs[addr as usize] = val;
                Ok(())
            }
        }
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

    fn handle_trap<T>(&mut self, trap: Trap, pc: u64, _insn_raw: Option<u32>) -> Result<T, Trap> {
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
    fn translate_addr(
        &mut self,
        bus: &mut dyn Bus,
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

    fn fetch_and_expand(&mut self, bus: &mut dyn Bus) -> Result<(u32, u8), Trap> {
        let pc = self.pc;
        if pc % 2 != 0 {
            return self.handle_trap(Trap::InstructionAddressMisaligned(pc), pc, None);
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

    fn check_pending_interrupt(&self) -> Option<Trap> {
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
            if (m_pending & (1 << 11)) != 0 { return Some(Trap::MachineExternalInterrupt); } // MEIP
            if (m_pending & (1 << 3)) != 0 { return Some(Trap::MachineSoftwareInterrupt); }   // MSIP
            if (m_pending & (1 << 7)) != 0 { return Some(Trap::MachineTimerInterrupt); }      // MTIP
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
                 log::trace!("Interrupt pending: s_pending={:x} mstatus={:x} mode={:?}", s_pending, mstatus, self.mode);
             }
        }

        if s_enabled {
            if (s_pending & (1 << 9)) != 0 { return Some(Trap::SupervisorExternalInterrupt); } // SEIP
            if (s_pending & (1 << 1)) != 0 { return Some(Trap::SupervisorSoftwareInterrupt); } // SSIP
            if (s_pending & (1 << 5)) != 0 { return Some(Trap::SupervisorTimerInterrupt); }    // STIP
        }

        None
    }

    pub fn step(&mut self, bus: &mut dyn Bus) -> Result<(), Trap> {
        // Poll device-driven interrupts into MIP mask.
        let mut hw_mip = bus.poll_interrupts();

        // Sstc support: raise STIP (bit 5) when time >= stimecmp and Sstc enabled.
        // menvcfg[63] gate is optional; xv6 enables it.
        let menvcfg = self.csrs[CSR_MENVCFG as usize];
        let sstc_enabled = ((menvcfg >> 63) & 1) == 1;
        let stimecmp = self.csrs[CSR_STIMECMP as usize];
        if sstc_enabled && stimecmp != 0 {
            // Read CLINT MTIME directly (physical address).
            if let Ok(now) = bus.read64(CLINT_BASE + MTIME_OFFSET) {
                if now >= stimecmp {
                    hw_mip |= 1 << 5; // STIP
                }
            }
        }

        // Update MIP: preserve software-writable bits (SSIP=bit1, STIP=bit5 if not Sstc),
        // but always update hardware-driven bits (MSIP=3, MTIP=7, SEIP=9, MEIP=11).
        // SSIP (bit 1) is software-writable and should be preserved.
        // STIP (bit 5) is normally read-only but Sstc makes it hardware-driven.
        let hw_bits: u64 = (1 << 3) | (1 << 7) | (1 << 9) | (1 << 11); // MSIP, MTIP, SEIP, MEIP
        let hw_bits_with_stip: u64 = hw_bits | (1 << 5); // Include STIP when Sstc enabled
        
        let mask = if sstc_enabled { hw_bits_with_stip } else { hw_bits };
        let old_mip = self.csrs[CSR_MIP as usize];
        self.csrs[CSR_MIP as usize] = (old_mip & !mask) | (hw_mip & mask);
        
        if let Some(trap) = self.check_pending_interrupt() {
            return self.handle_trap(trap, self.pc, None);
        }

        let pc = self.pc;
        // Fetch (supports compressed 16-bit and regular 32-bit instructions)
        let (insn_raw, insn_len) = self.fetch_and_expand(bus)?;
        // Decode
        let op = match decoder::decode(insn_raw) {
            Ok(v) => v,
            Err(trap) => return self.handle_trap(trap, pc, Some(insn_raw)),
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
                        )
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
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read8(pa) {
                        Ok(v) => (v as i8) as i64 as u64, // LB
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    1 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read16(pa) {
                        Ok(v) => (v as i16) as i64 as u64, // LH
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    2 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read32(pa) {
                        Ok(v) => (v as i32) as i64 as u64, // LW
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    3 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read64(pa) {
                        Ok(v) => v, // LD
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    4 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read8(pa) {
                        Ok(v) => v as u64, // LBU
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    5 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read16(pa) {
                        Ok(v) => v as u64, // LHU
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    6 => {
                        let pa = self.translate_addr(bus, addr, MmuAccessType::Load, pc, Some(insn_raw))?;
                        match bus.read32(pa) {
                        Ok(v) => v as u64, // LWU
                        Err(e) => return self.handle_trap(e, pc, Some(insn_raw)),
                    }}
                    _ => {
                        return self.handle_trap(
                            Trap::IllegalInstruction(insn_raw as u64),
                            pc,
                            Some(insn_raw),
                        )
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
                let pa = self.translate_addr(bus, addr, MmuAccessType::Store, pc, Some(insn_raw))?;
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
                        )
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
                        )
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
                    (6, 0x00) => val1 | val2,                              // OR
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
                        )
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
                        )
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
                        )
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
                        )
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
                    // AMO* operations
                    0b00001 | // AMOSWAP
                    0b00000 | // AMOADD
                    0b00100 | // AMOXOR
                    0b01000 | // AMOOR
                    0b01100 | // AMOAND
                    0b10000 | // AMOMIN
                    0b10100 | // AMOMAX
                    0b11000 | // AMOMINU
                    0b11100 // AMOMAXU
                    => {
                        // Any AMO acts like a store to the address, so clear reservation.
                        self.clear_reservation_if_conflict(addr);

                        let old = if is_word {
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
                        let rs2_val = self.read_reg(rs2);

                        let new_val = match funct5 {
                            0b00001 => rs2_val,                        // AMOSWAP
                            0b00000 => old.wrapping_add(rs2_val),      // AMOADD
                            0b00100 => old ^ rs2_val,                  // AMOXOR
                            0b01000 => old | rs2_val,                  // AMOOR
                            0b01100 => old & rs2_val,                  // AMOAND
                            0b10000 => {
                                // AMOMIN (signed)
                                let a = old as i64;
                                let b = rs2_val as i64;
                                if a < b { old } else { rs2_val }
                            }
                            0b10100 => {
                                // AMOMAX (signed)
                                let a = old as i64;
                                let b = rs2_val as i64;
                                if a > b { old } else { rs2_val }
                            }
                            0b11000 => {
                                // AMOMINU (unsigned)
                                if old < rs2_val { old } else { rs2_val }
                            }
                            0b11100 => {
                                // AMOMAXU (unsigned)
                                if old > rs2_val { old } else { rs2_val }
                            }
                            _ => unreachable!(),
                        };

                        let res = if is_word {
                            bus.write32(pa, new_val as u32)
                        } else {
                            bus.write64(pa, new_val)
                        };
                        if let Err(e) = res {
                            return self.handle_trap(e, pc, Some(insn_raw));
                        }

                        // rd receives the original loaded value (sign-extended to XLEN)
                        self.write_reg(rd, old);
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
                        } else {
                            match insn_raw {
                                0x0010_0073 => {
                                    // EBREAK
                                    return self.handle_trap(Trap::Breakpoint, pc, Some(insn_raw));
                                }
                                0x1050_0073 => {
                                    // WFI - treat as a hint NOP
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SystemBus;

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
        (imm11_5 << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (funct3 << 12)
            | (imm4_0 << 7)
            | opcode
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
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // ADDI x1, x0, -1
        let insn = encode_i(-1, 0, 0, 1, 0x13);
        bus.write32(0x8000_0000, insn).unwrap();

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(cpu.pc, 0x8000_0004);
    }

    #[test]
    fn test_lui() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // LUI x2, 0x12345
        // imm field is already << 12 in the encoding helper
        let imm = 0x12345 << 12;
        let insn = ((imm as u32) & 0xFFFFF000) | (2 << 7) | 0x37;
        bus.write32(0x8000_0000, insn).unwrap();

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X2), 0x0000_0000_1234_5000);
    }

    #[test]
    fn test_load_store() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // SD x1, 0(x2) -> Store x1 at x2+0
        // x1 = 0xDEADBEEF, x2 = 0x8000_0100
        cpu.write_reg(Register::X1, 0xDEADBEEF);
        cpu.write_reg(Register::X2, 0x8000_0100);

        // SD: Op=0x23, funct3=3, rs1=2, rs2=1, imm=0
        // Using manual encoding here: imm=0 so only rs2/rs1/funct3/opcode matter.
        let sd_insn = (1 << 20) | (2 << 15) | (3 << 12) | 0x23;
        bus.write32(0x8000_0000, sd_insn).unwrap();

        cpu.step(&mut bus).unwrap();
        assert_eq!(bus.read64(0x8000_0100).unwrap(), 0xDEADBEEF);

        // LD x3, 0(x2) -> Load x3 from x2+0
        // LD: Op=0x03, funct3=3, rd=3, rs1=2, imm=0
        let ld_insn = (2 << 15) | (3 << 12) | (3 << 7) | 0x03;
        bus.write32(0x8000_0004, ld_insn).unwrap();

        cpu.step(&mut bus).unwrap(); // Execute SD (pc was incremented in previous step? No wait)
                                     // Previous step PC went 0->4. Now at 4.

        assert_eq!(cpu.read_reg(Register::X3), 0xDEADBEEF);
    }

    #[test]
    fn test_x0_invariant() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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

        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();

        // x0 must remain hard-wired to zero
        assert_eq!(cpu.read_reg(Register::X0), 0);
    }

    #[test]
    fn test_branch_taken_and_not_taken() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // BEQ x1, x2, +8 (pc + 8 when taken)
        let beq_insn = encode_b(8, 2, 1, 0x0, 0x63);
        bus.write32(0x8000_0000, beq_insn).unwrap();

        // Taken: x1 == x2
        cpu.write_reg(Register::X1, 5);
        cpu.write_reg(Register::X2, 5);
        cpu.pc = 0x8000_0000;
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0008);

        // Not taken: x1 != x2
        cpu.write_reg(Register::X1, 1);
        cpu.write_reg(Register::X2, 2);
        cpu.pc = 0x8000_0000;
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0004);
    }

    #[test]
    fn test_w_ops_sign_extension() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // Set x1 = 0x0000_0000_8000_0000 (low 32 bits have sign bit set)
        cpu.write_reg(Register::X1, 0x0000_0000_8000_0000);
        cpu.write_reg(Register::X2, 0); // x2 = 0

        // ADDW x3, x1, x2  (opcode=0x3B, funct3=0, funct7=0)
        let addw = encode_r(0x00, 2, 1, 0x0, 3, 0x3B);
        bus.write32(0x8000_0000, addw).unwrap();

        cpu.step(&mut bus).unwrap();

        // Expect sign-extended 32-bit result: 0xFFFF_FFFF_8000_0000
        assert_eq!(cpu.read_reg(Register::X3), 0xFFFF_FFFF_8000_0000);
    }

    #[test]
    fn test_m_extension_mul_div_rem() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // MUL: 3 * 4 = 12
        cpu.write_reg(Register::X1, 3);
        cpu.write_reg(Register::X2, 4);
        let mul = encode_r(0x01, 2, 1, 0x0, 3, 0x33); // MUL x3, x1, x2
        bus.write32(0x8000_0000, mul).unwrap();
        cpu.step(&mut bus).unwrap();
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
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();

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
            cpu.step(&mut bus).unwrap();
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
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();

        assert_eq!(cpu.read_reg(Register::X11), i64::MIN as u64);
        assert_eq!(cpu.read_reg(Register::X12), 0);
    }

    #[test]
    fn test_compressed_addi_and_lwsp_paths() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0002);
        assert_eq!(cpu.read_reg(Register::X11), 11);

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0004);
        assert_eq!(cpu.read_reg(Register::X2), 0x8000_0110); // sp + 16

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x8000_0006);
        assert_eq!(cpu.read_reg(Register::X15), 0xFFFF_FFFF_DEAD_BEEF); // a5 (sign-extended lw)
    }

    #[test]
    fn test_zicsr_basic_csrs() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);
        let csr_addr: u32 = 0x300; // mstatus

        // CSRRWI x1, mstatus, 5  (mstatus = 5, x1 = old = 0)
        let csrrwi = {
            let zimm = 5u32;
            (csr_addr << 20) | (zimm << 15) | (0x5 << 12) | (1 << 7) | 0x73
        };
        bus.write32(0x8000_0000, csrrwi).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0);

        // CSRRSI x2, mstatus, 0xA  (mstatus = 5 | 0xA = 0xF, x2 = old = 5)
        let csrrsi = {
            let zimm = 0xAu32;
            (csr_addr << 20) | (zimm << 15) | (0x6 << 12) | (2 << 7) | 0x73
        };
        bus.write32(0x8000_0004, csrrsi).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X2), 5);

        // CSRRCI x3, mstatus, 0x3  (mstatus = 0xF & !0x3 = 0xC, x3 = old = 0xF)
        let csrrci = {
            let zimm = 0x3u32;
            (csr_addr << 20) | (zimm << 15) | (0x7 << 12) | (3 << 7) | 0x73
        };
        bus.write32(0x8000_0008, csrrci).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X3), 0xF);
    }

    #[test]
    fn test_a_extension_lr_sc_basic() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X3), 0xDEAD_BEEF_DEAD_BEEF);

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X4), 0); // SC success
        assert_eq!(bus.read64(addr).unwrap(), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn test_a_extension_reservation_and_misaligned_sc() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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

        cpu.step(&mut bus).unwrap(); // LR
        cpu.step(&mut bus).unwrap(); // AMOADD
        cpu.step(&mut bus).unwrap(); // SC (should fail)

        assert_eq!(cpu.read_reg(Register::X5), 1);

        // Misaligned SC.D must trap with StoreAddressMisaligned
        cpu.pc = 0x8000_0010;
        cpu.write_reg(Register::X1, addr + 1); // misaligned
        let sc_misaligned = encode_amo(0b00011, false, false, 2, 1, 0x3, 6);
        bus.write32(0x8000_0010, sc_misaligned).unwrap();

        let res = cpu.step(&mut bus);
        match res {
            Err(Trap::StoreAddressMisaligned(a)) => assert_eq!(a, addr + 1),
            _ => panic!("Expected StoreAddressMisaligned trap"),
        }
    }

    #[test]
    fn test_load_sign_and_zero_extension() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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
            cpu.step(&mut bus).unwrap();
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
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // x2 = misaligned address
        cpu.write_reg(Register::X2, 0x8000_0001);

        // LW x1, 0(x2)  -> should trap with LoadAddressMisaligned
        let lw = encode_i(0, 2, 2, 1, 0x03);
        bus.write32(0x8000_0000, lw).unwrap();

        let res = cpu.step(&mut bus);
        match res {
            Err(Trap::LoadAddressMisaligned(a)) => assert_eq!(a, 0x8000_0001),
            _ => panic!("Expected LoadAddressMisaligned trap"),
        }

        // SW x1, 0(x2)  -> should trap with StoreAddressMisaligned
        cpu.pc = 0x8000_0000;
        let sw = encode_s(0, 1, 2, 2, 0x23);
        bus.write32(0x8000_0000, sw).unwrap();

        let res = cpu.step(&mut bus);
        match res {
            Err(Trap::StoreAddressMisaligned(a)) => assert_eq!(a, 0x8000_0001),
            _ => panic!("Expected StoreAddressMisaligned trap"),
        }
    }

    #[test]
    fn test_access_fault_outside_dram() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

        // LW x1, 0(x0) -> effective address 0x0 (outside DRAM, but aligned)
        let lw = encode_i(0, 0, 2, 1, 0x03);
        bus.write32(0x8000_0000, lw).unwrap();

        let res = cpu.step(&mut bus);
        match res {
            Err(Trap::LoadAccessFault(a)) => assert_eq!(a, 0),
            _ => panic!("Expected LoadAccessFault trap"),
        }
    }

    #[test]
    fn test_jal() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.read_reg(Register::X1), 0x8000_0004); // Link address
        assert_eq!(cpu.pc, 0x8000_0008); // Target
    }

    #[test]
    fn test_misaligned_fetch() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0001); // Odd PC

        let res = cpu.step(&mut bus);
        match res {
            Err(Trap::InstructionAddressMisaligned(addr)) => assert_eq!(addr, 0x8000_0001),
            _ => panic!("Expected misaligned trap"),
        }
    }

    #[test]
    fn test_smoke_sum() {
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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
            match cpu.step(&mut bus) {
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
        let mut bus = make_bus();
        let mut cpu = Cpu::new(0x8000_0000);

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
        bus.clint.mtimecmp[0] = 100;
        // Set mtime to 101 (trigger condition)
        bus.clint.set_mtime(101);

        // We need a valid instruction at PC to attempt fetch, although interrupt checks before fetch.
        bus.write32(0x8000_0000, 0x00000013).unwrap(); // NOP (addi x0, x0, 0)

        let res = cpu.step(&mut bus);
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
        bus.clint.mtimecmp[0] = 200; 
        // CPU is now at handler. We need to "return" (mret) or just reset state for next test.
        // Reset PC back to start
        cpu.pc = 0x8000_0000;
        // Re-enable MIE (trap disabled it)
        let mut mstatus = cpu.read_csr(CSR_MSTATUS).unwrap();
        mstatus |= 1 << 3;
        cpu.write_csr(CSR_MSTATUS, mstatus).unwrap();

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
        let res = cpu.step(&mut bus);
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
