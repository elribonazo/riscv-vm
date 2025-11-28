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
    /// Path or URL to binary to load
    #[arg(short, long)]
    kernel: String,

    /// Address to load kernel at (default 0x8000_0000)
    #[arg(long, default_value_t = 0x8000_0000)]
    load_addr: u64,

    /// RAM size in MiB
    #[arg(long, default_value_t = 512)]
    mem_mib: usize,

    /// Optional path to a VirtIO Block disk image (e.g. xv6 fs.img)
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Optional TAP interface name for VirtIO network device (e.g. tap0)
    /// Requires the interface to exist: sudo ip tuntap add dev tap0 mode tap
    #[arg(long)]
    net_tap: Option<String>,

    /// Enable VirtIO network device with a dummy backend (for testing, no actual packets)
    #[arg(long)]
    net_dummy: bool,

    /// Connect to a WebSocket server for networking (e.g. ws://localhost:8765)
    /// Works on macOS and in browser/WASM
    #[arg(long)]
    net_ws: Option<String>,

    /// Connect to a WebTransport relay for networking (e.g. https://127.0.0.1:4433)
    /// Supports NAT traversal and peer-to-peer connections
    #[arg(long)]
    net_webtransport: Option<String>,

    /// Certificate hash for WebTransport (hex string)
    /// Required for self-signed certificates
    #[arg(long)]
    net_cert_hash: Option<String>,
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

fn print_vm_banner() {
    const BANNER: &str = r#"
    ┌─────────────────────────────────────────────────────────────────────────┐
    │                                                                         │
    │   ██████╗  █████╗ ██╗   ██╗██╗   ██╗    ██╗   ██╗██╗                    │
    │   ██╔══██╗██╔══██╗██║   ██║╚██╗ ██╔╝    ██║   ██║██║                    │
    │   ██████╔╝███████║██║   ██║ ╚████╔╝     ██║   ██║██║                    │
    │   ██╔══██╗██╔══██║╚██╗ ██╔╝  ╚██╔╝      ╚██╗ ██╔╝██║                    │
    │   ██████╔╝██║  ██║ ╚████╔╝    ██║        ╚████╔╝ ██║                    │
    │   ╚═════╝ ╚═╝  ╚═╝  ╚═══╝     ╚═╝         ╚═══╝  ╚═╝                    │
    │                                                                         │
    │   Bavy Virtual Machine v0.1.0                                           │
    │   64-bit RISC-V Emulator with VirtIO Support                            │
    │                                                                         │
    └─────────────────────────────────────────────────────────────────────────┘
"#;
    println!("{}", BANNER);
}

