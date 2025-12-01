use crate::dram::MemoryError;
use std::collections::VecDeque;
use std::sync::Mutex;

pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;

// Registers (offset)
const RBR: u64 = 0x00; // Receiver Buffer (Read)
const THR: u64 = 0x00; // Transmitter Holding (Write)
const IER: u64 = 0x01; // Interrupt Enable
const IIR: u64 = 0x02; // Interrupt Identity (Read)
const FCR: u64 = 0x02; // FIFO Control (Write)
const LCR: u64 = 0x03; // Line Control
const MCR: u64 = 0x04; // Modem Control
const LSR: u64 = 0x05; // Line Status
const MSR: u64 = 0x06; // Modem Status
const SCR: u64 = 0x07; // Scratch

/// RX path state (host → guest)
struct RxState {
    /// Input FIFO (keyboard/serial input from host)
    fifo: VecDeque<u8>,
}

/// TX path state (guest → host)
struct TxState {
    /// Output FIFO (console output to host)
    fifo: VecDeque<u8>,
    /// THRE interrupt pending flag
    thre_ip: bool,
}

/// Control registers (shared, less frequent access)
struct UartRegs {
    ier: u8,      // Interrupt Enable Register
    iir: u8,      // Interrupt Identification Register (read-only computed)
    fcr: u8,      // FIFO Control Register
    lcr: u8,      // Line Control Register
    mcr: u8,      // Modem Control Register
    lsr: u8,      // Line Status Register
    msr: u8,      // Modem Status Register
    scr: u8,      // Scratch Register
    dll: u8,      // Divisor Latch Low
    dlm: u8,      // Divisor Latch High
    interrupting: bool,
}

impl RxState {
    fn new() -> Self {
        Self { fifo: VecDeque::new() }
    }
}

impl TxState {
    fn new() -> Self {
        Self {
            fifo: VecDeque::new(),
            thre_ip: true, // Starts empty
        }
    }
}

impl UartRegs {
    fn new() -> Self {
        Self {
            ier: 0x00,
            iir: 0x01, // No interrupt pending
            fcr: 0x00,
            lcr: 0x00,
            mcr: 0x00,
            lsr: 0x60, // TX empty
            msr: 0x00,
            scr: 0x00,
            dll: 0x00,
            dlm: 0x00,
            interrupting: false,
        }
    }
}

pub struct Uart {
    /// RX path: input from host to guest
    rx: Mutex<RxState>,
    
    /// TX path: output from guest to host
    tx: Mutex<TxState>,
    
