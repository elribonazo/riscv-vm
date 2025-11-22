#![no_std]
#![no_main]

mod allocator;
mod uart;
extern crate alloc;
use alloc::vec::Vec;
use panic_halt as _;
use riscv_rt::entry;

#[entry]
fn main() -> ! {
    uart::write_line("Booting RISC-V kernel CLI...");
    uart::write_line("Type 'help' for a list of commands.");
    // Initialize simple bump allocator
    allocator::init();
    print_prompt();

    let console = uart::Console::new();
    let mut buffer = [0u8; 128];
    let mut len = 0usize;
    let mut count: usize = 0;

    loop {
        let byte = console.read_byte();

        // 0 means "no input" in our UART model
        if byte == 0 {
            continue;
        }

        match byte {
            b'\r' | b'\n' => {
                uart::write_line("");
                handle_line(&buffer, len, &mut count);
                print_prompt();
                len = 0;
            }
            // Backspace / Delete
            8 | 0x7f => {
                if len > 0 {
                    len -= 1;
                    // Move cursor back, erase char, move back again.
                    // (Simple TTY-style backspace handling.)
                    uart::write_str("\u{8} \u{8}");
                }
            }
            _ => {
                if len < buffer.len() {
                    buffer[len] = byte;
                    len += 1;
                    uart::Console::new().write_byte(byte);
                }
            }
        }
    }
}

fn print_prompt() {
    uart::write_str("risk-v> ");
}

fn handle_line(buffer: &[u8], len: usize, count: &mut usize) {
    // Trim leading/trailing whitespace (spaces and tabs only)
    let mut start = 0;
    let mut end = len;

    while start < end && (buffer[start] == b' ' || buffer[start] == b'\t') {
        start += 1;
    }
    while end > start && (buffer[end - 1] == b' ' || buffer[end - 1] == b'\t') {
        end -= 1;
    }

    if start >= end {
        // Empty line -> keep old behaviour: bump counter
        uart::write_line("Available commands:");
        uart::write_line("  help           - show this help");
        uart::write_line("  hello          - increment and print the counter");
        uart::write_line("  count          - show current counter value");
        uart::write_line("  echo <text>    - print <text>");
        uart::write_line("  clear          - print a few newlines");
        uart::write_line("  alloc <bytes>  - allocate bytes (leaked) to test heap usage");
        return;
    }

    let line = &buffer[start..end];

    // Split into command and arguments (first whitespace)
    let mut i = 0;
    while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
        i += 1;
    }
    let cmd = &line[..i];

    let mut arg_start = i;
    while arg_start < line.len() && (line[arg_start] == b' ' || line[arg_start] == b'\t') {
        arg_start += 1;
    }
    let args = &line[arg_start..];

    if eq_cmd(cmd, b"help") {
        uart::write_line("Available commands:");
        uart::write_line("  help           - show this help");
        uart::write_line("  hello          - increment and print the counter");
        uart::write_line("  count          - show current counter value");
        uart::write_line("  echo <text>    - print <text>");
        uart::write_line("  clear          - print a few newlines");
        uart::write_line("  alloc <bytes>  - allocate bytes (leaked) to test heap usage");
    } else if eq_cmd(cmd, b"hello") {
        *count += 400;
        uart::write_str("Hello, count ");
        uart::write_u64(*count as u64);
        uart::write_line("");
    } else if eq_cmd(cmd, b"count") {
        uart::write_str("Current count: ");
        uart::write_u64(*count as u64);
        uart::write_line("");
    } else if eq_cmd(cmd, b"clear") {
        for _ in 0..20 {
            uart::write_line("");
        }
    } else if eq_cmd(cmd, b"echo") {
        uart::write_bytes(args);
        uart::write_line("");
    } else if eq_cmd(cmd, b"alloc") {
        // Parse decimal size from args
        let mut n: usize = 0;
        let mut ok = false;
        for &b in args {
            if b >= b'0' && b <= b'9' {
                ok = true;
                let d = (b - b'0') as usize;
                n = n.saturating_mul(10).saturating_add(d);
            } else if b == b' ' || b == b'\t' {
                if ok {
                    break;
                }
            } else {
                ok = false;
                break;
            }
        }
        if ok && n > 0 {
            // Allocate and leak
            let mut v: Vec<u8> = Vec::with_capacity(n);
            v.resize(n, 0);
            core::mem::forget(v);
            uart::write_str("Allocated ");
            uart::write_u64(n as u64);
            uart::write_line(" bytes (leaked).");
        } else {
            uart::write_line("Usage: alloc <bytes>");
        }
    } else {
        uart::write_str("Unknown command: ");
        uart::write_bytes(cmd);
        uart::write_line("");
    }
}

fn eq_cmd(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
