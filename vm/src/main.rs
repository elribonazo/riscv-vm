use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process;
use std::sync::mpsc;
use std::thread;
use vm::bus::Bus;
use vm::cpu::Cpu;
use vm::loader::load_image;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kernel_path = match env::args().nth(1) {
        Some(arg) => arg,
        None => {
            eprintln!("Usage: vm <path-to-kernel>");
            process::exit(1);
        }
    };

    let image = fs::read(&kernel_path)?;
    let mut bus = Bus::new();
    load_image(&mut bus, &image)?;

    let mut cpu = Cpu::new(bus);
    let mut stdout = io::stdout();

    // Stdin handling
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1];
        while stdin.read_exact(&mut buf).is_ok() {
            let _ = tx.send(buf[0]);
        }
    });

    loop {
        // Check for input
        if let Ok(byte) = rx.try_recv() {
            cpu.bus.uart.push_input(byte);
        }

        if let Err(err) = cpu.step() {
            eprintln!("CPU halted: {err}");
            cpu.dump_regs();
            eprintln!(
                "Last executed instruction at {:#x}: {:#x}",
                cpu.last_pc, cpu.last_inst
            );
            if let Ok(word) = cpu.bus.load(cpu.pc, 4) {
                eprintln!(
                    "Offending instruction word at {:#x}: {:#x}",
                    cpu.pc, word as u32
                );
            }
            break;
        }

        // Drain any UART output and mirror it to the host stdout.
        while let Some(byte) = cpu.bus.uart.pop_output() {
            let _ = stdout.write_all(&[byte]);
            let _ = stdout.flush();
        }
    }

    Ok(())
}