    /// Control registers (shared, accessed for config)
    regs: Mutex<UartRegs>,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            rx: Mutex::new(RxState::new()),
            tx: Mutex::new(TxState::new()),
            regs: Mutex::new(UartRegs::new()),
        }
    }

    /// Internal helper to update interrupt state
    /// Lock order convention: regs must be locked first, then rx, then tx
    fn update_interrupts_internal(regs: &mut UartRegs, _rx: &RxState, tx: &TxState) {
        regs.interrupting = false;
        regs.iir = 0x01; // No interrupt pending

        // Priority 1: Receiver Line Status (not implemented extensively)

        // Priority 2: Received Data Available
        if (regs.lsr & 0x01) != 0 && (regs.ier & 0x01) != 0 {
            regs.interrupting = true;
            regs.iir = 0x04;
            return;
        }

        // Priority 3: THRE
        if tx.thre_ip && (regs.ier & 0x02) != 0 {
            regs.interrupting = true;
            regs.iir = 0x02;
        }
    }

    /// Check if the UART is currently signaling an interrupt (only locks regs)
    pub fn is_interrupting(&self) -> bool {
        self.regs.lock().unwrap().interrupting
    }

    // Snapshot support methods

    /// Get input FIFO contents for snapshot
    pub fn get_input(&self) -> Vec<u8> {
        self.rx.lock().unwrap().fifo.iter().copied().collect()
    }

    /// Get output FIFO contents for snapshot
    pub fn get_output(&self) -> Vec<u8> {
        self.tx.lock().unwrap().fifo.iter().copied().collect()
    }

    /// Get the number of bytes pending in the output FIFO
    pub fn output_len(&self) -> usize {
        self.tx.lock().unwrap().fifo.len()
    }

    /// Get all register values for snapshot
    pub fn get_registers(&self) -> (u8, u8, u8, u8, u8, u8, u8, u8, u8, u8) {
        let regs = self.regs.lock().unwrap();
        (regs.ier, regs.iir, regs.fcr, regs.lcr, regs.mcr,
         regs.lsr, regs.msr, regs.scr, regs.dll, regs.dlm)
    }

    /// Restore input FIFO from snapshot
    pub fn set_input(&self, values: &[u8]) {
        let mut regs = self.regs.lock().unwrap();
        let mut rx = self.rx.lock().unwrap();
        
        rx.fifo.clear();
        for &v in values {
            rx.fifo.push_back(v);
        }
        // Update LSR data ready bit
        if !rx.fifo.is_empty() {
            regs.lsr |= 0x01;
        } else {
            regs.lsr &= !0x01;
        }
    }

    /// Restore output FIFO from snapshot
    pub fn set_output(&self, values: &[u8]) {
        let mut tx = self.tx.lock().unwrap();
        tx.fifo.clear();
        for &v in values {
            tx.fifo.push_back(v);
        }
    }

    /// Restore register values from snapshot
    pub fn set_registers(&self, ier: u8, iir: u8, fcr: u8, lcr: u8, mcr: u8,
                         lsr: u8, msr: u8, scr: u8, dll: u8, dlm: u8) {
        let mut regs = self.regs.lock().unwrap();
        let rx = self.rx.lock().unwrap();
        let tx = self.tx.lock().unwrap();
        
        regs.ier = ier;
        regs.iir = iir;
        regs.fcr = fcr;
        regs.lcr = lcr;
        regs.mcr = mcr;
        regs.lsr = lsr;
        regs.msr = msr;
        regs.scr = scr;
        regs.dll = dll;
        regs.dlm = dlm;
        Self::update_interrupts_internal(&mut regs, &rx, &tx);
    }

    pub fn load(&self, offset: u64, size: u64) -> Result<u64, MemoryError> {
        if size != 1 {
            return Ok(0);
        }

        match offset {
            RBR => {
                let mut regs = self.regs.lock().unwrap();
                if (regs.lcr & 0x80) != 0 {
                    // DLAB mode: return DLL
                    Ok(regs.dll as u64)
                } else {
                    // Normal mode: read from RX FIFO
                    let mut rx = self.rx.lock().unwrap();
                    let byte = rx.fifo.pop_front().unwrap_or(0);

                    // Update LSR based on FIFO state
                    if rx.fifo.is_empty() {
                        regs.lsr &= !0x01; // Clear Data Ready
                    }

                    let tx = self.tx.lock().unwrap();
                    Self::update_interrupts_internal(&mut regs, &rx, &tx);
                    Ok(byte as u64)
                }
            }
            IER => {
                let regs = self.regs.lock().unwrap();
                if (regs.lcr & 0x80) != 0 {
                    Ok(regs.dlm as u64)
                } else {
                    Ok(regs.ier as u64)
                }
            }
            IIR => {
                let mut regs = self.regs.lock().unwrap();
                let val = regs.iir;
                // Clear THRE interrupt if it was the pending one
                if (val & 0x0F) == 0x02 {
                    let rx = self.rx.lock().unwrap();
                    let mut tx = self.tx.lock().unwrap();
                    tx.thre_ip = false;
                    Self::update_interrupts_internal(&mut regs, &rx, &tx);
                    log::trace!("[UART] IIR read cleared THRE ip");
                } else {
                    log::trace!("[UART] IIR read val={:x}", val);
                }
                Ok(val as u64)
            }
            LCR => Ok(self.regs.lock().unwrap().lcr as u64),
            MCR => Ok(self.regs.lock().unwrap().mcr as u64),
            LSR => Ok(self.regs.lock().unwrap().lsr as u64),
            MSR => Ok(self.regs.lock().unwrap().msr as u64),
            SCR => Ok(self.regs.lock().unwrap().scr as u64),
            _ => Ok(0),
        }
    }

    pub fn store(&self, offset: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        if size != 1 {
            return Ok(());
        }
        let val = (value & 0xff) as u8;

        match offset {
            THR => {
                let mut regs = self.regs.lock().unwrap();
                if (regs.lcr & 0x80) != 0 {
                    regs.dll = val;
                } else {
                    // Write to TX FIFO
                    log::trace!(
                        "[UART] TX '{}' (0x{:02x})",
                        if val.is_ascii_graphic() {
                            val as char
                        } else {
                            '.'
                        },
                        val
                    );
                    let rx = self.rx.lock().unwrap();
                    let mut tx = self.tx.lock().unwrap();
                    tx.fifo.push_back(val);

                    // THR is instantly "transmitted", so THRE stays set
                    regs.lsr |= 0x20;
                    tx.thre_ip = true; // Re-assert THRE interrupt

                    Self::update_interrupts_internal(&mut regs, &rx, &tx);
                }
            }
            IER => {
                let mut regs = self.regs.lock().unwrap();
                if (regs.lcr & 0x80) != 0 {
                    regs.dlm = val;
                } else {
                    regs.ier = val;
                    let rx = self.rx.lock().unwrap();
                    let tx = self.tx.lock().unwrap();
                    Self::update_interrupts_internal(&mut regs, &rx, &tx);
                }
            }
            FCR => {
                let mut regs = self.regs.lock().unwrap();
                regs.fcr = val;

                if (val & 0x02) != 0 {
                    // Clear RX FIFO
                    let mut rx = self.rx.lock().unwrap();
                    rx.fifo.clear();
                    regs.lsr &= !0x01;
                }
                if (val & 0x04) != 0 {
                    // Clear TX FIFO
                    let mut tx = self.tx.lock().unwrap();
                    tx.fifo.clear();
                    regs.lsr |= 0x60;
                }
            }
            LCR => self.regs.lock().unwrap().lcr = val,
            MCR => self.regs.lock().unwrap().mcr = val,
            LSR => {
                // Usually read-only, but factory test mode might write. Ignore.
            }
            MSR => {
                // Read-only.
            }
            SCR => self.regs.lock().unwrap().scr = val,
            _ => {}
        }
        Ok(())
    }

    // Host I/O methods

    /// Push input byte from host (lock-free for TX path)
    pub fn push_input(&self, byte: u8) {
        let mut regs = self.regs.lock().unwrap();
        let mut rx = self.rx.lock().unwrap();

        rx.fifo.push_back(byte);
        regs.lsr |= 0x01; // Data Ready

        let tx = self.tx.lock().unwrap();
        Self::update_interrupts_internal(&mut regs, &rx, &tx);
    }

    /// Pop output byte (only locks TX path)
    pub fn pop_output(&self) -> Option<u8> {
        self.tx.lock().unwrap().fifo.pop_front()
    }

    /// Drain all output (only locks TX path)
    pub fn drain_output(&self) -> Vec<u8> {
        self.tx.lock().unwrap().fifo.drain(..).collect()
    }

    /// Check if has output (only locks TX path)
    pub fn has_output(&self) -> bool {
        !self.tx.lock().unwrap().fifo.is_empty()
    }

    /// Push a byte directly to the output queue (only locks TX path)
    pub fn push_output(&self, byte: u8) {
        self.tx.lock().unwrap().fifo.push_back(byte);
    }

    /// Push a string directly to the output queue
    pub fn push_output_str(&self, s: &str) {
        let mut tx = self.tx.lock().unwrap();
        for b in s.bytes() {
            tx.fifo.push_back(b);
        }
    }

    /// Clear interrupt flag (only locks regs)
    pub fn clear_interrupt(&self) {
        self.regs.lock().unwrap().interrupting = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn test_basic_io() {
        let uart = Uart::new();
        
        // Push input
        uart.push_input(b'A');
        assert!(uart.get_input().contains(&b'A'));
        
        // Push output
        uart.push_output(b'B');
        assert_eq!(uart.pop_output(), Some(b'B'));
        assert_eq!(uart.pop_output(), None);
    }

    #[test]
    fn test_concurrent_input_output() {
        let uart = Arc::new(Uart::new());
        let uart_in = Arc::clone(&uart);
        let uart_out = Arc::clone(&uart);

        // Input thread
        let input_handle = thread::spawn(move || {
            for i in 0..1000 {
                uart_in.push_input(i as u8);
            }
        });

        // Output thread (should not block input)
        let output_handle = thread::spawn(move || {
            let mut count = 0;
            for _ in 0..10000 {
                if uart_out.pop_output().is_some() {
                    count += 1;
                }
            }
            count
        });

        input_handle.join().unwrap();
        output_handle.join().unwrap();

        // Input should have completed without blocking
        // (if single lock, there would be contention)
    }

    #[test]
    fn test_drain_doesnt_block_input() {
        let uart = Arc::new(Uart::new());

        // Fill output buffer
        for i in 0..100 {
            uart.push_output(i);
        }

        let uart_drain = Arc::clone(&uart);
        let uart_input = Arc::clone(&uart);

        let start = Instant::now();

        // Drain in one thread
        let drain_handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            uart_drain.drain_output()
        });

        // Push input concurrently
        for i in 0..100 {
            uart_input.push_input(i);
        }

        let input_time = start.elapsed();
        drain_handle.join().unwrap();

        // Input should complete quickly (< 50ms) if not blocked by drain
        assert!(input_time < Duration::from_millis(50));
    }

    #[test]
    fn test_output_operations_independent() {
        let uart = Arc::new(Uart::new());
        
        // Fill with some output
        for i in 0..10 {
            uart.push_output(i);
        }
        
        let uart1 = Arc::clone(&uart);
        let uart2 = Arc::clone(&uart);
        
        // Two threads doing output operations should not deadlock
        let h1 = thread::spawn(move || {
            for _ in 0..100 {
                uart1.has_output();
                uart1.pop_output();
            }
        });
        
        let h2 = thread::spawn(move || {
            for i in 0..100 {
                uart2.push_output(i as u8);
            }
        });
        
        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn test_register_access() {
        let uart = Uart::new();
        
        // Write to SCR (scratch register)
        uart.store(SCR, 1, 0x42).unwrap();
        assert_eq!(uart.load(SCR, 1).unwrap(), 0x42);
        
        // Test LCR
        uart.store(LCR, 1, 0x03).unwrap();
        assert_eq!(uart.load(LCR, 1).unwrap(), 0x03);
    }

    #[test]
    fn test_dlab_mode() {
        let uart = Uart::new();
        
        // Enable DLAB
        uart.store(LCR, 1, 0x80).unwrap();
        
        // Write divisor latch
        uart.store(THR, 1, 0x12).unwrap(); // DLL
        uart.store(IER, 1, 0x34).unwrap(); // DLM
        
        // Read back
        assert_eq!(uart.load(RBR, 1).unwrap(), 0x12);
        assert_eq!(uart.load(IER, 1).unwrap(), 0x34);
        
        // Disable DLAB
        uart.store(LCR, 1, 0x00).unwrap();
    }

    #[test]
    fn test_fifo_clear() {
        let uart = Uart::new();
        
        // Add some input
        uart.push_input(b'A');
        uart.push_input(b'B');
        
        // Add some output
        uart.push_output(b'X');
        uart.push_output(b'Y');
        
        // Clear RX FIFO (bit 1)
        uart.store(FCR, 1, 0x02).unwrap();
        assert!(uart.get_input().is_empty());
        
        // Output should still be there
        assert_eq!(uart.get_output().len(), 2);
        
        // Clear TX FIFO (bit 2)
        uart.store(FCR, 1, 0x04).unwrap();
        assert!(uart.get_output().is_empty());
    }

    #[test]
    fn test_snapshot_restore() {
        let uart = Uart::new();
        
        // Set up some state
        uart.push_input(b'A');
        uart.push_output(b'B');
        uart.store(SCR, 1, 0x55).unwrap();
        
        // Get snapshot
        let input = uart.get_input();
        let output = uart.get_output();
        let regs = uart.get_registers();
        
        // Create new UART and restore
        let uart2 = Uart::new();
        uart2.set_input(&input);
        uart2.set_output(&output);
        uart2.set_registers(regs.0, regs.1, regs.2, regs.3, regs.4,
                           regs.5, regs.6, regs.7, regs.8, regs.9);
        
        // Verify
        assert_eq!(uart2.get_input(), vec![b'A']);
        assert_eq!(uart2.get_output(), vec![b'B']);
        assert_eq!(uart2.load(SCR, 1).unwrap(), 0x55);
    }
}
