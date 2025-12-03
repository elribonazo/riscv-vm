//! JIT Helper Functions
//!
//! These functions are called by JIT'd code to perform operations that
//! can't be easily inlined (MMU translation, CSR access).
//!
//! The helper functions perform full MMU translation and return results
//! that indicate success/failure so the JIT'd code can handle traps.

use crate::bus::Bus;
use crate::cpu::types::{Mode, Trap};
use crate::mmu::{self, AccessType, Tlb};

/// Read a u64 with MMU translation.
///
/// # Returns
/// - On success: `(value, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_read_u64(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
) -> (u64, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Load) {
        Ok(pa) => match bus.read64(pa) {
            Ok(val) => (val, true),
            Err(_) => (5, false), // LoadAccessFault
        },
        Err(Trap::LoadPageFault(_)) => (13, false),
        Err(Trap::LoadAccessFault(_)) => (5, false),
        Err(_) => (5, false),
    }
}

/// Read a u32 with MMU translation.
///
/// # Returns
/// - On success: `(value, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_read_u32(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
) -> (u32, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Load) {
        Ok(pa) => match bus.read32(pa) {
            Ok(val) => (val, true),
            Err(_) => (5, false),
        },
        Err(Trap::LoadPageFault(_)) => (13, false),
        Err(_) => (5, false),
    }
}

/// Read a u16 with MMU translation.
///
/// # Returns
/// - On success: `(value, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_read_u16(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
) -> (u16, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Load) {
        Ok(pa) => match bus.read16(pa) {
            Ok(val) => (val, true),
            Err(_) => (5, false),
        },
        Err(Trap::LoadPageFault(_)) => (13, false),
        Err(_) => (5, false),
    }
}

/// Read a u8 with MMU translation.
///
/// # Returns
/// - On success: `(value, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_read_u8(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
) -> (u8, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Load) {
        Ok(pa) => match bus.read8(pa) {
            Ok(val) => (val, true),
            Err(_) => (5, false),
        },
        Err(Trap::LoadPageFault(_)) => (13, false),
        Err(_) => (5, false),
    }
}

/// Write a u64 with MMU translation.
///
/// # Returns
/// - On success: `(0, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_write_u64(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
    value: u64,
) -> (u32, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Store) {
        Ok(pa) => match bus.write64(pa, value) {
            Ok(()) => (0, true),
            Err(_) => (7, false), // StoreAccessFault
        },
        Err(Trap::StorePageFault(_)) => (15, false),
        Err(Trap::StoreAccessFault(_)) => (7, false),
        Err(_) => (7, false),
    }
}

/// Write a u32 with MMU translation.
///
/// # Returns
/// - On success: `(0, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_write_u32(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
    value: u32,
) -> (u32, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Store) {
        Ok(pa) => match bus.write32(pa, value) {
            Ok(()) => (0, true),
            Err(_) => (7, false),
        },
        Err(Trap::StorePageFault(_)) => (15, false),
        Err(_) => (7, false),
    }
}

/// Write a u16 with MMU translation.
///
/// # Returns
/// - On success: `(0, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_write_u16(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
    value: u16,
) -> (u32, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Store) {
        Ok(pa) => match bus.write16(pa, value) {
            Ok(()) => (0, true),
            Err(_) => (7, false),
        },
        Err(Trap::StorePageFault(_)) => (15, false),
        Err(_) => (7, false),
    }
}

/// Write a u8 with MMU translation.
///
/// # Returns
/// - On success: `(0, true)`
/// - On fault: `(trap_code, false)`
pub fn mmu_write_u8(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    vaddr: u64,
    value: u8,
) -> (u32, bool) {
    match mmu::translate(bus, tlb, mode, satp, mstatus, vaddr, AccessType::Store) {
        Ok(pa) => match bus.write8(pa, value) {
            Ok(()) => (0, true),
            Err(_) => (7, false),
        },
        Err(Trap::StorePageFault(_)) => (15, false),
        Err(_) => (7, false),
    }
}

/// WASM-exported helper function type indices.
/// These are used when building the import section.
pub mod import_indices {
    pub const READ_U64: u32 = 0;
    pub const READ_U32: u32 = 1;
    pub const READ_U16: u32 = 2;
    pub const READ_U8: u32 = 3;
    pub const WRITE_U64: u32 = 4;
    pub const WRITE_U32: u32 = 5;
    pub const WRITE_U16: u32 = 6;
    pub const WRITE_U8: u32 = 7;
}

/// WASM function type signatures for helper imports.
pub mod type_signatures {
    use wasm_encoder::ValType;

    /// read_u64(vaddr: i64) -> i64
    pub const READ_U64_PARAMS: &[ValType] = &[ValType::I64];
    pub const READ_U64_RESULTS: &[ValType] = &[ValType::I64];

    /// read_u32(vaddr: i64) -> i32
    pub const READ_U32_PARAMS: &[ValType] = &[ValType::I64];
    pub const READ_U32_RESULTS: &[ValType] = &[ValType::I32];

    /// read_u16(vaddr: i64) -> i32 (zero-extended)
    pub const READ_U16_PARAMS: &[ValType] = &[ValType::I64];
    pub const READ_U16_RESULTS: &[ValType] = &[ValType::I32];

    /// read_u8(vaddr: i64) -> i32 (zero-extended)
    pub const READ_U8_PARAMS: &[ValType] = &[ValType::I64];
    pub const READ_U8_RESULTS: &[ValType] = &[ValType::I32];