fn print_section(title: &str) {
    println!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1;33m  ▸ {}\x1b[0m", title);
    println!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
}

fn print_status(component: &str, status: &str, ok: bool) {
    let status_color = if ok { "\x1b[1;32m" } else { "\x1b[1;31m" };
    let check = if ok { "✓" } else { "✗" };
    println!("    \x1b[0;37m{:<40}\x1b[0m {}[{}] {}\x1b[0m", component, status_color, check, status);
}

fn print_info(key: &str, value: &str) {
    println!("    \x1b[0;90m├─\x1b[0m \x1b[0;37m{:<20}\x1b[0m \x1b[1;97m{}\x1b[0m", key, value);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    print_vm_banner();
    
    let args = Args::parse();

    // ─── CPU INITIALIZATION ───────────────────────────────────────────────────
    print_section("CPU INITIALIZATION");
    print_info("Architecture", "RISC-V 64-bit (RV64GC)");
    print_info("Extensions", "I, M, A, F, D, C, Zicsr, Zifencei");
    print_info("Privilege Modes", "Machine, Supervisor, User");
    print_status("CPU Core", "INITIALIZED", true);

    // ─── MEMORY SUBSYSTEM ─────────────────────────────────────────────────────
    print_section("MEMORY SUBSYSTEM");
    let dram_size_bytes = args
        .mem_mib
        .checked_mul(1024 * 1024)
        .ok_or("Requested memory size is too large")?;
    let dram_base = 0x8000_0000u64;
    
    print_info("RAM Base", &format!("0x{:08X}", dram_base));
    print_info("RAM Size", &format!("{} MiB ({} bytes)", args.mem_mib, dram_size_bytes));
    print_info("Address Range", &format!("0x{:08X} - 0x{:08X}", dram_base, dram_base + dram_size_bytes as u64));
    
    let mut bus = SystemBus::new(dram_base, dram_size_bytes);
    print_status("RAM Controller", "ONLINE", true);
    print_status("MMU (Sv39)", "READY", true);

    // ─── KERNEL LOADING ───────────────────────────────────────────────────────
    print_section("KERNEL LOADING");
    let buffer = if args.kernel.starts_with("http://") || args.kernel.starts_with("https://") {
        print_info("Source", "Remote (HTTP/HTTPS)");
        print_info("URL", &args.kernel);
        println!("    \x1b[0;90m├─\x1b[0m \x1b[0;33mDownloading...\x1b[0m");
        let response = reqwest::blocking::get(&args.kernel)?;
        if !response.status().is_success() {
            print_status("Download", "FAILED", false);
            return Err(format!("Failed to download kernel: {}", response.status()).into());
        }
        let bytes = response.bytes()?.to_vec();
        print_status("Download", "COMPLETE", true);
        bytes
    } else {
        print_info("Source", "Local filesystem");
        print_info("Path", &args.kernel);
        let mut file = File::open(&args.kernel)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        buffer
    };
    print_info("Kernel Size", &format!("{} bytes ({:.2} KiB)", buffer.len(), buffer.len() as f64 / 1024.0));


    // ─── VIRTIO DEVICE BUS ─────────────────────────────────────────────────────
    print_section("VIRTIO DEVICE BUS");
    print_info("Bus Type", "VirtIO MMIO v2");
    print_info("Base Address", "0x10001000");
    print_info("Device Spacing", "0x1000 (4 KiB)");
    
    // If a disk image is provided, wire up VirtIO Block at 0x1000_1000
    if let Some(disk_path) = &args.disk {
        let mut disk_file = File::open(disk_path)?;
        let mut disk_buf = Vec::new();
        disk_file.read_to_end(&mut disk_buf)?;
        let disk_size_mib = disk_buf.len() / (1024 * 1024);
        let vblk = riscv_vm::virtio::VirtioBlock::new(disk_buf);
        bus.virtio_devices.push(Box::new(vblk));
        println!();
        println!("    \x1b[1;35m┌─ VirtIO Block Device ─────────────────────────────────┐\x1b[0m");
        println!("    \x1b[1;35m│\x1b[0m  Address:    \x1b[1;97m0x10001000\x1b[0m                              \x1b[1;35m│\x1b[0m");
        println!("    \x1b[1;35m│\x1b[0m  IRQ:        \x1b[1;97m1\x1b[0m                                       \x1b[1;35m│\x1b[0m");
        println!("    \x1b[1;35m│\x1b[0m  Disk Size:  \x1b[1;97m{} MiB\x1b[0m                                  \x1b[1;35m│\x1b[0m", disk_size_mib);
        println!("    \x1b[1;35m│\x1b[0m  Image:      \x1b[0;90m{}\x1b[0m", disk_path.display());
        println!("    \x1b[1;35m└────────────────────────────────────────────────────────┘\x1b[0m");
        print_status("VirtIO Block", "ATTACHED", true);
    }

    // If WebTransport is provided, wire up VirtIO Net
    if let Some(wt_url) = &args.net_webtransport {
        let wt_backend = riscv_vm::net_webtransport::WebTransportBackend::new(wt_url, args.net_cert_hash.clone());
        let vnet = riscv_vm::virtio::VirtioNet::new(Box::new(wt_backend));
        let device_idx = bus.virtio_devices.len();
        let irq = 1 + device_idx;
        bus.virtio_devices.push(Box::new(vnet));
        let base_addr = 0x1000_1000 + (device_idx as u64) * 0x1000;
        println!();
        println!("    \x1b[1;34m┌─ VirtIO Network Device ───────────────────────────────┐\x1b[0m");
        println!("    \x1b[1;34m│\x1b[0m  Address:    \x1b[1;97m0x{:08X}\x1b[0m                            \x1b[1;34m│\x1b[0m", base_addr);
        println!("    \x1b[1;34m│\x1b[0m  IRQ:        \x1b[1;97m{}\x1b[0m                                       \x1b[1;34m│\x1b[0m", irq);
        println!("    \x1b[1;34m│\x1b[0m  Backend:    \x1b[1;97mWebTransport\x1b[0m                            \x1b[1;34m│\x1b[0m");
        println!("    \x1b[1;34m│\x1b[0m  Relay:      \x1b[0;90m{}\x1b[0m", wt_url);
        println!("    \x1b[1;34m└────────────────────────────────────────────────────────┘\x1b[0m");
        print_status("VirtIO Network", "ATTACHED", true);
    } 

    let entry_pc = if buffer.starts_with(b"\x7FELF") {
        print_info("Format", "ELF64 Executable");
        load_elf_into_dram(&buffer, &mut bus)?
    } else {
        if args.load_addr < dram_base {
            print_status("Load Address", "INVALID (below RAM)", false);
            return Ok(());
        }
        let offset = args.load_addr - dram_base;
        print_info("Format", "Raw Binary");
        print_info("Load Address", &format!("0x{:08X}", args.load_addr));
        bus.dram
            .load(&buffer, offset)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        args.load_addr
    };
    print_info("Entry Point", &format!("0x{:08X}", entry_pc));
    print_status("Kernel Image", "LOADED", true);

    // ─── SYSTEM PERIPHERALS ───────────────────────────────────────────────────
    print_section("SYSTEM PERIPHERALS");
    print_info("UART 16550", "0x10000000 (IRQ 10)");
    print_info("CLINT", "0x02000000 (Machine Timer)");
    print_info("PLIC", "0x0C000000 (IRQ Controller)");
    print_status("UART Console", "READY", true);
    print_status("Interrupt Controller", "CONFIGURED", true);
    
    // Early probe dump (harmless if device absent)
    dump_virtio_id(&mut bus);

    let mut cpu = Cpu::new(entry_pc);

    // ─── BOOT SEQUENCE COMPLETE ───────────────────────────────────────────────
    print_section("BOOT SEQUENCE COMPLETE");
    println!();
    println!("    \x1b[1;32m╔══════════════════════════════════════════════════════════════════════╗\x1b[0m");
    println!("    \x1b[1;32m║\x1b[0m                                                                      \x1b[1;32m║\x1b[0m");
    println!("    \x1b[1;32m║\x1b[0m   \x1b[1;97mStarting RISC-V Kernel at 0x{:08X}\x1b[0m                           \x1b[1;32m║\x1b[0m", entry_pc);
    println!("    \x1b[1;32m║\x1b[0m   \x1b[0;90mPress Ctrl-A then 'x' to terminate\x1b[0m                              \x1b[1;32m║\x1b[0m");
    println!("    \x1b[1;32m║\x1b[0m                                                                      \x1b[1;32m║\x1b[0m");
    println!("    \x1b[1;32m╚══════════════════════════════════════════════════════════════════════╝\x1b[0m");
    println!();
    println!("\x1b[1;36m══════════════════════════════════════════════════════════════════════════════\x1b[0m");
    println!("\x1b[1;33m                           KERNEL OUTPUT BEGINS\x1b[0m");
    println!("\x1b[1;36m══════════════════════════════════════════════════════════════════════════════\x1b[0m");
    println!();

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
        
        // Poll VirtIO devices for incoming network packets every 100 instructions
        // More frequent polling improves network responsiveness for interactive protocols
        if step_count % 100 == 0 {
            bus.poll_virtio();
        }
        
        // Progress report every 10M instructions (not every instruction!)
        if step_count - last_report_step >= 10_000_000 {
            // eprinteln!("[{} M insns] pc=0x{:x} mode={:?}", step_count / 1_000_000, cpu.pc, cpu.mode);
            last_report_step = step_count;
        }
        

        if let Err(trap) = step_result {
            match trap {
                // Test finisher / explicit host stop requested by the guest.
                Trap::RequestedTrap(_) => {
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
                "Segment start 0x{:x} lies below RAM base 0x{:x}",
                target_addr, base
            )
            .into());
        }
        let seg_end = target_addr
            .checked_add(mem_size as u64)
            .ok_or_else(|| "Segment end overflow".to_string())?;
        if seg_end > dram_end {
            return Err(format!(
                "Segment 0x{:x}-0x{:x} exceeds RAM (end 0x{:x})",
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
