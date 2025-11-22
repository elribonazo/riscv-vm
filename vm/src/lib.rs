pub mod bus;
pub mod cpu;
pub mod dram;
pub mod loader;
pub mod uart;

use crate::bus::Bus;
use crate::bus::DRAM_SIZE;
use crate::cpu::Cpu;
use crate::loader::load_image;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WasmVm {
    cpu: Cpu,
}

#[wasm_bindgen]
impl WasmVm {
    #[wasm_bindgen(constructor)]
    pub fn new(binary: &[u8]) -> Result<WasmVm, String> {
        console_error_panic_hook::set_once();
        let mut bus = Bus::new();
        load_image(&mut bus, binary).map_err(|e| e.to_string())?;
        let cpu = Cpu::new(bus);
        Ok(WasmVm { cpu })
    }

    pub fn step(&mut self) -> Result<(), String> {
        self.cpu.step()
    }

    pub fn run(&mut self, cycles: usize) -> Result<usize, String> {
        for i in 0..cycles {
            match self.cpu.step() {
                Ok(_) => {}
                Err(e) => return Err(format!("CPU error at step {}: {}", i, e)),
            }
            // Optional: Break on WFI or similar if we had interrupts implemented
        }
        Ok(cycles)
    }

    pub fn input(&mut self, byte: u8) {
        self.cpu.bus.uart.push_input(byte);
    }

    pub fn get_output(&mut self) -> Option<u8> {
        self.cpu.bus.uart.pop_output()
    }

    pub fn get_pc(&self) -> u64 {
        self.cpu.pc
    }

    pub fn get_reg(&self, reg: usize) -> u64 {
        if reg < 32 { self.cpu.regs[reg] } else { 0 }
    }

    pub fn get_memory_usage(&self) -> u64 {
        self.cpu.bus.used_memory
    }

    pub fn get_total_memory(&self) -> u64 {
        DRAM_SIZE
    }

    pub fn get_cpu_cycles(&self) -> u64 {
        self.cpu.csrs[0xC00]
    }
}

#[cfg(test)]
mod tests {
    use crate::bus::Bus;

    #[test]
    fn test_bus_load_store() {
        let mut bus = Bus::new();

        // Test loading a dummy binary
        let dummy_binary = vec![0xAA, 0xBB, 0xCC, 0xDD];
        bus.initialize_dram(&dummy_binary)
            .expect("Failed to initialize DRAM");

        // Read back from 0x8000_0000 (DRAM base)
        let val = bus.load(0x8000_0000, 1).expect("Failed to load byte");
        assert_eq!(val, 0xAA);

        let val = bus.load(0x8000_0001, 1).expect("Failed to load byte");
        assert_eq!(val, 0xBB);

        // Test 32-bit load
        let val = bus.load(0x8000_0000, 4).expect("Failed to load word");
        // Little endian: 0xDDCCBBAA
        assert_eq!(val, 0xDDCCBBAA);
    }
}
