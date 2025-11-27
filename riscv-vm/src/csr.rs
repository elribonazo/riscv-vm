use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    User,
    Supervisor,
    Machine,
}

impl Mode {
    /// Encode privilege mode into the MPP/SPP field encoding.
    pub fn to_mpp(self) -> u64 {
        match self {
            Mode::User => 0b00,
            Mode::Supervisor => 0b01,
            Mode::Machine => 0b11,
        }
    }

    /// Decode MPP/SPP field into a privilege mode.
    pub fn from_mpp(bits: u64) -> Mode {
        match bits & 0b11 {
            0b00 => Mode::User,
            0b01 => Mode::Supervisor,
            // 0b10 is reserved; treat as Machine for WARL coercion.
            _ => Mode::Machine,
        }
    }
}

// Common CSR addresses used by the privileged architecture.
pub const CSR_SATP: u16 = 0x180;

pub const CSR_MSTATUS: u16 = 0x300;
pub const CSR_MISA: u16 = 0x301;
pub const CSR_MEDELEG: u16 = 0x302;
pub const CSR_MIDELEG: u16 = 0x303;
pub const CSR_MIE: u16 = 0x304;
pub const CSR_MTVEC: u16 = 0x305;

pub const CSR_MEPC: u16 = 0x341;
pub const CSR_MCAUSE: u16 = 0x342;
pub const CSR_MTVAL: u16 = 0x343;
pub const CSR_MIP: u16 = 0x344;

// Supervisor CSRs
pub const CSR_SSTATUS: u16 = 0x100;
pub const CSR_SIE: u16 = 0x104;
pub const CSR_STVEC: u16 = 0x105;
pub const CSR_SSCRATCH: u16 = 0x140;
pub const CSR_SEPC: u16 = 0x141;
pub const CSR_SCAUSE: u16 = 0x142;
pub const CSR_STVAL: u16 = 0x143;
pub const CSR_SIP: u16 = 0x144;

// Additional CSRs used by xv6 and Sstc
pub const CSR_TIME: u16 = 0xC01;      // time (read-only)
pub const CSR_MENVCFG: u16 = 0x30A;   // menvcfg (for Sstc enable bit 63)
pub const CSR_STIMECMP: u16 = 0x14D;  // stimecmp (Sstc)
pub const CSR_MCOUNTEREN: u16 = 0x306;

// Machine Information Registers (read-only)
pub const CSR_MVENDORID: u16 = 0xF11;  // Vendor ID
pub const CSR_MARCHID: u16 = 0xF12;    // Architecture ID
pub const CSR_MIMPID: u16 = 0xF13;     // Implementation ID
pub const CSR_MHARTID: u16 = 0xF14;    // Hardware thread ID

