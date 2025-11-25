use clap::Parser;
use goblin::elf::{program_header::PT_LOAD, Elf};
use riscv_vm::bus::{Bus, SystemBus};
use riscv_vm::cpu::Cpu;
use riscv_vm::Trap;
use riscv_vm::csr::{CSR_MCAUSE, CSR_MEPC, CSR_MTVAL, CSR_MTVEC, CSR_SCAUSE, CSR_SEPC, CSR_STVAL, CSR_STVEC};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use riscv_vm::console::Console;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to binary to load
    #[arg(short, long)]
    kernel: PathBuf,

    /// Address to load kernel at (default 0x8000_0000)
    #[arg(long, default_value_t = 0x8000_0000)]
    load_addr: u64,

    /// DRAM size in MiB
    #[arg(long, default_value_t = 512)]
    mem_mib: usize,

    /// Optional path to a VirtIO Block disk image (e.g. xv6 fs.img)
    #[arg(long)]
    disk: Option<PathBuf>,
}

// Debug helper: dump VirtIO MMIO identity registers expected by xv6.
fn dump_virtio_id(bus: &mut SystemBus) {
    const VIRTIO0_BASE: u64 = 0x1000_1000;
    fn r32(bus: &mut SystemBus, off: u64) -> u32 {
        bus.read32(VIRTIO0_BASE + off).unwrap_or(0)
    }
    let magic = r32(bus, 0x000);
    let ver = r32(bus, 0x004);
    let devid = r32(bus, 0x008);
    let vendor = r32(bus, 0x00c);
    eprintln!(
        "VirtIO ID: MAGIC=0x{:08x} VERSION={} DEVICE_ID={} VENDOR=0x{:08x}",
        magic, ver, devid, vendor
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    let mut file = File::open(&args.kernel)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let dram_size_bytes = args
        .mem_mib
        .checked_mul(1024 * 1024)
        .ok_or("Requested memory size is too large")?;

    // Initialize DRAM at 0x8000_0000
    let dram_base = 0x8000_0000;
    let mut bus = SystemBus::new(dram_base, dram_size_bytes);

    // If a disk image is provided, wire up VirtIO Block at 0x1000_1000
    if let Some(disk_path) = &args.disk {
        let mut disk_file = File::open(disk_path)?;
        let mut disk_buf = Vec::new();
        disk_file.read_to_end(&mut disk_buf)?;
        let vblk = riscv_vm::virtio::VirtioBlock::new(disk_buf);
        bus.virtio_devices.push(Box::new(vblk));
        println!("VirtIO Block device attached at 0x1000_1000 (IRQ 1)");
    }

    let entry_pc = if buffer.starts_with(b"\x7FELF") {
        println!("Detected ELF payload, loading program segments...");
        load_elf_into_dram(&buffer, &mut bus)?
    } else {
        if args.load_addr < dram_base {
            eprintln!("Load address must be >= 0x{:x}", dram_base);
            return Ok(());
        }
        let offset = args.load_addr - dram_base;
        println!(
            "Loading raw binary ({} bytes) at 0x{:x}",
            buffer.len(),
            args.load_addr
        );
        bus.dram
            .load(&buffer, offset)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        args.load_addr
    };

    let mut cpu = Cpu::new(entry_pc);

    println!("Starting execution at 0x{:x}", cpu.pc);
    // Early probe dump (harmless if device absent): helps debug xv6 panic on probe.
    dump_virtio_id(&mut bus);

    let mut step_count = 0u64;
    let mut last_report_step = 0u64;
    
    // Initialize console for host input
    let console = Console::new();
    let mut escaped = false;

    loop {
        // Poll console input
        if let Some(b) = console.poll() {
            if escaped {
                if b == b'x' {
                    println!("\nTerminated by user.");
                    break;
                } else if b == 1 {
                    // Ctrl-A twice -> send Ctrl-A to guest
                    bus.uart.push_input(1);
                } else {
                    // Ctrl-A then something else -> send that something else
                    // (Ctrl-A is swallowed)
                    bus.uart.push_input(b);
                }
                escaped = false;
            } else {
                if b == 1 { // Ctrl-A
                    escaped = true;
                } else {
                    bus.uart.push_input(b);
                }
            }
        }

        let step_result = cpu.step(&mut bus);
        step_count += 1;
        
        // Progress report every 10M instructions (not every instruction!)
        if step_count - last_report_step >= 10_000_000 {
            // eprinteln!("[{} M insns] pc=0x{:x} mode={:?}", step_count / 1_000_000, cpu.pc, cpu.mode);
            last_report_step = step_count;
        }
        

        if let Err(trap) = step_result {
            match trap {
                // Test finisher / explicit host stop requested by the guest.
                Trap::RequestedTrap(code) => {
                    println!("Guest requested stop via test finisher: 0x{code:x}");
                    break;
                }
                // Non-recoverable emulator error: dump state and exit.
                Trap::Fatal(msg) => {
                    eprintln!("Fatal emulator error: {msg}");
                    println!("PC: 0x{:x}", cpu.pc);
                    for i in 0..32 {
                        if i % 4 == 0 {
                            println!();
                        }
                        print!("x{:<2}: 0x{:<16x} ", i, cpu.regs[i]);
                    }
                    println!();
                    break;
                }
                // Architectural traps (interrupts, page faults, ecalls, etc.)
                // are fully handled inside Cpu::handle_trap by updating CSRs
                // and redirecting PC to mtvec/stvec. We simply continue
                // stepping so that the guest handler can run.
                _other => {
                    // Traps are handled inside cpu.step() - just continue execution.
                    // Use RUST_LOG=debug to see trap details.
                    if log::log_enabled!(log::Level::Debug) {
                        let mepc  = cpu.read_csr(CSR_MEPC).unwrap_or(0);
                        let mcause = cpu.read_csr(CSR_MCAUSE).unwrap_or(0);
                        let mtval = cpu.read_csr(CSR_MTVAL).unwrap_or(0);
                        let mtvec = cpu.read_csr(CSR_MTVEC).unwrap_or(0);
                        log::debug!(
                            "Trap: {:?} pc=0x{:x} mepc=0x{:x} mcause=0x{:x} mtval=0x{:x} mtvec=0x{:x}",
                            _other, cpu.pc, mepc, mcause, mtval, mtvec
                        );
                    }
                }
            }
        }

        // Check UART output - handle raw mode by converting \n to \r\n
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut stdout_lock = stdout.lock();
        while let Some(byte) = bus.uart.pop_output() {
            // In raw terminal mode, \n alone doesn't return cursor to column 0.
            // We need to emit \r\n for proper line breaks.
            if byte == b'\n' {
                let _ = stdout_lock.write_all(b"\r\n");
            } else if byte == b'\r' {
                // Carriage return - just emit it
                let _ = stdout_lock.write_all(b"\r");
            } else {
                let _ = stdout_lock.write_all(&[byte]);
            }
        }
        let _ = stdout_lock.flush();

        // Stop if PC is 0 in Machine/Supervisor mode (likely trap to unmapped vector).
        // User mode PC=0 is valid (xv6 initcode).
        if cpu.pc == 0 && cpu.mode != riscv_vm::csr::Mode::User {
            let mepc  = cpu.read_csr(CSR_MEPC).unwrap_or(0);
            let mcause = cpu.read_csr(CSR_MCAUSE).unwrap_or(0);
            let mtval = cpu.read_csr(CSR_MTVAL).unwrap_or(0);
            let mtvec = cpu.read_csr(CSR_MTVEC).unwrap_or(0);
            let sepc  = cpu.read_csr(CSR_SEPC).unwrap_or(0);
            let scause = cpu.read_csr(CSR_SCAUSE).unwrap_or(0);
            let stval = cpu.read_csr(CSR_STVAL).unwrap_or(0);
            let stvec = cpu.read_csr(CSR_STVEC).unwrap_or(0);
            println!("PC reached 0, stopping.");
            println!(
                "Final state:\n  pc=0x{:016x} mode={:?}\n  M: mepc=0x{:016x} mcause=0x{:016x} mtval=0x{:016x} mtvec=0x{:016x}\n  S: sepc=0x{:016x} scause=0x{:016x} stval=0x{:016x} stvec=0x{:016x}",
                cpu.pc, cpu.mode, mepc, mcause, mtval, mtvec, sepc, scause, stval, stvec
            );
            break;
        }
    }

    Ok(())
}

fn load_elf_into_dram(
    buffer: &[u8],
    bus: &mut SystemBus,
) -> Result<u64, Box<dyn std::error::Error>> {
    let elf = Elf::parse(buffer)?;
    let base = bus.dram_base();
    let dram_end = base + bus.dram_size() as u64;

    for ph in &elf.program_headers {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }

        let file_size = ph.p_filesz as usize;
        let mem_size = ph.p_memsz as usize;
        let file_offset = ph.p_offset as usize;
        if file_offset + file_size > buffer.len() {
            return Err(format!(
                "ELF segment exceeds file bounds (offset 0x{:x})",
                file_offset
            )
            .into());
        }

        let target_addr = if ph.p_paddr != 0 {
            ph.p_paddr
        } else {
            ph.p_vaddr
        };
        if target_addr < base {
            return Err(format!(
                "Segment start 0x{:x} lies below DRAM base 0x{:x}",
                target_addr, base
            )
            .into());
        }
        let seg_end = target_addr
            .checked_add(mem_size as u64)
            .ok_or_else(|| "Segment end overflow".to_string())?;
        if seg_end > dram_end {
            return Err(format!(
                "Segment 0x{:x}-0x{:x} exceeds DRAM (end 0x{:x})",
                target_addr, seg_end, dram_end
            )
            .into());
        }

        let dram_offset = (target_addr - base) as u64;
        if file_size > 0 {
            let end = file_offset + file_size;
            bus.dram
                .load(&buffer[file_offset..end], dram_offset)
                .map_err(|e| format!("Failed to load segment: {}", e))?;
        }
        if mem_size > file_size {
            let zero_start = dram_offset as usize + file_size;
            bus.dram
                .zero_range(zero_start, mem_size - file_size)
                .map_err(|e| format!("Failed to zero bss: {}", e))?;
        }
        log::debug!(
            "Loaded segment: addr=0x{:x}, filesz=0x{:x}, memsz=0x{:x}",
            target_addr,
            file_size,
            mem_size
        );
    }

    Ok(elf.entry)
}