    /// write_u64(vaddr: i64, value: i64) -> i32 (0 = success, else trap code)
    pub const WRITE_U64_PARAMS: &[ValType] = &[ValType::I64, ValType::I64];
    pub const WRITE_U64_RESULTS: &[ValType] = &[ValType::I32];

    /// write_u32(vaddr: i64, value: i32) -> i32
    pub const WRITE_U32_PARAMS: &[ValType] = &[ValType::I64, ValType::I32];
    pub const WRITE_U32_RESULTS: &[ValType] = &[ValType::I32];

    /// write_u16(vaddr: i64, value: i32) -> i32
    pub const WRITE_U16_PARAMS: &[ValType] = &[ValType::I64, ValType::I32];
    pub const WRITE_U16_RESULTS: &[ValType] = &[ValType::I32];

    /// write_u8(vaddr: i64, value: i32) -> i32
    pub const WRITE_U8_PARAMS: &[ValType] = &[ValType::I64, ValType::I32];
    pub const WRITE_U8_RESULTS: &[ValType] = &[ValType::I32];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SystemBus;

    fn make_bus() -> SystemBus {
        SystemBus::new(0x8000_0000, 1024 * 1024) // 1MB
    }

    #[test]
    fn test_mmu_read_success_machine_mode() {
        let bus = make_bus();
        let mut tlb = Tlb::new();

        // Write test data
        bus.write64(0x8000_0100, 0xDEADBEEF_CAFEBABE).unwrap();

        // In Machine mode, no translation occurs (satp=0 means Bare mode)
        let (val, ok) = mmu_read_u64(&bus, &mut tlb, Mode::Machine, 0, 0, 0x8000_0100);
        assert!(ok);
        assert_eq!(val, 0xDEADBEEF_CAFEBABE);
    }

    #[test]
    fn test_mmu_read_u32_success() {
        let bus = make_bus();
        let mut tlb = Tlb::new();

        bus.write32(0x8000_0200, 0x12345678).unwrap();

        let (val, ok) = mmu_read_u32(&bus, &mut tlb, Mode::Machine, 0, 0, 0x8000_0200);
        assert!(ok);
        assert_eq!(val, 0x12345678);
    }

    #[test]
    fn test_mmu_write_success() {
        let bus = make_bus();
        let mut tlb = Tlb::new();

        let (code, ok) = mmu_write_u64(&bus, &mut tlb, Mode::Machine, 0, 0, 0x8000_0300, 0xABCD);
        assert!(ok);
        assert_eq!(code, 0);

        // Verify the write
        let val = bus.read64(0x8000_0300).unwrap();
        assert_eq!(val, 0xABCD);
    }

    #[test]
    fn test_mmu_read_access_fault() {
        let bus = make_bus();
        let mut tlb = Tlb::new();

        // Address 0x0 is outside DRAM, should cause LoadAccessFault
        let (code, ok) = mmu_read_u64(&bus, &mut tlb, Mode::Machine, 0, 0, 0x0);
        assert!(!ok);
        assert_eq!(code, 5); // LoadAccessFault
    }

    #[test]
    fn test_mmu_write_access_fault() {
        let bus = make_bus();
        let mut tlb = Tlb::new();

        // Address 0x0 is outside DRAM, should cause StoreAccessFault
        let (code, ok) = mmu_write_u64(&bus, &mut tlb, Mode::Machine, 0, 0, 0x0, 42);
        assert!(!ok);
        assert_eq!(code, 7); // StoreAccessFault
    }

    #[test]
    fn test_mmu_read_all_sizes() {
        let bus = make_bus();
        let mut tlb = Tlb::new();
        let base = 0x8000_0400;

        // Write a known pattern
        bus.write64(base, 0x8877_6655_4433_2211).unwrap();

        // Read as u8
        let (val, ok) = mmu_read_u8(&bus, &mut tlb, Mode::Machine, 0, 0, base);
        assert!(ok);
        assert_eq!(val, 0x11);

        // Read as u16
        let (val, ok) = mmu_read_u16(&bus, &mut tlb, Mode::Machine, 0, 0, base);
        assert!(ok);
        assert_eq!(val, 0x2211);

        // Read as u32
        let (val, ok) = mmu_read_u32(&bus, &mut tlb, Mode::Machine, 0, 0, base);
        assert!(ok);
        assert_eq!(val, 0x4433_2211);

        // Read as u64
        let (val, ok) = mmu_read_u64(&bus, &mut tlb, Mode::Machine, 0, 0, base);
        assert!(ok);
        assert_eq!(val, 0x8877_6655_4433_2211);
    }

    #[test]
    fn test_mmu_write_all_sizes() {
        let bus = make_bus();
        let mut tlb = Tlb::new();
        let base = 0x8000_0500;

        // Clear the memory
        bus.write64(base, 0).unwrap();

        // Write u8
        let (code, ok) = mmu_write_u8(&bus, &mut tlb, Mode::Machine, 0, 0, base, 0xAA);
        assert!(ok);
        assert_eq!(code, 0);

        // Write u16 at offset 2
        let (code, ok) = mmu_write_u16(&bus, &mut tlb, Mode::Machine, 0, 0, base + 2, 0xBBCC);
        assert!(ok);
        assert_eq!(code, 0);

        // Write u32 at offset 4
        let (code, ok) = mmu_write_u32(&bus, &mut tlb, Mode::Machine, 0, 0, base + 4, 0xDDEE_FF00);
        assert!(ok);
        assert_eq!(code, 0);

        // Verify final value
        let val = bus.read64(base).unwrap();
        assert_eq!(val, 0xDDEE_FF00_BBCC_00AA);
    }
}

