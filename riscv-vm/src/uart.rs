use crate::dram::MemoryError;
use std::collections::VecDeque;

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

pub struct Uart {
    pub input: VecDeque<u8>,
    pub output: VecDeque<u8>,
    
    // Registers
    pub ier: u8,
    pub iir: u8,
    pub fcr: u8,
    pub lcr: u8,
    pub mcr: u8,
    pub lsr: u8,
    pub msr: u8,
    pub scr: u8,
    
    // Divisor
    pub dll: u8,
    pub dlm: u8,

    pub interrupting: bool,
    
    /// Internal state to track if THRE interrupt is pending (waiting for IIR read or THR write).
    /// This separates the "condition" (THR empty) from the "event" (Interrupt Pending).
    thre_ip: bool,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            input: VecDeque::new(),
            output: VecDeque::new(),
            
            ier: 0x00,
            iir: 0x01, // Default: no interrupt pending
            fcr: 0x00,
            lcr: 0x00,
            mcr: 0x00,
            lsr: 0x60, // Transmitter Empty (bit 5) | Transmitter Holding Register Empty (bit 6)
            msr: 0x00,
            scr: 0x00,
            
            dll: 0x00,
            dlm: 0x00,

            interrupting: false,
            thre_ip: true, // Starts empty, so initial state could be pending if enabled
        }
    }

    pub fn update_interrupts(&mut self) {
        self.interrupting = false;
        self.iir = 0x01; // Default: no interrupt pending

        // Priority 1: Receiver Line Status (not implemented extensively)

        // Priority 2: Received Data Available
        if (self.lsr & 0x01) != 0 && (self.ier & 0x01) != 0 {
            self.interrupting = true;
            self.iir = 0x04; // Recieved Data Available
            return;
        }

        // Priority 3: Transmitter Holding Register Empty
        // Triggered if THR is empty AND IER bit 1 is set AND we haven't acknowledged it yet.
        if self.thre_ip && (self.ier & 0x02) != 0 {
             self.interrupting = true;
             self.iir = 0x02; // THRE
             return;
        }
        
        // Priority 4: Modem Status (not implemented)
    }

    pub fn load(&mut self, offset: u64, size: u64) -> Result<u64, MemoryError> {
        if size != 1 {
            // Some OS might try 4-byte reads, technically not allowed by spec but we can be lenient or strict.
            // Spec says 8-bit width. Let's return 0 for now if not byte access.
            return Ok(0);
        }

        let val = match offset {
            RBR => {
                if (self.lcr & 0x80) != 0 {
                    self.dll
                } else {
                    // RBR: Read from input FIFO
                    let byte = self.input.pop_front().unwrap_or(0);
                    // Update LSR: if more data, set bit 0, else clear it
                    if self.input.is_empty() {
                        self.lsr &= !0x01;
                    } else {
                        self.lsr |= 0x01;
                    }
                    self.update_interrupts();
                    byte
                }
            }
            IER => {
                if (self.lcr & 0x80) != 0 {
                    self.dlm
                } else {
                    self.ier
                }
            }
            IIR => {
                let val = self.iir;
                // Reading IIR clears THRE interrupt if it is the indicated interrupt
                if (val & 0x0F) == 0x02 {
                    self.thre_ip = false;
                    self.update_interrupts();
                    log::trace!("[UART] IIR read cleared THRE ip");
                } else {
                    log::trace!("[UART] IIR read val={:x} (thre_ip={})", val, self.thre_ip);
                }
                val
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => self.lsr,
            MSR => self.msr,
            SCR => self.scr,
            _ => 0,
        };

        Ok(val as u64)
    }

    pub fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), MemoryError> {
        if size != 1 {
            return Ok(());
        }

        let val = (value & 0xff) as u8;

        match offset {
            THR => {
                if (self.lcr & 0x80) != 0 {
                    self.dll = val;
                } else {
                    // THR: Write to output
                    log::trace!(
                        "[UART] TX '{}' (0x{:02x})",
                        if val.is_ascii_graphic() {
                            val as char
                        } else {
                            '.'
                        },
                        val
                    );
                    self.output.push_back(val);
                    // We instantly "transmit", so THRE (bit 5) is always set.
                    // Writing to THR clears the THRE interrupt (if pending),
                    // but since it becomes empty immediately, we set thre_ip to true again?
                    // In real HW, it goes Not Empty -> Empty.
                    // So we should clear it, then re-assert it.
                    // For edge-triggered emulation, simply re-asserting is correct because we transitioned.
                    self.lsr |= 0x20; 
                    self.thre_ip = true; 
                    self.update_interrupts();
                }
            }
            IER => {
                if (self.lcr & 0x80) != 0 {
                    self.dlm = val;
                } else {
                    self.ier = val;
                    self.update_interrupts();
                }
            }
            FCR => {
                self.fcr = val;
                if (self.fcr & 0x02) != 0 {
                    self.input.clear();
                    self.lsr &= !0x01;
                }
                if (self.fcr & 0x04) != 0 {
                    self.output.clear();
                    self.lsr |= 0x60; // Empty
                }
                self.update_interrupts();
            }
            LCR => {
                self.lcr = val;
            }
            MCR => {
                self.mcr = val;
            }
            LSR => {
                // Usually read-only, but factory test mode might write. Ignore.
            }
            MSR => {
                // Read-only.
            }
            SCR => {
                self.scr = val;
            }
            _ => {}
        }
        Ok(())
    }

    // Interface for the Host
    pub fn push_input(&mut self, byte: u8) {
        self.input.push_back(byte);
        self.lsr |= 0x01; // Data Ready
        self.update_interrupts();
    }

    pub fn pop_output(&mut self) -> Option<u8> {
        self.output.pop_front()
    }

    /// Push a byte directly to the output queue.
    /// This is used by the VM itself to emit messages (banners, status, etc.)
    /// that should appear in the same output stream as guest UART output.
    pub fn push_output(&mut self, byte: u8) {
        self.output.push_back(byte);
    }

    /// Push a string directly to the output queue.
    /// Convenience method for emitting VM messages.
    pub fn push_output_str(&mut self, s: &str) {
        for b in s.bytes() {
            self.output.push_back(b);
        }
    }
}


