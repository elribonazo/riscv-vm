use core::fmt::{self, Write};

const UART_BASE: usize = 0x1000_0000;

pub struct Console;

impl Console {
    pub const fn new() -> Self {
        Self
    }

    #[inline(always)]
    fn data_reg() -> *mut u8 {
        UART_BASE as *mut u8
    }

    pub fn write_byte(&mut self, byte: u8) {
        unsafe {
            core::ptr::write_volatile(Self::data_reg(), byte);
        }
    }

    pub fn read_byte(&self) -> u8 {
        unsafe { core::ptr::read_volatile(Self::data_reg() as *const u8) }
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// Write a raw string to the UART without using `core::fmt`.
pub fn write_str(s: &str) {
    let mut console = Console::new();
    let _ = console.write_str(s);
}

/// Write a raw string followed by `\n`.
pub fn write_line(s: &str) {
    write_str(s);
    write_str("\n");
}

/// Write a raw byte slice to the UART.
pub fn write_bytes(bytes: &[u8]) {
    let mut console = Console::new();
    for &b in bytes {
        console.write_byte(b);
    }
}

/// Write an unsigned integer in decimal.
pub fn write_u64(mut n: u64) {
    let mut console = Console::new();

    if n == 0 {
        console.write_byte(b'0');
        return;
    }

    let mut buf = [0u8; 20]; // enough for u64
    let mut i = 0;

    while n > 0 && i < buf.len() {
        let digit = (n % 10) as u8;
        buf[i] = b'0' + digit;
        n /= 10;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        console.write_byte(buf[i]);
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ({
        $crate::uart::print_fmt(core::format_args!($($arg)*));
    });
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($fmt:expr $(, $($arg:tt)*)?) => ({
        $crate::uart::print_fmt(core::format_args!(concat!($fmt, "\n") $(, $($arg)*)?));
    });
}
