use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("Address out of bounds: {0:#x}")]
    OutOfBounds(u64),
    #[error("Invalid alignment for address: {0:#x}")]
    InvalidAlignment(u64),
}

pub struct Dram {
    data: Vec<u8>,
}

impl Dram {
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
        }
    }

    pub fn load(&self, addr: u64, size: u64) -> Result<u64, MemoryError> {
        let addr = addr as usize;
        let size = size as usize;

        if addr + size > self.data.len() {
            return Err(MemoryError::OutOfBounds(addr as u64));
        }

        let mut value = 0u64;
        for i in 0..size {
            value |= (self.data[addr + i] as u64) << (i * 8);
        }

        Ok(value)
    }

    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        let addr = addr as usize;
        let size = size as usize;

        if addr + size > self.data.len() {
            return Err(MemoryError::OutOfBounds(addr as u64));
        }

        for i in 0..size {
            self.data[addr + i] = ((value >> (i * 8)) & 0xFF) as u8;
        }

        Ok(())
    }

    pub fn load_8(&self, addr: u64) -> Result<u64, MemoryError> {
        self.load(addr, 1)
    }

    pub fn load_16(&self, addr: u64) -> Result<u64, MemoryError> {
        self.load(addr, 2)
    }

    pub fn load_32(&self, addr: u64) -> Result<u64, MemoryError> {
        self.load(addr, 4)
    }

    pub fn load_64(&self, addr: u64) -> Result<u64, MemoryError> {
        self.load(addr, 8)
    }

    pub fn store_8(&mut self, addr: u64, value: u64) -> Result<(), MemoryError> {
        self.store(addr, 1, value)
    }

    pub fn store_16(&mut self, addr: u64, value: u64) -> Result<(), MemoryError> {
        self.store(addr, 2, value)
    }

    pub fn store_32(&mut self, addr: u64, value: u64) -> Result<(), MemoryError> {
        self.store(addr, 4, value)
    }

    pub fn store_64(&mut self, addr: u64, value: u64) -> Result<(), MemoryError> {
        self.store(addr, 8, value)
    }

    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> Result<(), MemoryError> {
        let addr = addr as usize;
        let end = addr
            .checked_add(data.len())
            .ok_or(MemoryError::OutOfBounds(addr as u64))?;

        if end > self.data.len() {
            return Err(MemoryError::OutOfBounds(addr as u64));
        }

        self.data[addr..end].copy_from_slice(data);
        Ok(())
    }

    pub fn fill(&mut self, addr: u64, len: usize, value: u8) -> Result<(), MemoryError> {
        if len == 0 {
            return Ok(());
        }

        let addr = addr as usize;
        let end = addr
            .checked_add(len)
            .ok_or(MemoryError::OutOfBounds(addr as u64))?;

        if end > self.data.len() {
            return Err(MemoryError::OutOfBounds(addr as u64));
        }

        self.data[addr..end].fill(value);
        Ok(())
    }
}
