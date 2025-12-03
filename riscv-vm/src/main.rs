use clap::Parser;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use riscv_vm::jit::JitConfig;
use riscv_vm::vm::native::NativeVm;

#[derive(Parser, Debug)]
#[command(name = "riscv-vm")]
#[command(about = "RISC-V emulator with SMP support")]
#[command(version)]
struct Args {
    /// Path to kernel ELF or binary
    #[arg(short, long)]
    kernel: PathBuf,

    /// Path to disk image (optional)
    #[arg(short, long)]
    disk: Option<PathBuf>,

    /// Number of harts (CPUs), 0 for auto-detect
    #[arg(short = 'n', long, default_value = "0")]
    harts: usize,

    /// WebTransport relay URL for networking (e.g., https://127.0.0.1:4433)
    #[arg(long)]
    net_webtransport: Option<String>,

    /// Certificate hash for WebTransport (for self-signed certs)
    #[arg(long)]
    cert_hash: Option<String>,

    /// Enable debug output
    #[arg(long)]
    debug: bool,

    /// Enable JIT compilation
    #[arg(long, default_value = "false")]
    jit: bool,

    /// JIT threshold (executions before compiling a block)
    #[arg(long, default_value = "50")]
    jit_threshold: u32,

    /// Enable JIT debug output (WAT dumps, compilation logs)
    #[arg(long, default_value = "false")]
    jit_debug: bool,
}

/// Write to stdout with \r\n line endings (for raw terminal mode)
fn uart_print(s: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for byte in s.bytes() {
        if byte == b'\n' {
            let _ = out.write_all(b"\r\n");
        } else {
            let _ = out.write_all(&[byte]);
        }
    }
    let _ = out.flush();
}

/// Write formatted output to stdout with \r\n, adding a newline at the end
macro_rules! uart_println {
    () => { uart_print("\n") };
    ($($arg:tt)*) => {{
        uart_print(&format!($($arg)*));
        uart_print("\n");
    }};
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize logging
    if args.debug {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    // Load kernel
    let kernel_data = fs::read(&args.kernel)
        .map_err(|e| format!("Failed to read kernel '{}': {}", args.kernel.display(), e))?;

    // Determine hart count - use half available cores or user-specified count
    let num_harts = if args.harts == 0 {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        (cpus / 2).max(1) // Use half the CPUs, ensure at least 1
    } else {
        args.harts
    }
    .max(1); // Ensure at least 1

    // Print banner
    uart_println!();
    uart_println!("╔══════════════════════════════════════════════════════════════╗");
    uart_println!("║              RISC-V Emulator (SMP Edition)                   ║");
    uart_println!("╠══════════════════════════════════════════════════════════════╣");
    uart_println!(
        "║  Kernel: {:50} ║",
        args.kernel
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    uart_println!("║  Harts:  {:50} ║", num_harts);
    if let Some(relay) = &args.net_webtransport {
        uart_println!("║  Network: {:49} ║", relay);
    }
    uart_println!("╚══════════════════════════════════════════════════════════════╝");
    uart_println!();

    // Create VM
    let mut vm = NativeVm::new(&kernel_data, num_harts)?;

    // Enable JIT if requested
    if args.jit {
        let config = JitConfig {
            tier1_threshold: args.jit_threshold,
            debug_wat: args.jit_debug,
            ..Default::default()
        };
        vm.enable_jit(config);
        uart_println!("[VM] JIT enabled (threshold: {})", args.jit_threshold);
    }

    // Load disk if specified
    if let Some(disk_path) = &args.disk {
        let disk_data = fs::read(disk_path)
            .map_err(|e| format!("Failed to read disk '{}': {}", disk_path.display(), e))?;
        vm.load_disk(disk_data);
        uart_println!("[VM] Loaded disk: {}", disk_path.display());
    }

    // Connect to WebTransport relay if specified
    if let Some(relay_url) = &args.net_webtransport {
        vm.connect_webtransport(relay_url, args.cert_hash.clone());
    }

    // Run VM
    vm.run();

    // Report exit status
    let halt_code = vm.shared.halt_code();
    if halt_code == 0x5555 {
        uart_println!();
        uart_println!("[VM] Clean shutdown (PASS)");
        Ok(())
    } else {
        uart_println!();
        uart_println!("[VM] Shutdown with code: {:#x}", halt_code);
        Ok(())
    }
}
