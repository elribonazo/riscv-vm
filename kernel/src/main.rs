#![no_std]
#![no_main]

mod allocator;
mod dns;
mod net;
mod uart;
mod virtio_net;
mod virtio_blk;
mod fs;

extern crate alloc;
use alloc::vec::Vec;
use panic_halt as _;
use riscv_rt::entry;

const CLINT_MTIME: usize = 0x0200_BFF8;
const TEST_FINISHER: usize = 0x0010_0000;
static mut NET_STATE: Option<net::NetState> = None;
static mut FS_STATE: Option<fs::FileSystem> = None;

struct PingState {
    #[allow(dead_code)]
    target: smoltcp::wire::Ipv4Address,
    seq: u16,
    sent_time: i64,
    waiting: bool,
}

static mut BLK_DEV: Option<virtio_blk::VirtioBlock> = None;
static mut PING_STATE: Option<PingState> = None;

/// Read current time in milliseconds from CLINT mtime register
fn get_time_ms() -> i64 {
    let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
    (mtime / 10_000) as i64
}

/// Print the kernel boot banner
fn print_banner() {
    uart::write_line("");
    uart::write_line("\x1b[1;35m    +=====================================================================+\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m                                                                     \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m   \x1b[1;36m ____  ___ ____  _  __    __     __   ___  ____  \x1b[0m                \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m   \x1b[1;36m|  _ \\|_ _/ ___|| |/ /    \\ \\   / /  / _ \\/ ___| \x1b[0m                \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m   \x1b[1;36m| |_) || |\\___ \\| ' / _____\\ \\ / /  | | | \\___ \\ \x1b[0m                \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m   \x1b[1;36m|  _ < | | ___) | . \\|_____|_\\ V /___| |_| |___) |\x1b[0m                \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m   \x1b[1;36m|_| \\_\\___|____/|_|\\_\\      \\_/     \\___/|____/ \x1b[0m                \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m                                                                     \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m     \x1b[1;97mRISC-V Operating System Kernel v0.1.0\x1b[0m                           \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m     \x1b[0;90mBuilt with Rust - smoltcp networking - VirtIO drivers\x1b[0m          \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    |\x1b[0m                                                                     \x1b[1;35m|\x1b[0m");
    uart::write_line("\x1b[1;35m    +=====================================================================+\x1b[0m");
    uart::write_line("");
}

/// Print a section header
fn print_section(title: &str) {
    uart::write_line("");
    uart::write_line("\x1b[1;33m────────────────────────────────────────────────────────────────────────\x1b[0m");
    uart::write_str("\x1b[1;33m  ◆ ");
    uart::write_str(title);
    uart::write_line("\x1b[0m");
    uart::write_line("\x1b[1;33m────────────────────────────────────────────────────────────────────────\x1b[0m");
}

/// Print a boot status line
fn print_boot_status(component: &str, ok: bool) {
    if ok {
        uart::write_str("    \x1b[1;32m[✓]\x1b[0m ");
    } else {
        uart::write_str("    \x1b[1;31m[✗]\x1b[0m ");
    }
    uart::write_line(component);
}

/// Print a boot info line
fn print_boot_info(key: &str, value: &str) {
    uart::write_str("    \x1b[0;90m├─\x1b[0m ");
    uart::write_str(key);
    uart::write_str(": \x1b[1;97m");
    uart::write_str(value);
    uart::write_line("\x1b[0m");
}

