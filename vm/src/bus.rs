use crate::dram::{Dram, MemoryError};
use crate::uart::Uart;

pub const DRAM_BASE: u64 = 0x8000_0000;
// Use a smaller DRAM size for Wasm to avoid excessive linear memory allocation.
#[cfg(target_arch = "wasm32")]
pub const DRAM_SIZE: u64 = 512 * 1024 * 1024; // 512MB for browser environments
#[cfg(not(target_arch = "wasm32"))]
pub const DRAM_SIZE: u64 = 512 * 1024 * 1024; // 512MB for native builds
pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100; // Arbitrary small size for now
pub const STATS_MMIO_BASE: u64 = 0x2000_0000;

pub struct Bus {
    pub dram: Dram,
    pub uart: Uart,
    pub used_memory: u64,
}

impl Bus {
    pub fn new() -> Self {
        Self {
            dram: Dram::new(DRAM_SIZE as usize),
            uart: Uart::new(),
            used_memory: 0,
        }
    }

    pub fn load(&mut self, addr: u64, size: u64) -> Result<u64, MemoryError> {
        if addr >= DRAM_BASE && addr + size <= DRAM_BASE + DRAM_SIZE {
            let offset = addr - DRAM_BASE;
            return self.dram.load(offset, size);
        }
        if addr >= UART_BASE && addr + size <= UART_BASE + UART_SIZE {
            return self.uart.load(addr - UART_BASE, size);
        }
        if addr == STATS_MMIO_BASE {
            // Optional: allow reads back of the last reported value
            return Ok(self.used_memory);
        }
        Err(MemoryError::OutOfBounds(addr))
    }

    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        if addr == STATS_MMIO_BASE {
            // Accept 4 or 8 byte writes and capture the reported usage
            self.used_memory = if size >= 8 {
                value
            } else {
                value & 0xffff_ffff
            };
            return Ok(());
        }
        if addr >= DRAM_BASE && addr + size <= DRAM_BASE + DRAM_SIZE {
            let offset = addr - DRAM_BASE;
            return self.dram.store(offset, size, value);
        }
        if addr >= UART_BASE && addr + size <= UART_BASE + UART_SIZE {
            return self.uart.store(addr - UART_BASE, size, value);
        }
        Err(MemoryError::OutOfBounds(addr))
    }

    // Initialize memory from a byte slice (used for loading binaries)
    pub fn initialize_dram(&mut self, data: &[u8]) -> Result<(), MemoryError> {
        self.write_bytes(DRAM_BASE, data)
    }

    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> Result<(), MemoryError> {
        if data.is_empty() {
            return Ok(());
        }

        let end = addr
            .checked_add(data.len() as u64)
            .ok_or(MemoryError::OutOfBounds(addr))?;

        if addr >= DRAM_BASE && end <= DRAM_BASE + DRAM_SIZE {
            self.dram.write_bytes(addr - DRAM_BASE, data)
        } else {
            Err(MemoryError::OutOfBounds(addr))
        }
    }

    pub fn fill_bytes(&mut self, addr: u64, len: usize, value: u8) -> Result<(), MemoryError> {
        if len == 0 {
            return Ok(());
        }

        let end = addr
            .checked_add(len as u64)
            .ok_or(MemoryError::OutOfBounds(addr))?;

        if addr >= DRAM_BASE && end <= DRAM_BASE + DRAM_SIZE {
            self.dram.fill(addr - DRAM_BASE, len, value)
        } else {
            Err(MemoryError::OutOfBounds(addr))
        }
    }
}
