use thiserror::Error;

/// Base physical address of DRAM as seen by devices that work directly with
/// physical addresses (VirtIO, etc.).
///
/// This matches the DRAM base used by the `SystemBus` in `bus.rs` and the
/// Phase-0 virt memory map.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// Device-local memory access errors.
///
/// These are mapped into architectural traps (`Trap`) by higher layers
/// (e.g., the system bus) where appropriate.
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("Out-of-bounds memory access at {0:#x}")]
    OutOfBounds(u64),

    #[error("Invalid or misaligned access at {0:#x}")]
    InvalidAlignment(u64),
}

/// Simple byte-addressable DRAM backing store used by VirtIO-style devices.
///
/// Offsets passed to the load/store helpers are **physical offsets from
/// `DRAM_BASE`**, not full guest physical addresses. Callers typically use
/// `DRAM_BASE` and subtract it via a helper (see `virtio.rs`).
pub struct Dram {
    pub base: u64,
    pub data: Vec<u8>,
}

impl Dram {
    /// Create a new DRAM image of `size` bytes, zero-initialised.
    pub fn new(base: u64, size: usize) -> Self {
        Self { base, data: vec![0; size] }
    }

    #[inline(always)]
    pub fn offset(&self, addr: u64) -> Option<usize> {
        // Use wrapping_sub to avoid branch on underflow check
        let off = addr.wrapping_sub(self.base) as usize;
        if off < self.data.len() {
            Some(off)
        } else {
            None
        }
    }

    pub fn load(&mut self, data: &[u8], offset: u64) -> Result<(), MemoryError> {
        self.write_bytes(offset, data)
    }

    pub fn zero_range(&mut self, offset: usize, len: usize) -> Result<(), MemoryError> {
        if offset + len > self.data.len() {
            return Err(MemoryError::OutOfBounds(offset as u64));
        }
        for i in 0..len {
            self.data[offset + i] = 0;
        }
        Ok(())
    }

    #[inline(always)]
    fn check_bounds(&self, offset: u64, size: usize) -> Result<usize, MemoryError> {
        let off = offset as usize;
        let end = off.checked_add(size).ok_or(MemoryError::OutOfBounds(offset))?;
        if end > self.data.len() {
            return Err(MemoryError::OutOfBounds(offset));
        }
        Ok(off)
    }

    #[inline(always)]
    pub fn load_8(&self, offset: u64) -> Result<u8, MemoryError> {
        let off = self.check_bounds(offset, 1)?;
        Ok(self.data[off])
    }

    #[inline(always)]
    pub fn load_16(&self, offset: u64) -> Result<u16, MemoryError> {
        if offset % 2 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 2)?;
        let bytes: [u8; 2] = self.data[off..off + 2].try_into().unwrap();
        Ok(u16::from_le_bytes(bytes))
    }

    #[inline(always)]
    pub fn load_32(&self, offset: u64) -> Result<u32, MemoryError> {
        if offset % 4 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 4)?;
        let bytes: [u8; 4] = self.data[off..off + 4].try_into().unwrap();
        Ok(u32::from_le_bytes(bytes))
    }

    #[inline(always)]
    pub fn load_64(&self, offset: u64) -> Result<u64, MemoryError> {
        if offset % 8 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 8)?;
        let bytes: [u8; 8] = self.data[off..off + 8].try_into().unwrap();
        Ok(u64::from_le_bytes(bytes))
    }

    #[inline(always)]
    pub fn store_8(&mut self, offset: u64, value: u64) -> Result<(), MemoryError> {
        let off = self.check_bounds(offset, 1)?;
        self.data[off] = (value & 0xff) as u8;
        Ok(())
    }

    #[inline(always)]
    pub fn store_16(&mut self, offset: u64, value: u64) -> Result<(), MemoryError> {
        if offset % 2 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 2)?;
        let bytes = (value as u16).to_le_bytes();
        self.data[off..off + 2].copy_from_slice(&bytes);
        Ok(())
    }

    #[inline(always)]
    pub fn store_32(&mut self, offset: u64, value: u64) -> Result<(), MemoryError> {
        if offset % 4 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 4)?;
        let bytes = (value as u32).to_le_bytes();
        self.data[off..off + 4].copy_from_slice(&bytes);
        Ok(())
    }

    #[inline(always)]
    pub fn store_64(&mut self, offset: u64, value: u64) -> Result<(), MemoryError> {
        if offset % 8 != 0 {
            return Err(MemoryError::InvalidAlignment(offset));
        }
        let off = self.check_bounds(offset, 8)?;
        let bytes = value.to_le_bytes();
        self.data[off..off + 8].copy_from_slice(&bytes);
        Ok(())
    }

    /// Write an arbitrary slice into DRAM starting at `offset`.
    pub fn write_bytes(&mut self, offset: u64, data: &[u8]) -> Result<(), MemoryError> {
        let off = self.check_bounds(offset, data.len())?;
        self.data[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }
}