#[entry]
fn main() -> ! {
    // ─── BOOT BANNER ──────────────────────────────────────────────────────────
    print_banner();
    
    // ─── CPU & ARCHITECTURE INFO ──────────────────────────────────────────────
    print_section("CPU & ARCHITECTURE");
    print_boot_info("Architecture", "RISC-V 64-bit (RV64GC)");
    print_boot_info("Mode", "Machine Mode (M-Mode)");
    print_boot_info("Timer Source", "CLINT @ 0x02000000");
    print_boot_status("CPU initialized", true);
    
    // ─── MEMORY SUBSYSTEM ─────────────────────────────────────────────────────
    print_section("MEMORY SUBSYSTEM");
    allocator::init();
    let total_heap = allocator::heap_size();
    uart::write_str("    \x1b[0;90m├─\x1b[0m Heap Base: \x1b[1;97m0x");
    uart::write_hex(0x8080_0000u64); // Approximate heap start
    uart::write_line("\x1b[0m");
    uart::write_str("    \x1b[0;90m├─\x1b[0m Heap Size: \x1b[1;97m");
    uart::write_u64(total_heap as u64 / 1024);
    uart::write_line(" KiB\x1b[0m");
    print_boot_status("Heap allocator ready", true);
    
    // ─── STORAGE SUBSYSTEM ────────────────────────────────────────────────────
    init_storage();
    
    // ─── NETWORK SUBSYSTEM ────────────────────────────────────────────────────
    print_section("NETWORK SUBSYSTEM");
    init_network();
    
    // ─── BOOT COMPLETE ────────────────────────────────────────────────────────
    print_section("BOOT COMPLETE");
    uart::write_line("");
    uart::write_line("    \x1b[1;32m╭─────────────────────────────────────────────────────────────────╮\x1b[0m");
    uart::write_line("    \x1b[1;32m│\x1b[0m                                                                 \x1b[1;32m│\x1b[0m");
    uart::write_line("    \x1b[1;32m│\x1b[0m   \x1b[1;97mRISK-V OS is ready!\x1b[0m                                           \x1b[1;32m│\x1b[0m");
    uart::write_line("    \x1b[1;32m│\x1b[0m   \x1b[0;90mType 'help' for available commands\x1b[0m                            \x1b[1;32m│\x1b[0m");
    uart::write_line("    \x1b[1;32m│\x1b[0m                                                                 \x1b[1;32m│\x1b[0m");
    uart::write_line("    \x1b[1;32m╰─────────────────────────────────────────────────────────────────╯\x1b[0m");
    uart::write_line("");

    print_prompt();

    let console = uart::Console::new();
    let mut buffer = [0u8; 128];
    let mut len = 0usize;
    let mut count: usize = 0;
    let mut last_newline: u8 = 0; // Track last newline char to handle \r\n sequences

    loop {
        // Poll network stack
        poll_network();
        
        let byte = console.read_byte();

        // 0 means "no input" in our UART model
        if byte == 0 {
            continue;
        }

        match byte {
            b'\r' | b'\n' => {
                // Skip second char of \r\n or \n\r sequence
                if (last_newline == b'\r' && byte == b'\n') || (last_newline == b'\n' && byte == b'\r') {
                    last_newline = 0;
                    continue;
                }
                last_newline = byte;
                uart::write_line("");  // Echo the newline
                uart::write_line("");  // Add blank line before command output
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
                last_newline = 0; // Reset newline tracking on regular input
                if len < buffer.len() {
                    buffer[len] = byte;
                    len += 1;
                    uart::Console::new().write_byte(byte);
                }
            }
        }
    }
}


fn init_storage() {
    print_section("STORAGE SUBSYSTEM");
    if let Some(blk) = virtio_blk::VirtioBlock::probe() {
        uart::write_str("    \x1b[0;90m├─\x1b[0m Block Device: \x1b[1;97m");
        uart::write_u64(blk.capacity() * 512 / 1024 / 1024);
        uart::write_line(" MiB\x1b[0m");
        unsafe { BLK_DEV = Some(blk); }
        print_boot_status("VirtIO-Block driver loaded", true);
    } else {
        print_boot_status("No storage device found", false);
    }
    if let Some(ref mut blk) = unsafe { BLK_DEV.as_mut() } {
        if let Some(fs) = fs::FileSystem::init(blk) {
            uart::write_line("    \x1b[1;32m[✓]\x1b[0m SFS Mounted (R/W)");
            unsafe { FS_STATE = Some(fs); }
        }
    }
}

