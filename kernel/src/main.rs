#![no_std]
#![no_main]

mod allocator;
mod dns;
mod fs;
mod http;
mod net;
mod scripting;
mod tls;
mod tls12;
mod uart;
mod virtio_blk;
mod virtio_net;

extern crate alloc;
use alloc::{format, string::String, vec::Vec};
use panic_halt as _;
use riscv_rt::entry;

const CLINT_MTIME: usize = 0x0200_BFF8;
const TEST_FINISHER: usize = 0x0010_0000;
static mut NET_STATE: Option<net::NetState> = None;
static mut FS_STATE: Option<fs::FileSystem> = None;

/// State for continuous ping (like Linux ping command)
struct PingState {
    target: smoltcp::wire::Ipv4Address,
    seq: u16,
    sent_time: i64,           // Time when current ping was sent
    last_send_time: i64,      // Time when we last sent a ping (for 1s interval)
    waiting: bool,            // Waiting for reply to current ping
    continuous: bool,         // Whether running in continuous mode
    // Statistics
    packets_sent: u32,
    packets_received: u32,
    min_rtt: i64,
    max_rtt: i64,
    total_rtt: i64,
}

impl PingState {
    fn new(target: smoltcp::wire::Ipv4Address, timestamp: i64) -> Self {
        PingState {
            target,
            seq: 0,
            sent_time: timestamp,
            last_send_time: 0,
            waiting: false,
            continuous: true,
            packets_sent: 0,
            packets_received: 0,
            min_rtt: i64::MAX,
            max_rtt: 0,
            total_rtt: 0,
        }
    }
    
    fn record_reply(&mut self, rtt: i64) {
        self.packets_received += 1;
        self.total_rtt += rtt;
        if rtt < self.min_rtt {
            self.min_rtt = rtt;
        }
        if rtt > self.max_rtt {
            self.max_rtt = rtt;
        }
    }
    
    fn avg_rtt(&self) -> i64 {
        if self.packets_received > 0 {
            self.total_rtt / self.packets_received as i64
        } else {
            0
        }
    }
    
    fn packet_loss_percent(&self) -> u32 {
        if self.packets_sent > 0 {
            ((self.packets_sent - self.packets_received) * 100) / self.packets_sent
        } else {
            0
        }
    }
}

static mut BLK_DEV: Option<virtio_blk::VirtioBlock> = None;
static mut PING_STATE: Option<PingState> = None;
static mut COMMAND_RUNNING: bool = false;

// ─── CURRENT WORKING DIRECTORY ────────────────────────────────────────────────
const CWD_MAX_LEN: usize = 128;
static mut CWD: [u8; CWD_MAX_LEN] = [0u8; CWD_MAX_LEN];
static mut CWD_LEN: usize = 1; // Start with "/" (root)

/// Initialize CWD to root
fn cwd_init() {
    unsafe {
        CWD[0] = b'/';
        CWD_LEN = 1;
    }
}

/// Get current working directory as str
fn cwd_get() -> &'static str {
    unsafe {
        core::str::from_utf8(&CWD[..CWD_LEN]).unwrap_or("/")
    }
}

/// Set current working directory
fn cwd_set(path: &str) {
    unsafe {
        let bytes = path.as_bytes();
        let len = core::cmp::min(bytes.len(), CWD_MAX_LEN);
        CWD[..len].copy_from_slice(&bytes[..len]);
        CWD_LEN = len;
    }
}

// ─── OUTPUT CAPTURE FOR REDIRECTION ────────────────────────────────────────────
const OUTPUT_BUFFER_SIZE: usize = 4096;
static mut OUTPUT_BUFFER: [u8; OUTPUT_BUFFER_SIZE] = [0u8; OUTPUT_BUFFER_SIZE];
static mut OUTPUT_LEN: usize = 0;
static mut CAPTURE_MODE: bool = false;

/// Start capturing output to the buffer
fn output_capture_start() {
    unsafe {
        CAPTURE_MODE = true;
        OUTPUT_LEN = 0;
    }
}

/// Stop capturing and return the captured bytes
fn output_capture_stop() -> &'static [u8] {
    unsafe {
        CAPTURE_MODE = false;
        &OUTPUT_BUFFER[..OUTPUT_LEN]
    }
}

/// Write a string - respects capture mode
fn out_str(s: &str) {
    unsafe {
        if CAPTURE_MODE {
            for &b in s.as_bytes() {
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = b;
                    OUTPUT_LEN += 1;
                }
            }
        } else {
            uart::write_str(s);
        }
    }
}

/// Write a string with newline - respects capture mode
fn out_line(s: &str) {
    out_str(s);
    out_str("\n");
}

/// Write bytes - respects capture mode
fn out_bytes(bytes: &[u8]) {
    unsafe {
        if CAPTURE_MODE {
            for &b in bytes {
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = b;
                    OUTPUT_LEN += 1;
                }
            }
        } else {
            uart::write_bytes(bytes);
        }
    }
}

