use crate::dram::MemoryError;
use std::collections::VecDeque;

pub struct Uart {
    pub input: VecDeque<u8>,
    pub output: VecDeque<u8>,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            input: VecDeque::new(),
            output: VecDeque::new(),
        }
    }

    pub fn load(&mut self, offset: u64, size: u64) -> Result<u64, MemoryError> {
        if size != 1 {
            return Err(MemoryError::InvalidAlignment(offset));
        }

        // If we have input, return it. Otherwise 0.
        // (Real 16550 UART has status registers, but we simplify)
        if let Some(byte) = self.input.pop_front() {
            Ok(byte as u64)
        } else {
            Ok(0)
        }
    }

    pub fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        if size != 1 {
            return Err(MemoryError::InvalidAlignment(offset));
        }

        let byte = (value & 0xff) as u8;
        self.output.push_back(byte);
        Ok(())
    }

    // Interface for the Host (Wasm)
    pub fn push_input(&mut self, byte: u8) {
        self.input.push_back(byte);
    }

    pub fn pop_output(&mut self) -> Option<u8> {
        self.output.pop_front()
    }
}