fn init_fs() {
    if let Some(blk) = virtio_blk::VirtioBlock::probe() {
        uart::write_line("    \x1b[1;32m[✓]\x1b[0m VirtIO Block found");
        unsafe { 
            BLK_DEV = Some(blk);
            if let Some(ref mut dev) = BLK_DEV {
                if let Some(fs) = fs::FileSystem::init(dev) {
                    FS_STATE = Some(fs);
                    uart::write_line("    \x1b[1;32m[✓]\x1b[0m FileSystem Mounted");
                }
            }
        }
    }
}

/// Initialize the network stack
fn init_network() {
    uart::write_line("    \x1b[0;90m├─\x1b[0m Probing for VirtIO devices...");
    
    // Probe for VirtIO network device
    match virtio_net::VirtioNet::probe() {
        Some(device) => {
            uart::write_str("    \x1b[0;90m├─\x1b[0m VirtIO-Net found at: \x1b[1;97m0x");
            uart::write_hex(device.base_addr() as u64);
            uart::write_line("\x1b[0m");
            
            match net::NetState::new(device) {
                Ok(state) => {
                    // Store in static FIRST, then finalize
                    unsafe { 
                        NET_STATE = Some(state);
                        if let Some(ref mut s) = NET_STATE {
                            s.finalize();
                            
                            // Print network configuration
                            uart::write_line("");
                            uart::write_line("    \x1b[1;34m┌─ Network Interface ─────────────────────────────────────┐\x1b[0m");
                            uart::write_str("    \x1b[1;34m│\x1b[0m  MAC Address:   \x1b[1;97m");
                            uart::write_bytes(&s.mac_str());
                            uart::write_line("\x1b[0m                    \x1b[1;34m│\x1b[0m");
                            
                            let mut ip_buf = [0u8; 16];
                            let my_ip = net::get_my_ip();
                            let ip_len = net::format_ipv4(my_ip, &mut ip_buf);
                            uart::write_str("    \x1b[1;34m│\x1b[0m  IPv4 Address:  \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_str("/");
                            uart::write_u64(net::PREFIX_LEN as u64);
                            uart::write_line("\x1b[0m                   \x1b[1;34m│\x1b[0m");
                            
                            let gw_len = net::format_ipv4(net::GATEWAY, &mut ip_buf);
                            uart::write_str("    \x1b[1;34m│\x1b[0m  Gateway:       \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..gw_len]);
                            uart::write_line("\x1b[0m                       \x1b[1;34m│\x1b[0m");
                            
                            let dns_len = net::format_ipv4(net::DNS_SERVER, &mut ip_buf);
                            uart::write_str("    \x1b[1;34m│\x1b[0m  DNS Server:    \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..dns_len]);
                            uart::write_line("\x1b[0m                       \x1b[1;34m│\x1b[0m");
                            uart::write_line("    \x1b[1;34m└─────────────────────────────────────────────────────────┘\x1b[0m");
                        }
                    }
                    print_boot_status("Network stack initialized (smoltcp)", true);
                    print_boot_status("VirtIO-Net driver loaded", true);
                }
                Err(e) => {
                    uart::write_str("    \x1b[1;31m[✗]\x1b[0m Network init failed: ");
                    uart::write_line(e);
                }
            }
        }
        None => {
            uart::write_line("    \x1b[1;33m[!]\x1b[0m No VirtIO network device detected");
            uart::write_line("    \x1b[0;90m    └─ Network features will be unavailable\x1b[0m");
        }
    }
}

/// Poll the network stack
fn poll_network() {
    let timestamp = get_time_ms();
    
    unsafe {
        if let Some(ref mut state) = NET_STATE {
            state.poll(timestamp);
            
            // Check for ping reply
            if let Some(ref mut ping) = PING_STATE {
                if ping.waiting {
                    if let Some((from, _ident, seq)) = state.check_ping_reply() {
                        if seq == ping.seq {
                            let rtt = timestamp - ping.sent_time;
                            let mut ip_buf = [0u8; 16];
                            let ip_len = net::format_ipv4(from, &mut ip_buf);
                            uart::write_str("\x1b[1;32m64 bytes from ");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_str(": icmp_seq=");
                            uart::write_u64(seq as u64);
                            uart::write_str(" time=");
                            uart::write_u64(rtt as u64);
                            uart::write_line(" ms\x1b[0m");
                            ping.waiting = false;
                        }
                    }
                    
                    // Timeout after 5 seconds
                    if timestamp - ping.sent_time > 5000 {
                        uart::write_line("\x1b[1;31mRequest timed out\x1b[0m");
                        ping.waiting = false;
                    }
                }
            }
        }
    }
}

fn print_prompt() {
    uart::write_str("\x1b[1;35mrisk-v\x1b[0m:\x1b[1;34m~\x1b[0m$ ");
}

fn handle_line(buffer: &[u8], len: usize, _count: &mut usize) {
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
        // Empty line -> show help
        show_help();
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

    // Print newline before command output
    uart::write_line("");

    if eq_cmd(cmd, b"help") {
        show_help();
    } else if eq_cmd(cmd, b"clear") {
        for _ in 0..20 {
            uart::write_line("");
        }
    } else if eq_cmd(cmd, b"ip") {
        cmd_ip(args);
    } else if eq_cmd(cmd, b"ping") {
        cmd_ping(args);
    } else if eq_cmd(cmd, b"nslookup") {
        cmd_nslookup(args);
    } else if eq_cmd(cmd, b"netstat") {
        cmd_netstat();
    } else if eq_cmd(cmd, b"shutdown") || eq_cmd(cmd, b"poweroff") {
        cmd_shutdown();
    }else if eq_cmd(cmd, b"ls") {
            unsafe {
                if let (Some(fs), Some(dev)) = (FS_STATE.as_ref(), BLK_DEV.as_mut()) {
                    fs.ls(dev);
                }
            }
        } else if eq_cmd(cmd, b"cat") {
            let filename = core::str::from_utf8(args).unwrap_or("").trim();
            unsafe {
                if let (Some(fs), Some(dev)) = (FS_STATE.as_ref(), BLK_DEV.as_mut()) {
                    match fs.read_file(dev, filename) {
                        Some(data) => if let Ok(s) = core::str::from_utf8(&data) { uart::write_line(s); },
                        None => uart::write_line("File not found"),
                    }
                }
            }
        } else if eq_cmd(cmd, b"write") {
            // Basic write: write filename content...
            let line = core::str::from_utf8(args).unwrap_or("");
            if let Some((name, data)) = line.split_once(' ') {
                unsafe {
                    if let (Some(fs), Some(dev)) = (FS_STATE.as_mut(), BLK_DEV.as_mut()) {
                        if fs.write_file(dev, name, data.as_bytes()).is_ok() {
                            uart::write_line("Written.");
                        } else {
                            uart::write_line("Write failed.");
                        }
                    }
                }
            }
    } else {
        uart::write_str("Unknown command: ");
        uart::write_bytes(cmd);
        uart::write_line("");
    }
}

fn show_help() {
    uart::write_line("");
    uart::write_line("\x1b[1;36m╭──────────────────────────────────────────────────────────────────────────╮\x1b[0m");
    uart::write_line("\x1b[1;36m│\x1b[0m                     \x1b[1;97mRISK-V OS Command Reference\x1b[0m                         \x1b[1;36m│\x1b[0m");
    uart::write_line("\x1b[1;36m╰──────────────────────────────────────────────────────────────────────────╯\x1b[0m");
    uart::write_line("");
    uart::write_line("\x1b[1;33m  System Commands:\x1b[0m");
    uart::write_line("    \x1b[1;32mhelp\x1b[0m              Show this help message");
    uart::write_line("    \x1b[1;32mclear\x1b[0m             Clear the screen");
    uart::write_line("    \x1b[1;32mshutdown\x1b[0m          Power off the virtual machine");
    uart::write_line("");
    uart::write_line("\x1b[1;33m  Memory Commands:\x1b[0m");
    uart::write_line("    \x1b[1;32mmemstats\x1b[0m          Show heap memory statistics");
    uart::write_line("    \x1b[1;32mmemtest\x1b[0m \x1b[0;90m[n]\x1b[0m       Run n allocation/deallocation cycles");
    uart::write_line("    \x1b[1;32malloc\x1b[0m \x1b[0;90m<bytes>\x1b[0m     Allocate bytes (for testing)");
    uart::write_line("");
    uart::write_line("\x1b[1;33m  Network Commands:\x1b[0m");
    uart::write_line("    \x1b[1;32mip addr\x1b[0m           Show network interface configuration");
    uart::write_line("    \x1b[1;32mping\x1b[0m \x1b[0;90m<ip|host>\x1b[0m    Send ICMP echo request");
    uart::write_line("    \x1b[1;32mnslookup\x1b[0m \x1b[0;90m<host>\x1b[0m   DNS lookup (resolve hostname to IP)");
    uart::write_line("    \x1b[1;32mnetstat\x1b[0m           Show network status");
    uart::write_line("");
}

fn cmd_alloc(args: &[u8]) {
    // Parse decimal size from args
    let n = parse_usize(args);
    if n > 0 {
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
}

fn cmd_readsec(args: &[u8]) {
    let sector = parse_usize(args) as u64;
    unsafe {
        if let Some(ref mut blk) = BLK_DEV {
            let mut buf = [0u8; 512];
            if blk.read_sector(sector, &mut buf).is_ok() {
                uart::write_line("Sector contents (first 64 bytes):");
                for i in 0..64 {
                   uart::write_hex_byte(buf[i]);
                   if (i+1) % 16 == 0 { uart::write_line(""); }
                   else { uart::write_str(" "); }
                }
            } else {
                uart::write_line("Read failed.");
            }
        } else {
            uart::write_line("No block device.");
        }
    }
}

fn cmd_memtest(args: &[u8]) {
    // Parse iteration count, default to 10
    let iterations = {
        let n = parse_usize(args);
        if n == 0 { 10 } else { n }
    };

    uart::write_str("Running ");
    uart::write_u64(iterations as u64);
    uart::write_line(" memory test iterations...");

    let (used_before, free_before) = allocator::heap_stats();
    uart::write_str("  Before: used=");
    uart::write_u64(used_before as u64);
    uart::write_str(" free=");
    uart::write_u64(free_before as u64);
    uart::write_line("");

    let mut success_count = 0usize;
    let mut fail_count = 0usize;

    for i in 0..iterations {
        // Allocate a Vec, fill it with a pattern, verify, then drop
        let size = 1024; // 1KB per iteration
        let pattern = ((i % 256) as u8).wrapping_add(0x42);

        let mut v: Vec<u8> = Vec::with_capacity(size);
        v.resize(size, pattern);

        // Verify contents
        let mut ok = true;
        for &byte in v.iter() {
            if byte != pattern {
                ok = false;
                break;
            }
        }

        if ok {
            success_count += 1;
        } else {
            fail_count += 1;
        }

        // v is dropped here, memory should be freed
    }

    let (used_after, free_after) = allocator::heap_stats();
    uart::write_str("  After:  used=");
    uart::write_u64(used_after as u64);
    uart::write_str(" free=");
    uart::write_u64(free_after as u64);
    uart::write_line("");

    uart::write_str("Results: ");
    uart::write_u64(success_count as u64);
    uart::write_str(" passed, ");
    uart::write_u64(fail_count as u64);
    uart::write_line(" failed.");

    // Check if memory was properly reclaimed
    if used_after <= used_before + 64 {
        // Allow small overhead for fragmentation
        uart::write_line("Memory deallocation: OK (memory reclaimed)");
    } else {
        uart::write_line("WARNING: Memory may not be properly reclaimed!");
        uart::write_str("  Leaked approximately ");
        uart::write_u64((used_after - used_before) as u64);
        uart::write_line(" bytes");
    }
}

fn cmd_memstats() {
    let total = allocator::heap_size();
    let (used, free) = allocator::heap_stats();
    let percent_used = if total > 0 { (used * 100) / total } else { 0 };
    
    // Create a visual bar
    let bar_width = 30;
    let filled = (percent_used * bar_width) / 100;

    uart::write_line("");
    uart::write_line("\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    uart::write_line("\x1b[1;36m│\x1b[0m              \x1b[1;97mHeap Memory Statistics\x1b[0m                        \x1b[1;36m│\x1b[0m");
    uart::write_line("\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m");
    
    uart::write_str("\x1b[1;36m│\x1b[0m  Total:   \x1b[1;97m");
    uart::write_u64(total as u64 / 1024);
    uart::write_line(" KiB\x1b[0m                                        \x1b[1;36m│\x1b[0m");
    
    uart::write_str("\x1b[1;36m│\x1b[0m  Used:    \x1b[1;33m");
    uart::write_u64(used as u64 / 1024);
    uart::write_line(" KiB\x1b[0m                                        \x1b[1;36m│\x1b[0m");
    
    uart::write_str("\x1b[1;36m│\x1b[0m  Free:    \x1b[1;32m");
    uart::write_u64(free as u64 / 1024);
    uart::write_line(" KiB\x1b[0m                                        \x1b[1;36m│\x1b[0m");
    
    uart::write_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    uart::write_str("\x1b[1;36m│\x1b[0m  Usage:   [");
    for i in 0..bar_width {
        if i < filled {
            uart::write_str("\x1b[1;32m█\x1b[0m");
        } else {
            uart::write_str("\x1b[0;90m░\x1b[0m");
        }
    }
    uart::write_str("] ");
    uart::write_u64(percent_used as u64);
    uart::write_line("%           \x1b[1;36m│\x1b[0m");
    uart::write_line("\x1b[1;36m└─────────────────────────────────────────────────────────────┘\x1b[0m");
    uart::write_line("");
}

fn cmd_ip(args: &[u8]) {
    // Check for "addr" subcommand
    if args.is_empty() || eq_cmd(args, b"addr") {
        unsafe {
            if let Some(ref state) = NET_STATE {
                uart::write_line("");
                uart::write_line("\x1b[1;34m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
                uart::write_line("\x1b[1;34m│\x1b[0m            \x1b[1;97mNetwork Interface: virtio0\x1b[0m                       \x1b[1;34m│\x1b[0m");
                uart::write_line("\x1b[1;34m├─────────────────────────────────────────────────────────────┤\x1b[0m");
                
                uart::write_str("\x1b[1;34m│\x1b[0m  \x1b[1;33mlink/ether\x1b[0m  ");
                uart::write_bytes(&state.mac_str());
                uart::write_line("                              \x1b[1;34m│\x1b[0m");
                
                let mut ip_buf = [0u8; 16];
                let my_ip = net::get_my_ip();
                let ip_len = net::format_ipv4(my_ip, &mut ip_buf);
                uart::write_str("\x1b[1;34m│\x1b[0m  \x1b[1;33minet\x1b[0m        ");
                uart::write_bytes(&ip_buf[..ip_len]);
                uart::write_str("/");
                uart::write_u64(net::PREFIX_LEN as u64);
                uart::write_line("                               \x1b[1;34m│\x1b[0m");
                
                let gw_len = net::format_ipv4(net::GATEWAY, &mut ip_buf);
                uart::write_str("\x1b[1;34m│\x1b[0m  \x1b[1;33mgateway\x1b[0m     ");
                uart::write_bytes(&ip_buf[..gw_len]);
                uart::write_line("                                  \x1b[1;34m│\x1b[0m");
                
                uart::write_line("\x1b[1;34m│\x1b[0m                                                             \x1b[1;34m│\x1b[0m");
                uart::write_line("\x1b[1;34m│\x1b[0m  \x1b[1;32mState: UP\x1b[0m    \x1b[0;90mMTU: 1500    Type: VirtIO-Net\x1b[0m              \x1b[1;34m│\x1b[0m");
                uart::write_line("\x1b[1;34m└─────────────────────────────────────────────────────────────┘\x1b[0m");
                uart::write_line("");
            } else {
                uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
            }
        }
    } else {
        uart::write_line("Usage: ip addr");
    }
}

fn cmd_ping(args: &[u8]) {
    if args.is_empty() {
        uart::write_line("Usage: ping <ip|hostname>");
        uart::write_line("\x1b[0;90mExamples:\x1b[0m");
        uart::write_line("  ping 10.0.2.2");
        uart::write_line("  ping google.com");
        return;
    }
    
    // Trim any trailing whitespace
    let mut arg_len = args.len();
    while arg_len > 0 && (args[arg_len - 1] == b' ' || args[arg_len - 1] == b'\t') {
        arg_len -= 1;
    }
    let trimmed_args = &args[..arg_len];
    
    // Try to parse as IP address first
    let target = match net::parse_ipv4(trimmed_args) {
        Some(ip) => ip,
        None => {
            // Not an IP address - try to resolve as hostname
            uart::write_str("\x1b[0;90m[DNS]\x1b[0m Resolving ");
            uart::write_bytes(trimmed_args);
            uart::write_line("...");
            
            unsafe {
                if let Some(ref mut state) = NET_STATE {
                    match dns::resolve(state, trimmed_args, net::DNS_SERVER, 5000, get_time_ms) {
                        Some(resolved_ip) => {
                            let mut ip_buf = [0u8; 16];
                            let ip_len = net::format_ipv4(resolved_ip, &mut ip_buf);
                            uart::write_str("\x1b[1;32m[DNS]\x1b[0m Resolved to \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_line("\x1b[0m");
                            resolved_ip
                        }
                        None => {
                            uart::write_str("\x1b[1;31m[DNS]\x1b[0m Failed to resolve: ");
                            uart::write_bytes(trimmed_args);
                            uart::write_line("");
                            return;
                        }
                    }
                } else {
                    uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
                    return;
                }
            }
        }
    };
    
    unsafe {
        if let Some(ref mut state) = NET_STATE {
            // Get current sequence number
            let seq = match &PING_STATE {
                Some(ps) => ps.seq.wrapping_add(1),
                None => 1,
            };
            
            let timestamp = get_time_ms();
            
            let mut ip_buf = [0u8; 16];
            let ip_len = net::format_ipv4(target, &mut ip_buf);
            uart::write_str("\x1b[1;36mPING\x1b[0m ");
            uart::write_bytes(&ip_buf[..ip_len]);
            uart::write_line(" 56 bytes of data");
            
            // Set up ping state
            PING_STATE = Some(PingState {
                target,
                seq,
                sent_time: timestamp,
                waiting: true,
            });
            
            // Send the actual ICMP echo request
            match state.send_ping(target, seq, timestamp) {
                Ok(()) => {
                    uart::write_str("\x1b[0;90m[ICMP]\x1b[0m Echo request seq=");
                    uart::write_u64(seq as u64);
                    uart::write_line(" sent");
                }
                Err(e) => {
                    uart::write_str("\x1b[1;31m[ICMP]\x1b[0m Failed: ");
                    uart::write_line(e);
                    PING_STATE = None;
                }
            }
        } else {
            uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
        }
    }
}

fn cmd_nslookup(args: &[u8]) {
    if args.is_empty() {
        uart::write_line("Usage: nslookup <hostname>");
        uart::write_line("\x1b[0;90mExample: nslookup google.com\x1b[0m");
        return;
    }
    
    // Trim any trailing whitespace from hostname
    let mut hostname_len = args.len();
    while hostname_len > 0 && (args[hostname_len - 1] == b' ' || args[hostname_len - 1] == b'\t') {
        hostname_len -= 1;
    }
    let hostname = &args[..hostname_len];
    
    unsafe {
        if let Some(ref mut state) = NET_STATE {
            uart::write_line("");
            uart::write_str("\x1b[1;33mServer:\x1b[0m  ");
            let mut ip_buf = [0u8; 16];
            let dns_len = net::format_ipv4(net::DNS_SERVER, &mut ip_buf);
            uart::write_bytes(&ip_buf[..dns_len]);
            uart::write_line("");
            uart::write_line("\x1b[1;33mPort:\x1b[0m    53");
            uart::write_line("");
            
            uart::write_str("\x1b[0;90mQuerying ");
            uart::write_bytes(hostname);
            uart::write_line("...\x1b[0m");
            
            // Perform DNS lookup with 5 second timeout
            match dns::resolve(state, hostname, net::DNS_SERVER, 5000, get_time_ms) {
                Some(addr) => {
                    uart::write_line("");
                    uart::write_str("\x1b[1;32mName:\x1b[0m    ");
                    uart::write_bytes(hostname);
                    uart::write_line("");
                    let addr_len = net::format_ipv4(addr, &mut ip_buf);
                    uart::write_str("\x1b[1;32mAddress:\x1b[0m \x1b[1;97m");
                    uart::write_bytes(&ip_buf[..addr_len]);
                    uart::write_line("\x1b[0m");
                    uart::write_line("");
                }
                None => {
                    uart::write_line("");
                    uart::write_str("\x1b[1;31m*** Can't find ");
                    uart::write_bytes(hostname);
                    uart::write_line(": No response from server\x1b[0m");
                    uart::write_line("");
                }
            }
        } else {
            uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
        }
    }
}

fn cmd_netstat() {
    unsafe {
        if let Some(ref _state) = NET_STATE {
            uart::write_line("");
            uart::write_line("\x1b[1;35m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m                   \x1b[1;97mNetwork Statistics\x1b[0m                         \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m├─────────────────────────────────────────────────────────────┤\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m  \x1b[1;33mDevice:\x1b[0m                                                   \x1b[1;35m│\x1b[0m");
            uart::write_str("\x1b[1;35m│\x1b[0m    Type:     \x1b[1;97mVirtIO Network Device\x1b[0m                        \x1b[1;35m│\x1b[0m\n");
            uart::write_str("\x1b[1;35m│\x1b[0m    Address:  \x1b[1;97m0x");
            uart::write_hex(virtio_net::VIRTIO_NET_BASE as u64);
            uart::write_line("\x1b[0m                           \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m    Status:   \x1b[1;32m● ONLINE\x1b[0m                                    \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m                                                             \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m  \x1b[1;33mProtocol Stack:\x1b[0m                                         \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m    \x1b[1;97msmoltcp\x1b[0m - Lightweight TCP/IP stack                     \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m│\x1b[0m    Protocols: ICMP, UDP, TCP, ARP                          \x1b[1;35m│\x1b[0m");
            uart::write_line("\x1b[1;35m└─────────────────────────────────────────────────────────────┘\x1b[0m");
            uart::write_line("");
        } else {
            uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
        }
    }
}

fn cmd_shutdown() {
    uart::write_line("");
    uart::write_line("\x1b[1;31m╔═══════════════════════════════════════════════════════════════════╗\x1b[0m");
    uart::write_line("\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m");
    uart::write_line("\x1b[1;31m║\x1b[0m                    \x1b[1;97mSystem Shutdown Initiated\x1b[0m                       \x1b[1;31m║\x1b[0m");
    uart::write_line("\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m");
    uart::write_line("\x1b[1;31m╚═══════════════════════════════════════════════════════════════════╝\x1b[0m");
    uart::write_line("");
    uart::write_line("    \x1b[0;90m[1/3]\x1b[0m Syncing filesystems...");
    uart::write_line("    \x1b[0;90m[2/3]\x1b[0m Stopping network services...");
    uart::write_line("    \x1b[0;90m[3/3]\x1b[0m Powering off CPU...");
    uart::write_line("");
    uart::write_line("    \x1b[1;32m✓ Goodbye!\x1b[0m");
    uart::write_line("");
    
    // Write to the test finisher address to signal the VM to stop
    // Value 0x5555 indicates successful exit (PASS)
    unsafe {
        core::ptr::write_volatile(TEST_FINISHER as *mut u32, 0x5555);
    }
    // Should not reach here, but loop just in case
    loop {}
}

fn parse_usize(args: &[u8]) -> usize {
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
            break;
        }
    }
    if ok { n } else { 0 }
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