/// Write u64 - respects capture mode
fn out_u64(n: u64) {
    unsafe {
        if CAPTURE_MODE {
            if n == 0 {
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = b'0';
                    OUTPUT_LEN += 1;
                }
                return;
            }
            let mut buf = [0u8; 20];
            let mut i = 0;
            let mut val = n;
            while val > 0 && i < buf.len() {
                buf[i] = b'0' + (val % 10) as u8;
                val /= 10;
                i += 1;
            }
            while i > 0 {
                i -= 1;
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = buf[i];
                    OUTPUT_LEN += 1;
                }
            }
        } else {
            uart::write_u64(n);
        }
    }
}

/// Write hex - respects capture mode  
fn out_hex(n: u64) {
    unsafe {
        if CAPTURE_MODE {
            let hex_digits = b"0123456789abcdef";
            if n == 0 {
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = b'0';
                    OUTPUT_LEN += 1;
                }
                return;
            }
            let mut buf = [0u8; 16];
            let mut i = 0;
            let mut val = n;
            while val > 0 && i < buf.len() {
                buf[i] = hex_digits[(val & 0xf) as usize];
                val >>= 4;
                i += 1;
            }
            while i > 0 {
                i -= 1;
                if OUTPUT_LEN < OUTPUT_BUFFER_SIZE {
                    OUTPUT_BUFFER[OUTPUT_LEN] = buf[i];
                    OUTPUT_LEN += 1;
                }
            }
        } else {
            uart::write_hex(n);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RedirectMode {
    None,
    Overwrite, // >
    Append,    // >>
}

/// Read current time in milliseconds from CLINT mtime register
fn get_time_ms() -> i64 {
    let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
    (mtime / 10_000) as i64
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
    
    // ─── SCRIPTING ENGINE ──────────────────────────────────────────────────────
    // Preload scripts from /usr/bin/ into AST cache for faster first execution
    scripting::preload_scripts();
    
    // ─── NETWORK SUBSYSTEM ────────────────────────────────────────────────────
    print_section("NETWORK SUBSYSTEM");
    init_network();
    
    // ─── BOOT COMPLETE ────────────────────────────────────────────────────────
    print_section(&format!("\x1b[1;97mBAVY OS BOOT COMPLETE!\x1b[0m"));
    uart::write_line("");
    uart::write_line("");

    cwd_init();
    print_prompt();

    let console = uart::Console::new();
    let mut buffer = [0u8; 128];
    let mut len = 0usize;
    let mut count: usize = 0;
    let mut last_newline: u8 = 0; // Track last newline char to handle \r\n sequences
    
    // Command history
    const HISTORY_SIZE: usize = 16;
    let mut history: [[u8; 128]; HISTORY_SIZE] = [[0u8; 128]; HISTORY_SIZE];
    let mut history_lens: [usize; HISTORY_SIZE] = [0; HISTORY_SIZE];
    let mut history_count: usize = 0;  // Total commands stored
    let mut history_pos: usize = 0;    // Current position when navigating (0 = newest)
    let mut browsing_history: bool = false;
    
    // Escape sequence state
    let mut esc_state: u8 = 0; // 0 = normal, 1 = got ESC, 2 = got ESC[

    loop {
        // Poll network stack
        poll_network();
        
        let byte = console.read_byte();

        // 0 means "no input" in our UART model
        if byte == 0 {
            continue;
        }
        
        // Check for Ctrl+C (0x03) to cancel running commands
        if byte == 0x03 {
            if cancel_running_command() {
                // Command was cancelled, print new prompt
                print_prompt();
                len = 0;
                browsing_history = false;
                history_pos = 0;
            }
            continue;
        }
        
        // Handle escape sequences for arrow keys
        if esc_state == 1 {
            if byte == b'[' {
                esc_state = 2;
                continue;
            } else {
                esc_state = 0;
                // Fall through to handle the byte normally
            }
        } else if esc_state == 2 {
            esc_state = 0;
            match byte {
                b'A' => {
                    // Up arrow - go to older command
                    if history_count > 0 {
                        let max_pos = if history_count < HISTORY_SIZE { history_count } else { HISTORY_SIZE };
                        if history_pos < max_pos {
                            if !browsing_history {
                                browsing_history = true;
                                history_pos = 0;
                            }
                            if history_pos < max_pos {
                                // Clear current line
                                clear_input_line(len);
                                
                                // Get command from history (0 = most recent)
                                let idx = ((history_count - 1 - history_pos) % HISTORY_SIZE) as usize;
                                len = history_lens[idx];
                                buffer[..len].copy_from_slice(&history[idx][..len]);
                                
                                // Display the command
                                uart::write_bytes(&buffer[..len]);
                                
                                if history_pos + 1 < max_pos {
                                    history_pos += 1;
                                }
                            }
                        }
                    }
                    continue;
                }
                b'B' => {
                    // Down arrow - go to newer command
                    if browsing_history && history_pos > 0 {
                        history_pos -= 1;
                        
                        // Clear current line
                        clear_input_line(len);
                        
                        if history_pos == 0 {
                            // Back to empty line (current input)
                            browsing_history = false;
                            len = 0;
                        } else {
                            // Get command from history
                            let idx = ((history_count - history_pos) % HISTORY_SIZE) as usize;
                            len = history_lens[idx];
                            buffer[..len].copy_from_slice(&history[idx][..len]);
                            
                            // Display the command
                            uart::write_bytes(&buffer[..len]);
                        }
                    } else if browsing_history {
                        // At position 0, clear and go back to empty
                        clear_input_line(len);
                        browsing_history = false;
                        len = 0;
                    }
                    continue;
                }
                b'C' | b'D' => {
                    // Right/Left arrow - ignore for now
                    continue;
                }
                _ => {
                    // Unknown escape sequence, ignore
                    continue;
                }
            }
        }

        match byte {
            0x1b => {
                // ESC - start of escape sequence
                esc_state = 1;
            }
            b'\r' | b'\n' => {
                // Skip second char of \r\n or \n\r sequence
                if (last_newline == b'\r' && byte == b'\n') || (last_newline == b'\n' && byte == b'\r') {
                    last_newline = 0;
                    continue;
                }
                last_newline = byte;
                uart::write_line("");  // Echo the newline
                
                // Save to history if non-empty
                if len > 0 {
                    let idx = history_count % HISTORY_SIZE;
                    history[idx][..len].copy_from_slice(&buffer[..len]);
                    history_lens[idx] = len;
                    history_count += 1;
                }
                
                handle_line(&buffer, len, &mut count);
                print_prompt();
                len = 0;
                browsing_history = false;
                history_pos = 0;
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
            // Tab - autocomplete
            b'\t' => {
                last_newline = 0;
                let new_len = handle_tab_completion(&mut buffer, len);
                len = new_len;
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

/// Clear the current input line on the terminal
fn clear_input_line(len: usize) {
    // Move cursor back and clear each character
    for _ in 0..len {
        uart::write_str("\u{8} \u{8}");
    }
}

/// Handle tab completion
/// Returns the new buffer length after completion
fn handle_tab_completion(buffer: &mut [u8], len: usize) -> usize {
    use alloc::string::String;
    use alloc::vec::Vec;
    
    if len == 0 {
        return 0;
    }
    
    let input = match core::str::from_utf8(&buffer[..len]) {
        Ok(s) => s,
        Err(_) => return len,
    };
    
    // Find the word being completed (last space-separated token)
    let last_space = input.rfind(' ');
    let (prefix, word_to_complete) = match last_space {
        Some(pos) => (&input[..=pos], &input[pos+1..]),
        None => ("", input),
    };
    
    let is_command = prefix.is_empty();
    
    let mut matches: Vec<String> = Vec::new();
    
    if is_command {
        // Complete commands - check built-ins first
        let builtins = [
            "clear", "shutdown", "cd", "pwd", "ping", "nslookup", "node", "help",
            "ls", "cat", "echo", "cowsay", "sysinfo", "ip", "netstat", "memstats",
            "uptime", "write", "wget",
        ];
        
        for cmd in builtins.iter() {
            if cmd.starts_with(word_to_complete) {
                matches.push(String::from(*cmd));
            }
        }
        
        // Also check /usr/bin/ for scripts
        unsafe {
            if let (Some(fs), Some(dev)) = (FS_STATE.as_ref(), BLK_DEV.as_mut()) {
                let files = fs.list_dir(dev, "/");
                for f in files {
                    if f.name.starts_with("/usr/bin/") {
                        let script_name = &f.name[9..]; // Strip "/usr/bin/"
                        if script_name.starts_with(word_to_complete) {
                            // Avoid duplicates with builtins
                            if !matches.iter().any(|m| m == script_name) {
                                matches.push(String::from(script_name));
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Complete file/directory paths
        let path_to_complete = if word_to_complete.starts_with('/') {
            String::from(word_to_complete)
        } else {
            resolve_path(word_to_complete)
        };
        
        // Find the directory part and file prefix
        let (dir_path, file_prefix) = if let Some(last_slash) = path_to_complete.rfind('/') {
            if last_slash == 0 {
                ("/", &path_to_complete[1..])
            } else {
                (&path_to_complete[..last_slash], &path_to_complete[last_slash+1..])
            }
        } else {
            ("/", path_to_complete.as_str())
        };
        
        unsafe {
            if let (Some(fs), Some(dev)) = (FS_STATE.as_ref(), BLK_DEV.as_mut()) {
                let files = fs.list_dir(dev, "/");
                let mut seen_dirs: Vec<String> = Vec::new();
                
                for f in files {
                    // Check if file is in the target directory
                    let check_prefix = if dir_path == "/" {
                        "/"
                    } else {
                        dir_path
                    };
                    
                    if !f.name.starts_with(check_prefix) {
                        continue;
                    }
                    
                    // Get the part after the directory
                    let relative = if dir_path == "/" {
                        &f.name[1..]
                    } else if f.name.len() > check_prefix.len() + 1 {
                        &f.name[check_prefix.len() + 1..]
                    } else {
                        continue;
                    };
                    
                    // Get just the immediate child (first path component)
                    let child_name = if let Some(slash_pos) = relative.find('/') {
                        &relative[..slash_pos]
                    } else {
                        relative
                    };
                    
                    if child_name.is_empty() {
                        continue;
                    }
                    
                    // Check if it matches the prefix
                    if !child_name.starts_with(file_prefix) {
                        continue;
                    }
                    
                    // Check if this is a directory (has more path after)
                    let is_dir = relative.len() > child_name.len();
                    
                    let completion = if is_dir {
                        let dir_name = String::from(child_name) + "/";
                        if seen_dirs.contains(&dir_name) {
                            continue;
                        }
                        seen_dirs.push(dir_name.clone());
                        dir_name
                    } else {
                        String::from(child_name)
                    };
                    
                    if !matches.iter().any(|m| m == &completion) {
                        matches.push(completion);
                    }
                }
            }
        }
    }
    
    matches.sort();
    
    if matches.is_empty() {
        // No matches - beep or do nothing
        return len;
    }
    
    if matches.len() == 1 {
        // Single match - complete it
        let completion = &matches[0];
        let to_add = &completion[word_to_complete.len()..];
        
        // Add completion to buffer
        let new_len = len + to_add.len();
        if new_len <= buffer.len() {
            for (i, b) in to_add.bytes().enumerate() {
                buffer[len + i] = b;
            }
            uart::write_str(to_add);
            
            // Add space after command completion (not for paths ending in /)
            if is_command && new_len + 1 <= buffer.len() {
                buffer[new_len] = b' ';
                uart::write_str(" ");
                return new_len + 1;
            }
            
            return new_len;
        }
        return len;
    }
    
    // Multiple matches - find common prefix and show options
    let common = find_common_prefix(&matches);
    
    if common.len() > word_to_complete.len() {
        // Complete up to common prefix
        let to_add = &common[word_to_complete.len()..];
        let new_len = len + to_add.len();
        if new_len <= buffer.len() {
            for (i, b) in to_add.bytes().enumerate() {
                buffer[len + i] = b;
            }
            uart::write_str(to_add);
            return new_len;
        }
        return len;
    }
    
    // Show all matches
    uart::write_line("");
    let mut col = 0;
    let col_width = 16;
    let num_cols = 4;
    
    for m in &matches {
        let display_len = m.len();
        uart::write_str(m);
        
        col += 1;
        if col >= num_cols {
            uart::write_line("");
            col = 0;
        } else {
            // Pad to column width
            for _ in display_len..col_width {
                uart::write_str(" ");
            }
        }
    }
    if col > 0 {
        uart::write_line("");
    }
    
    // Redraw prompt and current input
    print_prompt();
    uart::write_bytes(&buffer[..len]);
    
    len
}

/// Find common prefix among strings
fn find_common_prefix(strings: &[alloc::string::String]) -> alloc::string::String {
    use alloc::string::String;
    
    if strings.is_empty() {
        return String::new();
    }
    
    let first = &strings[0];
    let mut prefix_len = first.len();
    
    for s in strings.iter().skip(1) {
        let mut common = 0;
        for (a, b) in first.chars().zip(s.chars()) {
            if a == b && common < prefix_len {
                common += 1;
            } else {
                break;
            }
        }
        prefix_len = common;
    }
    
    String::from(&first[..prefix_len])
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
                            uart::write_str("    \x1b[0m  MAC Address:   \x1b[1;97m");
                            uart::write_bytes(&s.mac_str());
                            uart::write_line("\x1b[0m                    \x1b[0m");
                            
                            let mut ip_buf = [0u8; 16];
                            let my_ip = net::get_my_ip();
                            let ip_len = net::format_ipv4(my_ip, &mut ip_buf);
                            uart::write_str("    \x1b[0m  IPv4 Address:  \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_str("/");
                            uart::write_u64(net::PREFIX_LEN as u64);
                            uart::write_line("\x1b[0m                   \x1b[0m");
                            
                            let gw_len = net::format_ipv4(net::GATEWAY, &mut ip_buf);
                            uart::write_str("    \x1b[0m  Gateway:       \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..gw_len]);
                            uart::write_line("\x1b[0m                       \x1b[0m");
                            
                            let dns_len = net::format_ipv4(net::DNS_SERVER, &mut ip_buf);
                            uart::write_str("    \x1b[0m  DNS Server:    \x1b[1;97m");
                            uart::write_bytes(&ip_buf[..dns_len]);
                            uart::write_line("\x1b[0m                       \x1b[0m");
                            uart::write_line("");
                        }
                    }
                    print_boot_status("Network stack initialized (smoltcp)", true);
                    print_boot_status("VirtIO-Net driver loaded", true);
                }
                Err(_e) => {
                    // Network initialization failed - no IP assigned
                    // Networking is disabled, NET_STATE remains None
                    uart::write_line("    \x1b[0;90m    └─ Network features will be unavailable\x1b[0m");
                }
            }
        }
        None => {
            uart::write_line("    \x1b[1;33m[!]\x1b[0m No VirtIO network device detected");
            uart::write_line("    \x1b[0;90m    └─ Network features will be unavailable\x1b[0m");
        }
    }
}

/// Cancel any running command (called when Ctrl+C is pressed)
fn cancel_running_command() -> bool {
    unsafe {
        if !COMMAND_RUNNING {
            return false;
        }
        
        // Check if ping is running
        if let Some(ref ping) = PING_STATE {
            if ping.continuous {
                uart::write_line("^C");
                print_ping_statistics();
                PING_STATE = None;
                COMMAND_RUNNING = false;
                return true;
            }
        }
        
        // Generic command cancellation
        COMMAND_RUNNING = false;
        uart::write_line("^C");
        true
    }
}

/// Print ping statistics summary (like Linux ping)
fn print_ping_statistics() {
    unsafe {
        if let Some(ref ping) = PING_STATE {
            let mut ip_buf = [0u8; 16];
            let ip_len = net::format_ipv4(ping.target, &mut ip_buf);
            
            uart::write_line("");
            uart::write_str("--- ");
            uart::write_bytes(&ip_buf[..ip_len]);
            uart::write_line(" ping statistics ---");
            
            uart::write_u64(ping.packets_sent as u64);
            uart::write_str(" packets transmitted, ");
            uart::write_u64(ping.packets_received as u64);
            uart::write_str(" received, ");
            uart::write_u64(ping.packet_loss_percent() as u64);
            uart::write_line("% packet loss");
            
            if ping.packets_received > 0 {
                uart::write_str("rtt min/avg/max = ");
                uart::write_u64(ping.min_rtt as u64);
                uart::write_str("/");
                uart::write_u64(ping.avg_rtt() as u64);
                uart::write_str("/");
                uart::write_u64(ping.max_rtt as u64);
                uart::write_line(" ms");
            }
            uart::write_line("");
        }
    }
}

/// Poll the network stack
fn poll_network() {
    let timestamp = get_time_ms();
    
    unsafe {
        if let Some(ref mut state) = NET_STATE {
            state.poll(timestamp);
            
            // Handle continuous ping
            if let Some(ref mut ping) = PING_STATE {
                // Check for ping reply
                if ping.waiting {
                    if let Some((from, _ident, seq)) = state.check_ping_reply() {
                        if seq == ping.seq {
                            let rtt = timestamp - ping.sent_time;
                            ping.record_reply(rtt);
                            
                            let mut ip_buf = [0u8; 16];
                            let ip_len = net::format_ipv4(from, &mut ip_buf);
                            uart::write_str("64 bytes from ");
                            uart::write_bytes(&ip_buf[..ip_len]);
                            uart::write_str(": icmp_seq=");
                            uart::write_u64(seq as u64);
                            uart::write_str(" time=");
                            uart::write_u64(rtt as u64);
                            uart::write_line(" ms");
                            ping.waiting = false;
                        }
                    }
                    
                    // Timeout after 5 seconds for current ping
                    if timestamp - ping.sent_time > 5000 {
                        uart::write_str("Request timeout for icmp_seq ");
                        uart::write_u64(ping.seq as u64);
                        uart::write_line("");
                        ping.waiting = false;
                    }
                }
                
                // In continuous mode, send next ping after 1 second interval
                if ping.continuous && !ping.waiting {
                    if timestamp - ping.last_send_time >= 1000 {
                        ping.seq = ping.seq.wrapping_add(1);
                        ping.sent_time = timestamp;
                        ping.last_send_time = timestamp;
                        ping.packets_sent += 1;
                        
                        match state.send_ping(ping.target, ping.seq, timestamp) {
                            Ok(()) => {
                                ping.waiting = true;
                            }
                            Err(_e) => {
                                // Failed to send, will retry next interval
                            }
                        }
                    }
                }
            }
        }
    }
}

fn print_prompt() {
    let cwd = cwd_get();
    let prompt_path = if cwd == "/" {
        String::new()
    } else {
        format!(" {}", cwd)
    };
    
    uart::write_str(&format!("\x1b[1;35mBavy\x1b[0m\x1b[1;34m{}\x1b[0m # ", prompt_path));
}

/// Parse a command line for redirection operators
/// Returns: (command_part, redirect_mode, filename)
fn parse_redirection(line: &[u8]) -> (&[u8], RedirectMode, &[u8]) {
    // Look for >> first (must check before >)
    for i in 0..line.len().saturating_sub(1) {
        if line[i] == b'>' && line[i + 1] == b'>' {
            let cmd_part = trim_bytes(&line[..i]);
            let file_part = trim_bytes(&line[i + 2..]);
            return (cmd_part, RedirectMode::Append, file_part);
        }
    }
    
    // Look for single >
    for i in 0..line.len() {
        if line[i] == b'>' {
            let cmd_part = trim_bytes(&line[..i]);
            let file_part = trim_bytes(&line[i + 1..]);
            return (cmd_part, RedirectMode::Overwrite, file_part);
        }
    }
    
    (line, RedirectMode::None, &[])
}

/// Trim whitespace from byte slice
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    
    while start < end && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    while end > start && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    
    &bytes[start..end]
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
        // Empty line -> do nothing
        return;
    }

    let full_line = &buffer[start..end];
    
    // Parse for redirection
    let (line, redirect_mode, redirect_file) = parse_redirection(full_line);
    
    // Validate redirection target
    if redirect_mode != RedirectMode::None && redirect_file.is_empty() {
        uart::write_line("");
        uart::write_line("\x1b[1;31mError:\x1b[0m Missing filename for redirection");
        return;
    }

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
    
    // Start capturing if redirecting
    if redirect_mode != RedirectMode::None {
        output_capture_start();
    }

    // Execute the command
    execute_command(cmd, args);
    
    // Handle redirection output
    if redirect_mode != RedirectMode::None {
        let output = output_capture_stop();
        
        if let Ok(filename) = core::str::from_utf8(redirect_file) {
            let filename = filename.trim();
            // Resolve path relative to CWD
            let resolved_path = resolve_path(filename);
            
            unsafe {
                if let (Some(fs), Some(dev)) = (FS_STATE.as_mut(), BLK_DEV.as_mut()) {
                    let final_data = if redirect_mode == RedirectMode::Append {
                        // Read existing file content and append
                        let mut combined = match fs.read_file(dev, &resolved_path) {
                            Some(existing) => existing,
                            None => Vec::new(),
                        };
                        combined.extend_from_slice(output);
                        combined
                    } else {
                        // Overwrite mode - just use new output
                        Vec::from(output)
                    };
                    
                    match fs.write_file(dev, &resolved_path, &final_data) {
                        Ok(()) => {
                            uart::write_line("");
                            uart::write_str("\x1b[1;32m✓\x1b[0m Output written to ");
                            uart::write_line(&resolved_path);
                        }
                        Err(e) => {
                            uart::write_line("");
                            uart::write_str("\x1b[1;31mError:\x1b[0m Failed to write to file: ");
                            uart::write_line(e);
                        }
                    }
                } else {
                    uart::write_line("");
                    uart::write_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
                }
            }
        } else {
            uart::write_line("");
            uart::write_line("\x1b[1;31mError:\x1b[0m Invalid filename");
        }
    }
}

/// Execute a command (separated for cleaner redirection handling)
/// 
/// Commands are resolved in this order:
/// 1. Essential built-in commands (that require direct kernel access)
/// 2. Scripts: searched in root, then /usr/bin/ directory (PATH-like)
fn execute_command(cmd: &[u8], args: &[u8]) {
    let cmd_str = core::str::from_utf8(cmd).unwrap_or("");
    let args_str = core::str::from_utf8(args).unwrap_or("");
    
    // ═══════════════════════════════════════════════════════════════════════════
    // ESSENTIAL BUILT-IN COMMANDS
    // These require direct kernel access or cannot be implemented in scripts
    // ═══════════════════════════════════════════════════════════════════════════
    
    match cmd_str {
        // System control - requires direct hardware access
        "shutdown" | "poweroff" => { cmd_shutdown(); return; }
        "clear" => { for _ in 0..50 { out_line(""); } return; }
        
        // Directory navigation - requires shell state
        "cd" => { cmd_cd(args_str); return; }
        "pwd" => { out_line(cwd_get()); return; }
        
        // Scripting engine control
        "node" => { cmd_node(args); return; }
        
        // Async network commands - require event loop integration
        "ping" => { cmd_ping(args); return; }
        "nslookup" => { cmd_nslookup(args); return; }
        
        // Low-level debugging commands
        "readsec" => { cmd_readsec(args); return; }
        "alloc" => { cmd_alloc(args); return; }
        "memtest" => { cmd_memtest(args); return; }
        
        // Help - try script first, fall back to built-in
        "help" => {
            // First try to run help script
            if let Some(script_bytes) = scripting::find_script("help") {
                run_script_bytes(&script_bytes, args_str);
                return;
            }
            // Fallback to built-in help
            cmd_help();
            return;
        }
        
        _ => {}
    }
    
    // ═══════════════════════════════════════════════════════════════════════════
    // SCRIPT RESOLUTION (PATH-like)
    // Search: 1) exact path  2) root directory  3) /usr/bin/ directory
    // ═══════════════════════════════════════════════════════════════════════════
    
    if let Some(script_bytes) = scripting::find_script(cmd_str) {
        run_script_bytes(&script_bytes, args_str);
        return;
    }
    
    // ═══════════════════════════════════════════════════════════════════════════
    // COMMAND NOT FOUND
    // ═══════════════════════════════════════════════════════════════════════════
    
    out_str("\x1b[1;31mCommand not found:\x1b[0m ");
    out_line(cmd_str);
    out_line("\x1b[0;90mTry 'help' for available commands, or check /usr/bin/ for scripts\x1b[0m");
}

/// Run a script from its bytes
fn run_script_bytes(bytes: &[u8], args: &str) {
    let script = unsafe { core::str::from_utf8_unchecked(bytes) };
    match scripting::execute_script(script, args) {
        Ok(output) => {
            if !output.is_empty() {
                out_str(&output);
            }
        }
        Err(e) => {
            out_str("\x1b[1;31mScript error:\x1b[0m ");
            out_line(&e);
        }
    }
}

/// Node scripting engine info and configuration
fn cmd_node(args: &[u8]) {
    let args_str = core::str::from_utf8(args).unwrap_or("").trim();
    
    if args_str.is_empty() || args_str == "info" {
        // Show scripting engine info
        scripting::print_info();
    } else if args_str.starts_with("log ") {
        // Set log level: node log <level>
        let level_str = args_str.strip_prefix("log ").unwrap_or("").trim();
        let level = match level_str {
            "off" | "OFF" => scripting::LogLevel::Off,
            "error" | "ERROR" => scripting::LogLevel::Error,
            "warn" | "WARN" => scripting::LogLevel::Warn,
            "info" | "INFO" => scripting::LogLevel::Info,
            "debug" | "DEBUG" => scripting::LogLevel::Debug,
            "trace" | "TRACE" => scripting::LogLevel::Trace,
            _ => {
                out_line("Usage: node log <level>");
                out_line("Levels: off, error, warn, info, debug, trace");
                return;
            }
        };
        scripting::set_log_level(level);
        out_str("\x1b[1;32m✓\x1b[0m Script log level set to: ");
        out_line(level_str);
    } else if args_str == "eval" || args_str.starts_with("eval ") {
        // Quick eval: node eval <expression>
        let expr = args_str.strip_prefix("eval").unwrap_or("").trim();
        if expr.is_empty() {
            out_line("Usage: node eval <expression>");
            out_line("Example: node eval 2 + 2 * 3");
            return;
        }
        // Use uncached execution for one-off REPL expressions
        match scripting::execute_script_uncached(expr, "") {
            Ok(output) => {
                if !output.is_empty() {
                    out_str(&output);
                }
            }
            Err(e) => {
                out_str("\x1b[1;31mError:\x1b[0m ");
                out_line(&e);
            }
        }
    } else if !args_str.is_empty() {
        // node <script> [args...] - run a script file
        let (script_name, script_args) = match args_str.split_once(' ') {
            Some((name, rest)) => (name, rest),
            None => (args_str, ""),
        };
        
        // Resolve the script path relative to CWD
        let resolved_path = if script_name.starts_with('/') {
            // Absolute path - use as-is
            alloc::string::String::from(script_name)
        } else {
            // Relative path (including ./, ../, or just "bin/cat")
            resolve_path(script_name)
        };
        
        unsafe {
            if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                match fs.read_file(dev, &resolved_path) {
                    Some(script_bytes) => {
                        if let Ok(script) = core::str::from_utf8(&script_bytes) {
                            match scripting::execute_script(script, script_args) {
                                Ok(output) => {
                                    if !output.is_empty() {
                                        out_str(&output);
                                    }
                                }
                                Err(e) => {
                                    out_str("\x1b[1;31mScript error:\x1b[0m ");
                                    out_line(&e);
                                }
                            }
                        } else {
                            out_line("\x1b[1;31mError:\x1b[0m Invalid UTF-8 in script file");
                        }
                    }
                    None => {
                        out_str("\x1b[1;31mError:\x1b[0m Script not found: ");
                        out_line(&resolved_path);
                    }
                }
            } else {
                out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
            }
        }
    }
}

/// Help command - now a script, but we keep a fallback built-in
fn cmd_help() {
    out_line("\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m                   \x1b[1;97mBAVY OS Commands\x1b[0m                        \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mBuilt-in:\x1b[0m                                                 \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    cd <dir>        Change directory                         \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    pwd             Print working directory                  \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    clear           Clear the screen                         \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    shutdown        Power off the system                     \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    ping <host>     Ping host (Ctrl+C to stop)               \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    nslookup <host> DNS lookup                               \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    node [info]     Scripting engine info/control            \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mUser Scripts:\x1b[0m  \x1b[0;90m(in /usr/bin/ - Rhai language)\x1b[0m            \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    help, ls, cat, echo, cowsay, sysinfo, ip, memstats, ...  \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mKernel API:\x1b[0m  \x1b[0;90m(available in scripts)\x1b[0m                      \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    ls(), read_file(), write_file(), file_exists()           \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    get_ip(), get_mac(), get_gateway(), net_available()      \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    time_ms(), sleep(ms), kernel_version(), arch()           \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m    heap_total(), heap_used(), heap_free()                   \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mRedirection:\x1b[0m  cmd > file | cmd >> file                    \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m  \x1b[1;32mTip:\x1b[0m  \x1b[1;97mCtrl+C\x1b[0m cancel  |  \x1b[1;97m↑/↓\x1b[0m history  |  \x1b[1;97mnode info\x1b[0m API  \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m└─────────────────────────────────────────────────────────────┘\x1b[0m");
}

// Legacy cmd_ls and cmd_cat removed - now implemented as user-space scripts
// See mkfs/root/usr/bin/ls and mkfs/root/usr/bin/cat


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

// Legacy cmd_memstats and cmd_ip removed - now implemented as user-space scripts
// See mkfs/root/usr/bin/memstats and mkfs/root/usr/bin/ip

fn cmd_ping(args: &[u8]) {
    if args.is_empty() {
        uart::write_line("Usage: ping <ip|hostname>");
        uart::write_line("\x1b[0;90mExamples:\x1b[0m");
        uart::write_line("  ping 10.0.2.2");
        uart::write_line("  ping google.com");
        uart::write_line("\x1b[0;90mPress Ctrl+C to stop\x1b[0m");
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
            let timestamp = get_time_ms();
            
            let mut ip_buf = [0u8; 16];
            let ip_len = net::format_ipv4(target, &mut ip_buf);
            uart::write_str("PING ");
            uart::write_bytes(&ip_buf[..ip_len]);
            uart::write_line(" 56(84) bytes of data.");
            
            // Set up continuous ping state
            let mut ping_state = PingState::new(target, timestamp);
            ping_state.seq = 1;
            ping_state.sent_time = timestamp;
            ping_state.last_send_time = timestamp;
            ping_state.packets_sent = 1;
            ping_state.waiting = true;
            
            // Send the first ICMP echo request immediately
            match state.send_ping(target, ping_state.seq, timestamp) {
                Ok(()) => {
                    PING_STATE = Some(ping_state);
                    COMMAND_RUNNING = true;
                }
                Err(e) => {
                    uart::write_str("ping: ");
                    uart::write_line(e);
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

// Legacy cmd_netstat removed - now implemented as user-space script
// See mkfs/root/usr/bin/netstat

/// Change directory command
fn cmd_cd(args: &str) {
    let path = args.trim();
    
    // Handle special cases
    if path.is_empty() || path == "~" {
        // Go to home directory (or root for now)
        cwd_set("/");
        return;
    }
    
    if path == "-" {
        // TODO: Previous directory (would need to track)
        out_line("cd: OLDPWD not set");
        return;
    }
    
    // Resolve the path
    let new_path = resolve_path(path);
    
    // Verify the path exists (has files under it)
    if path_exists(&new_path) {
        cwd_set(&new_path);
    } else {
        out_str("\x1b[1;31mcd:\x1b[0m ");
        out_str(path);
        out_line(": No such directory");
    }
}

/// Resolve a path relative to CWD
fn resolve_path(path: &str) -> alloc::string::String {
    use alloc::string::String;
    use alloc::vec::Vec;
    
    let mut result = String::new();
    
    // Start from root or CWD
    let base = if path.starts_with('/') {
        "/"
    } else {
        cwd_get()
    };
    
    // Combine base and path, then normalize
    let full = if path.starts_with('/') {
        String::from(path)
    } else if base == "/" {
        let mut s = String::from("/");
        s.push_str(path);
        s
    } else {
        let mut s = String::from(base);
        s.push('/');
        s.push_str(path);
        s
    };
    
    // Split and normalize (handle . and ..)
    let mut parts: Vec<&str> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => continue,
            ".." => { parts.pop(); }
            p => parts.push(p),
        }
    }
    
    // Rebuild path
    result.push('/');
    for (i, part) in parts.iter().enumerate() {
        if i > 0 { result.push('/'); }
        result.push_str(part);
    }
    
    if result.is_empty() {
        result.push('/');
    }
    
    result
}

/// Check if a path exists (has files under it or is a file)
fn path_exists(path: &str) -> bool {
    unsafe {
        if let (Some(fs), Some(dev)) = (FS_STATE.as_ref(), BLK_DEV.as_mut()) {
            // Root always exists
            if path == "/" {
                return true;
            }
            
            let files = fs.list_dir(dev, "/");
            let path_with_slash = if path.ends_with('/') {
                alloc::string::String::from(path)
            } else {
                let mut s = alloc::string::String::from(path);
                s.push('/');
                s
            };
            
            for file in files {
                // Check if any file starts with this path (it's a directory)
                if file.name.starts_with(&path_with_slash) {
                    return true;
                }
                // Or if it exactly matches (it's a file)
                if file.name == path {
                    return true;
                }
            }
        }
    }
    false
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
