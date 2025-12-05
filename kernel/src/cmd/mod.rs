use alloc::{format, string::String, vec::Vec};
use core::ptr;
use core::sync::atomic::Ordering;

use crate::{
    allocator, dns, net, scheduler, scripting, uart, BenchmarkMode, PingState, BENCHMARK, BLK_DEV,
    COMMAND_RUNNING, FS_STATE, HARTS_ONLINE, NET_STATE, PING_STATE, TEST_FINISHER,
};
use crate::{count_primes_in_range, cwd_get, cwd_set, get_time_ms, resolve_path, send_ipi};
use crate::{out_line, out_str};

// ═══════════════════════════════════════════════════════════════════════════════
// NATIVE COMMANDS - Fast implementations in Rust (no scripting overhead)
// ═══════════════════════════════════════════════════════════════════════════════

/// Try to execute a native command. Returns true if handled, false if not found.
pub fn try_native(cmd: &str, args: &str) -> bool {
    match cmd {
        "ls" => {
            native_ls(args);
            true
        }
        "cat" => {
            native_cat(args);
            true
        }
        "echo" => {
            native_echo(args);
            true
        }
        "ps" => {
            native_ps();
            true
        }
        "uptime" => {
            native_uptime();
            true
        }
        "memstats" => {
            native_memstats();
            true
        }
        "kill" => {
            native_kill(args);
            true
        }
        "sysinfo" => {
            native_sysinfo();
            true
        }
        "grep" => {
            native_grep(args);
            true
        }
        "ip" => {
            native_ip(args);
            true
        }
        "mkdir" => {
            native_mkdir(args);
            true
        }
        "netstat" => {
            native_netstat();
            true
        }
        "rm" => {
            native_rm(args);
            true
        }
        "service" => {
            native_service(args);
            true
        }
        "tail" => {
            native_tail(args);
            true
        }
        "top" => {
            native_top(args);
            true
        }
        "write" => {
            native_write(args);
            true
        }
        _ => false,
    }
}

/// Entry for ls output (either a file or inferred directory)
struct LsEntry {
    name: String,
    size: u32,
    is_dir: bool,
}

/// ls - List directory contents (native implementation)
fn native_ls(args: &str) {
    let mut show_long = false;
    let mut target_path = cwd_get();

    // Parse arguments
    for arg in args.split_whitespace() {
        if arg.starts_with('-') {
            for ch in arg.chars().skip(1) {
                if ch == 'l' {
                    show_long = true;
                }
            }
        } else {
            // Path argument
            if arg.starts_with('/') {
                target_path = String::from(arg);
            } else {
                let cwd = cwd_get();
                if cwd == "/" {
                    target_path = format!("/{}", arg);
                } else {
                    target_path = format!("{}/{}", cwd, arg);
                }
            }
        }
    }

    // Normalize: ensure no trailing slash (except for root)
    if target_path.len() > 1 && target_path.ends_with('/') {
        target_path.pop();
    }

    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        // Get ALL files (list_dir ignores path param)
        let all_files = fs.list_dir(dev, "/");

        // Build prefix for filtering
        let prefix = if target_path == "/" {
            String::from("/")
        } else {
            format!("{}/", target_path)
        };

        // Collect entries: files in this dir + inferred subdirectories
        let mut entries: Vec<LsEntry> = Vec::new();
        let mut seen_dirs: Vec<String> = Vec::new();

        for f in &all_files {
            // Check if file is under target directory
            if target_path == "/" {
                // Root: all files start with /
                if !f.name.starts_with('/') {
                    continue;
                }
            } else {
                if !f.name.starts_with(&prefix) {
                    continue;
                }
            }

            // Get relative path after the target directory
            let relative = if target_path == "/" {
                &f.name[1..] // Skip leading /
            } else {
                &f.name[prefix.len()..] // Skip prefix including trailing /
            };

            if relative.is_empty() {
                continue;
            }

            // Check if there's a subdirectory (contains /)
            if let Some(slash_pos) = relative.find('/') {
                // This is a subdirectory
                let dir_name = &relative[..slash_pos];
                if !seen_dirs.iter().any(|d| d == dir_name) {
                    seen_dirs.push(String::from(dir_name));
                    entries.push(LsEntry {
                        name: String::from(dir_name),
                        size: 0,
                        is_dir: true,
                    });
                }
            } else {
                // Direct file in this directory
                entries.push(LsEntry {
                    name: String::from(relative),
                    size: f.size,
                    is_dir: false,
                });
            }
        }

        if entries.is_empty() {
            // Check if path exists at all
            let path_exists = all_files.iter().any(|f| {
                f.name == target_path || f.name.starts_with(&prefix)
            });
            if !path_exists && target_path != "/" {
                out_str("ls: cannot access '");
                out_str(&target_path);
                out_line("': No such file or directory");
                return;
            }
            // Directory exists but is empty
            out_line("\x1b[0;90m(empty)\x1b[0m");
            return;
        }

        // Sort: directories first, then files, alphabetically
        entries.sort_by(|a, b| {
            match (a.is_dir, b.is_dir) {
                (true, false) => core::cmp::Ordering::Less,
                (false, true) => core::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            }
        });

        let is_usr_bin = target_path == "/usr/bin" || target_path.starts_with("/usr/bin/");

        if show_long {
            // Long format
            for e in &entries {
                if e.is_dir {
                    out_str(" \x1b[0;90m<dir>\x1b[0m  \x1b[1;34m");
                    out_str(&e.name);
                    out_line("/\x1b[0m");
                } else {
                    let size_str = format!("{:>6}", e.size);
                    out_str(&size_str);
                    out_str("  ");
                    if is_usr_bin {
                        out_str("\x1b[1;32m");
                    }
                    out_str(&e.name);
                    if is_usr_bin {
                        out_str("\x1b[0m");
                    }
                    out_line("");
                }
            }
            let dir_count = entries.iter().filter(|e| e.is_dir).count();
            let file_count = entries.len() - dir_count;
            out_line("");
            out_str(&format!("\x1b[0;90m{} dir(s), {} file(s)\x1b[0m", dir_count, file_count));
            out_line("");
        } else {
            // Compact columnar format
            let max_len = entries.iter()
                .map(|e| e.name.len() + if e.is_dir { 1 } else { 0 })
                .max()
                .unwrap_or(4);
            let col_width = (max_len + 2).max(4);
            let num_cols = (60 / col_width).max(1);
            let mut col = 0;

            for e in &entries {
                let display_len = e.name.len() + if e.is_dir { 1 } else { 0 };

                if e.is_dir {
                    out_str("\x1b[1;34m");
                    out_str(&e.name);
                    out_str("/\x1b[0m");
                } else if is_usr_bin {
                    out_str("\x1b[1;32m");
                    out_str(&e.name);
                    out_str("\x1b[0m");
                } else {
                    out_str(&e.name);
                }

                col += 1;
                if col >= num_cols {
                    out_line("");
                    col = 0;
                } else {
                    for _ in 0..(col_width - display_len) {
                        out_str(" ");
                    }
                }
            }
            if col > 0 {
                out_line("");
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// cat - Display file contents (native implementation)
fn native_cat(args: &str) {
    let path = args.trim();
    if path.is_empty() {
        out_line("Usage: cat <filename>");
        return;
    }

    // Resolve path
    let filename = if path.starts_with('/') {
        String::from(path)
    } else {
        let cwd = cwd_get();
        if cwd == "/" {
            format!("/{}", path)
        } else {
            format!("{}/{}", cwd, path)
        }
    };

    let fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
        match fs.read_file(dev, &filename) {
            Some(content) => {
                if let Ok(text) = core::str::from_utf8(&content) {
                    out_str(text);
                    if !text.ends_with('\n') {
                        out_line("");
                    }
                } else {
                    out_line("\x1b[1;31mError:\x1b[0m File contains invalid UTF-8");
                }
            }
            None => {
                out_str("Error: File not found: ");
                out_line(&filename);
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// echo - Print arguments (native implementation)
fn native_echo(args: &str) {
    out_line(args);
}

/// ps - List processes (native implementation)
fn native_ps() {
    out_line("\x1b[1;36m  PID  STATE  PRI     CPU    UPTIME  NAME\x1b[0m");
    out_line("\x1b[90m─────────────────────────────────────────────────────\x1b[0m");

    let tasks = scheduler::SCHEDULER.list_tasks();

    if tasks.is_empty() {
        out_line("\x1b[90m  (no processes)\x1b[0m");
    } else {
        for task in tasks {
            // Color based on state
            let color = match task.state.as_str() {
                "R+" => "\x1b[1;32m",
                "S" => "\x1b[33m",
                "Z" => "\x1b[31m",
                _ => "\x1b[0m",
            };

            out_str(color);
            out_str(&format!(
                "{:>5}  {:<6} {:<6} {:>6}ms {:>7}s  {}\x1b[0m",
                task.pid,
                task.state.as_str(),
                task.priority.as_str(),
                task.cpu_time,
                task.uptime / 1000,
                task.name
            ));
            out_line("");
        }
    }

    out_line("");
    out_line("\x1b[90mStates: R=Ready R+=Running S=Sleeping Z=Zombie\x1b[0m");
}

/// uptime - Show system uptime (native implementation)
fn native_uptime() {
    let ms = get_time_ms();
    let total_sec = ms / 1000;
    let hours = total_sec / 3600;
    let minutes = (total_sec % 3600) / 60;
    let seconds = total_sec % 60;

    if hours > 0 {
        out_str(&format!("Uptime: {}h {}m {}s", hours, minutes, seconds));
    } else if minutes > 0 {
        out_str(&format!("Uptime: {}m {}s", minutes, seconds));
    } else {
        out_str(&format!("Uptime: {}s", seconds));
    }
    out_line("");
}

/// memstats - Show memory statistics (native implementation)
fn native_memstats() {
    let total = allocator::heap_size();
    let (used, free) = allocator::heap_stats();

    let total_kb = total / 1024;
    let used_kb = used / 1024;
    let free_kb = free / 1024;
    let percent = if total > 0 { (used * 100) / total } else { 0 };

    out_line("");
    out_line("\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line("\x1b[1;36m│\x1b[0m              \x1b[1;97mHeap Memory Statistics\x1b[0m                         \x1b[1;36m│\x1b[0m");
    out_line("\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m");

    out_str(&format!(
        "\x1b[1;36m│\x1b[0m  Total:   \x1b[1;97m{} KiB\x1b[0m",
        total_kb
    ));
    // Pad to box width
    let pad = 49 - format!("{} KiB", total_kb).len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;36m│\x1b[0m");

    out_str(&format!(
        "\x1b[1;36m│\x1b[0m  Used:    \x1b[1;33m{} KiB\x1b[0m",
        used_kb
    ));
    let pad = 49 - format!("{} KiB", used_kb).len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;36m│\x1b[0m");

    out_str(&format!(
        "\x1b[1;36m│\x1b[0m  Free:    \x1b[1;32m{} KiB\x1b[0m",
        free_kb
    ));
    let pad = 49 - format!("{} KiB", free_kb).len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;36m│\x1b[0m");

    out_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");

    // Progress bar
    out_str("\x1b[1;36m│\x1b[0m  Usage:   [");
    let bar_width = 30;
    let filled = (percent * bar_width) / 100;
    for i in 0..bar_width {
        if i < filled {
            out_str("\x1b[1;32m█\x1b[0m");
        } else {
            out_str("\x1b[0;90m░\x1b[0m");
        }
    }
    out_str(&format!("] {}%", percent));
    let pad = 14 - format!("{}%", percent).len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;36m│\x1b[0m");

    out_line("\x1b[1;36m└───────────────────────────────────────────────────────────┘\x1b[0m");
    out_line("");
}

/// kill - Terminate a process (native implementation)
fn native_kill(args: &str) {
    let pid_str = args.trim();
    if pid_str.is_empty() {
        out_line("Usage: kill <pid>");
        out_line("");
        out_line("Terminate a process by its PID.");
        out_line("Use 'ps' to list running processes.");
        return;
    }

    let pid: i64 = pid_str.parse().unwrap_or(0);
    if pid <= 0 {
        out_str("\x1b[1;31mError:\x1b[0m Invalid PID: ");
        out_line(pid_str);
    } else if pid == 1 {
        out_line("\x1b[1;31mError:\x1b[0m Cannot kill init (PID 1)");
    } else {
        if scheduler::SCHEDULER.kill(pid as u32) {
            out_str("\x1b[1;32m✓\x1b[0m Killed process ");
            out_line(pid_str);
        } else {
            out_str("\x1b[1;31mError:\x1b[0m Process ");
            out_str(pid_str);
            out_line(" not found");
        }
    }
}

/// sysinfo - Display system information (native implementation)
fn native_sysinfo() {
    let version = env!("CARGO_PKG_VERSION");
    let (used, _free) = allocator::heap_stats();
    let total = allocator::heap_size();
    let uptime_ms = get_time_ms();
    let uptime_sec = uptime_ms / 1000;

    out_line("");
    out_line("\x1b[1;35m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m              \x1b[1;97mBAVY OS System Information\x1b[0m                     \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m├─────────────────────────────────────────────────────────────┤\x1b[0m");

    out_str(&format!(
        "\x1b[1;35m│\x1b[0m  Kernel:       \x1b[1;97mBAVY OS v{}\x1b[0m",
        version
    ));
    let pad = 44 - format!("BAVY OS v{}", version).len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;35m│\x1b[0m");

    out_line("\x1b[1;35m│\x1b[0m  Architecture: \x1b[1;97mRISC-V 64-bit (RV64GC)\x1b[0m                       \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m  Mode:         \x1b[1;97mMachine Mode (M-Mode)\x1b[0m                        \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m  Runtime:      \x1b[1;97mJavaScript + Native\x1b[0m                          \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m                                                             \x1b[1;35m│\x1b[0m");

    // Network status
    let net_guard = NET_STATE.lock();
    if net_guard.is_some() {
        let ip = net::get_my_ip();
        let mut ip_buf = [0u8; 16];
        let ip_len = net::format_ipv4(ip, &mut ip_buf);
        let ip_str = core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("?");
        out_str(&format!(
            "\x1b[1;35m│\x1b[0m  Network:      \x1b[1;32m● Online\x1b[0m  IP: \x1b[1;97m{}\x1b[0m",
            ip_str
        ));
        let pad = 29 - ip_str.len();
        for _ in 0..pad {
            out_str(" ");
        }
        out_line("\x1b[1;35m│\x1b[0m");
    } else {
        out_line("\x1b[1;35m│\x1b[0m  Network:      \x1b[1;31m● Offline\x1b[0m                                  \x1b[1;35m│\x1b[0m");
    }
    drop(net_guard);

    // Filesystem status
    let fs_guard = FS_STATE.lock();
    if fs_guard.is_some() {
        out_line("\x1b[1;35m│\x1b[0m  Filesystem:   \x1b[1;32m● Mounted\x1b[0m                                  \x1b[1;35m│\x1b[0m");
    } else {
        out_line("\x1b[1;35m│\x1b[0m  Filesystem:   \x1b[1;31m● Not mounted\x1b[0m                              \x1b[1;35m│\x1b[0m");
    }
    drop(fs_guard);

    out_line("\x1b[1;35m│\x1b[0m                                                             \x1b[1;35m│\x1b[0m");

    // Memory
    let mem_str = format!("{} / {} KiB", used / 1024, total / 1024);
    out_str(&format!(
        "\x1b[1;35m│\x1b[0m  Memory:       \x1b[1;97m{}\x1b[0m",
        mem_str
    ));
    let pad = 44 - mem_str.len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;35m│\x1b[0m");

    // Uptime
    let uptime_str = format!("{} seconds", uptime_sec);
    out_str(&format!(
        "\x1b[1;35m│\x1b[0m  Uptime:       \x1b[1;97m{}\x1b[0m",
        uptime_str
    ));
    let pad = 44 - uptime_str.len();
    for _ in 0..pad {
        out_str(" ");
    }
    out_line("\x1b[1;35m│\x1b[0m");

    out_line("\x1b[1;35m└─────────────────────────────────────────────────────────────┘\x1b[0m");
    out_line("");
}

/// grep - Search for patterns in files (native implementation)
fn native_grep(args: &str) {
    let mut case_insensitive = false;
    let mut show_line_numbers = false;
    let mut invert_match = false;
    let mut pattern = String::new();
    let mut files: Vec<String> = Vec::new();

    // Parse arguments
    for arg in args.split_whitespace() {
        if arg.starts_with('-') && pattern.is_empty() {
            for ch in arg.chars().skip(1) {
                match ch {
                    'i' => case_insensitive = true,
                    'n' => show_line_numbers = true,
                    'v' => invert_match = true,
                    _ => {}
                }
            }
        } else if pattern.is_empty() {
            pattern = String::from(arg);
        } else {
            files.push(String::from(arg));
        }
    }

    if pattern.is_empty() {
        out_line("Usage: grep [OPTIONS] <pattern> [file...]");
        out_line("Options: -i (case-insensitive), -n (line numbers), -v (invert)");
        return;
    }

    // If no files specified, show usage
    if files.is_empty() {
        out_line("Usage: grep [OPTIONS] <pattern> <file...>");
        return;
    }

    let search_pattern = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.clone()
    };

    let fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
        for file_arg in &files {
            // Resolve path
            let filepath = if file_arg.starts_with('/') {
                file_arg.clone()
            } else {
                let cwd = cwd_get();
                if cwd == "/" {
                    format!("/{}", file_arg)
                } else {
                    format!("{}/{}", cwd, file_arg)
                }
            };

            match fs.read_file(dev, &filepath) {
                Some(content) => {
                    if let Ok(text) = core::str::from_utf8(&content) {
                        let show_filename = files.len() > 1;
                        for (line_num, line) in text.lines().enumerate() {
                            let search_line = if case_insensitive {
                                line.to_lowercase()
                            } else {
                                String::from(line)
                            };

                            let matches = search_line.contains(&search_pattern);
                            let should_print = if invert_match { !matches } else { matches };

                            if should_print {
                                if show_filename {
                                    out_str("\x1b[1;35m");
                                    out_str(&filepath);
                                    out_str("\x1b[0m:");
                                }
                                if show_line_numbers {
                                    out_str("\x1b[1;32m");
                                    out_str(&format!("{}", line_num + 1));
                                    out_str("\x1b[0m:");
                                }
                                // Highlight match
                                if !invert_match {
                                    let idx = search_line.find(&search_pattern);
                                    if let Some(i) = idx {
                                        out_str(&line[..i]);
                                        out_str("\x1b[1;31m");
                                        out_str(&line[i..i + pattern.len()]);
                                        out_str("\x1b[0m");
                                        out_str(&line[i + pattern.len()..]);
                                        out_line("");
                                    } else {
                                        out_line(line);
                                    }
                                } else {
                                    out_line(line);
                                }
                            }
                        }
                    }
                }
                None => {
                    out_str("\x1b[1;31mgrep:\x1b[0m ");
                    out_str(&filepath);
                    out_line(": No such file");
                }
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// ip - Show network configuration (native implementation)
fn native_ip(args: &str) {
    let show_addr = args.trim().is_empty() || args.trim() == "addr";

    if !show_addr {
        out_line("Usage: ip addr");
        return;
    }

    let net_guard = NET_STATE.lock();
    if net_guard.is_none() {
        out_line("\x1b[1;31m✗\x1b[0m Network not initialized");
        return;
    }
    drop(net_guard);

    let ip = net::get_my_ip();
    let mut ip_buf = [0u8; 16];
    let ip_len = net::format_ipv4(ip, &mut ip_buf);
    let ip_str = core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("?");

    let mut gw_buf = [0u8; 16];
    let gw_len = net::format_ipv4(net::GATEWAY, &mut gw_buf);
    let gw_str = core::str::from_utf8(&gw_buf[..gw_len]).unwrap_or("?");

    let net_guard = NET_STATE.lock();
    let mac_str = if let Some(ref state) = *net_guard {
        let mac = state.mac_str();
        String::from_utf8_lossy(&mac).into_owned()
    } else {
        String::from("00:00:00:00:00:00")
    };
    drop(net_guard);

    out_line("");
    out_line("\x1b[1;34m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line("\x1b[1;34m│\x1b[0m            \x1b[1;97mNetwork Interface: virtio0\x1b[0m                       \x1b[1;34m│\x1b[0m");
    out_line("\x1b[1;34m├─────────────────────────────────────────────────────────────┤\x1b[0m");

    out_str(&format!("\x1b[1;34m│\x1b[0m  \x1b[1;33mlink/ether\x1b[0m  {}", mac_str));
    let pad = 47 - mac_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;34m│\x1b[0m");

    let inet_str = format!("{}/{}", ip_str, net::PREFIX_LEN);
    out_str(&format!("\x1b[1;34m│\x1b[0m  \x1b[1;33minet\x1b[0m        {}", inet_str));
    let pad = 47 - inet_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;34m│\x1b[0m");

    out_str(&format!("\x1b[1;34m│\x1b[0m  \x1b[1;33mgateway\x1b[0m     {}", gw_str));
    let pad = 47 - gw_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;34m│\x1b[0m");

    out_line("\x1b[1;34m│\x1b[0m                                                             \x1b[1;34m│\x1b[0m");
    out_line("\x1b[1;34m│\x1b[0m  \x1b[1;32mState: UP\x1b[0m    \x1b[0;90mMTU: 1500    Type: VirtIO-Net\x1b[0m              \x1b[1;34m│\x1b[0m");
    out_line("\x1b[1;34m└─────────────────────────────────────────────────────────────┘\x1b[0m");
    out_line("");
}

/// mkdir - Create directories (native implementation)
fn native_mkdir(args: &str) {
    let mut create_parents = false;
    let mut verbose = false;
    let mut dirs: Vec<String> = Vec::new();

    for arg in args.split_whitespace() {
        if arg.starts_with('-') {
            for ch in arg.chars().skip(1) {
                match ch {
                    'p' => create_parents = true,
                    'v' => verbose = true,
                    _ => {}
                }
            }
        } else {
            dirs.push(String::from(arg));
        }
    }

    if dirs.is_empty() {
        out_line("Usage: mkdir [-pv] <directory...>");
        return;
    }

    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        for dir in dirs {
            // Resolve path
            let path = if dir.starts_with('/') {
                dir.clone()
            } else {
                let cwd = cwd_get();
                if cwd == "/" {
                    format!("/{}", dir)
                } else {
                    format!("{}/{}", cwd, dir)
                }
            };

            if create_parents {
                // Create all parent directories
                let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                let mut current = String::new();
                for part in parts {
                    current = format!("{}/{}", current, part);
                    if !fs.is_dir(dev, &current) {
                        if fs.mkdir(dev, &current).is_ok() {
                            if verbose {
                                out_str("\x1b[1;32mmkdir:\x1b[0m created '");
                                out_str(&current);
                                out_line("'");
                            }
                        }
                    }
                }
            } else {
                match fs.mkdir(dev, &path) {
                    Ok(()) => {
                        if verbose {
                            out_str("\x1b[1;32mmkdir:\x1b[0m created '");
                            out_str(&path);
                            out_line("'");
                        }
                    }
                    Err(_) => {
                        out_str("\x1b[1;31mmkdir:\x1b[0m cannot create '");
                        out_str(&path);
                        out_line("'");
                    }
                }
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// netstat - Show network statistics (native implementation)
fn native_netstat() {
    let net_guard = NET_STATE.lock();
    if net_guard.is_none() {
        out_line("\x1b[1;31m✗\x1b[0m Network not initialized");
        return;
    }

    let mac_str = if let Some(ref state) = *net_guard {
        let mac = state.mac_str();
        String::from_utf8_lossy(&mac).into_owned()
    } else {
        String::from("00:00:00:00:00:00")
    };
    drop(net_guard);

    let ip = net::get_my_ip();
    let mut ip_buf = [0u8; 16];
    let ip_len = net::format_ipv4(ip, &mut ip_buf);
    let ip_str = core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("?");

    let mut gw_buf = [0u8; 16];
    let gw_len = net::format_ipv4(net::GATEWAY, &mut gw_buf);
    let gw_str = core::str::from_utf8(&gw_buf[..gw_len]).unwrap_or("?");

    let mut dns_buf = [0u8; 16];
    let dns_len = net::format_ipv4(net::DNS_SERVER, &mut dns_buf);
    let dns_str = core::str::from_utf8(&dns_buf[..dns_len]).unwrap_or("?");

    out_line("");
    out_line("\x1b[1;35m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m                   \x1b[1;97mNetwork Statistics\x1b[0m                        \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m├─────────────────────────────────────────────────────────────┤\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m  \x1b[1;33mDevice:\x1b[0m                                                    \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m    Type:     \x1b[1;97mVirtIO Network Device\x1b[0m                          \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m    Address:  \x1b[1;97m0x10001000\x1b[0m                                     \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m    Status:   \x1b[1;32m● ONLINE\x1b[0m                                       \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m                                                             \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m  \x1b[1;33mConfiguration:\x1b[0m                                             \x1b[1;35m│\x1b[0m");

    out_str(&format!("\x1b[1;35m│\x1b[0m    MAC:      \x1b[1;97m{}\x1b[0m", mac_str));
    let pad = 45 - mac_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;35m│\x1b[0m");

    let ip_full = format!("{}/{}", ip_str, net::PREFIX_LEN);
    out_str(&format!("\x1b[1;35m│\x1b[0m    IP:       \x1b[1;97m{}\x1b[0m", ip_full));
    let pad = 45 - ip_full.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;35m│\x1b[0m");

    out_str(&format!("\x1b[1;35m│\x1b[0m    Gateway:  \x1b[1;97m{}\x1b[0m", gw_str));
    let pad = 45 - gw_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;35m│\x1b[0m");

    out_str(&format!("\x1b[1;35m│\x1b[0m    DNS:      \x1b[1;97m{}\x1b[0m", dns_str));
    let pad = 45 - dns_str.len();
    for _ in 0..pad { out_str(" "); }
    out_line("\x1b[1;35m│\x1b[0m");

    out_line("\x1b[1;35m│\x1b[0m                                                             \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m  \x1b[1;33mProtocol Stack:\x1b[0m                                            \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m    \x1b[1;97msmoltcp\x1b[0m - Lightweight TCP/IP stack                       \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m│\x1b[0m    Protocols: ICMP, UDP, TCP, ARP                           \x1b[1;35m│\x1b[0m");
    out_line("\x1b[1;35m└─────────────────────────────────────────────────────────────┘\x1b[0m");
    out_line("");
}

/// rm - Remove files or directories (native implementation)
fn native_rm(args: &str) {
    let mut recursive = false;
    let mut force = false;
    let mut verbose = false;
    let mut files: Vec<String> = Vec::new();

    for arg in args.split_whitespace() {
        if arg.starts_with('-') {
            for ch in arg.chars().skip(1) {
                match ch {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    'v' => verbose = true,
                    _ => {}
                }
            }
        } else {
            files.push(String::from(arg));
        }
    }

    if files.is_empty() {
        out_line("Usage: rm [-rfv] <file...>");
        return;
    }

    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        for file_arg in files {
            // Resolve path
            let path = if file_arg.starts_with('/') {
                file_arg.clone()
            } else {
                let cwd = cwd_get();
                if cwd == "/" {
                    format!("/{}", file_arg)
                } else {
                    format!("{}/{}", cwd, file_arg)
                }
            };

            let is_dir = fs.is_dir(dev, &path);

            if is_dir && !recursive {
                out_str("\x1b[1;31mrm:\x1b[0m cannot remove '");
                out_str(&path);
                out_line("': Is a directory (use -r)");
                continue;
            }

            if is_dir {
                // Remove directory contents first
                let all_files = fs.list_dir(dev, "/");
                let prefix = format!("{}/", path);
                let mut children: Vec<String> = all_files
                    .iter()
                    .filter(|f| f.name.starts_with(&prefix))
                    .map(|f| f.name.clone())
                    .collect();
                // Sort by depth (deepest first)
                children.sort_by(|a, b| b.matches('/').count().cmp(&a.matches('/').count()));

                for child in children {
                    if fs.remove(dev, &child).is_ok() && verbose {
                        out_str("\x1b[1;32mremoved\x1b[0m '");
                        out_str(&child);
                        out_line("'");
                    }
                }
                // Remove directory itself
                let dir_path = format!("{}/", path);
                if fs.remove(dev, &dir_path).is_ok() && verbose {
                    out_str("\x1b[1;32mremoved directory\x1b[0m '");
                    out_str(&path);
                    out_line("'");
                }
            } else {
                match fs.remove(dev, &path) {
                    Ok(()) => {
                        if verbose {
                            out_str("\x1b[1;32mremoved\x1b[0m '");
                            out_str(&path);
                            out_line("'");
                        }
                    }
                    Err(_) => {
                        if !force {
                            out_str("\x1b[1;31mrm:\x1b[0m cannot remove '");
                            out_str(&path);
                            out_line("': No such file");
                        }
                    }
                }
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// service - Service management (native implementation)
fn native_service(args: &str) {
    let parts: Vec<&str> = args.split_whitespace().collect();

    if parts.is_empty() {
        out_line("Usage: service <name> {start|stop|restart|status}");
        out_line("       service --list");
        return;
    }

    if parts[0] == "--list" || parts[0] == "-l" {
        out_line("\x1b[1;36mAvailable services:\x1b[0m");
        let defs = crate::init::list_service_defs();
        for (name, desc) in defs {
            out_str("  ");
            out_str(&name);
            out_str(" - ");
            out_line(&desc);
        }
        return;
    }

    if parts[0] == "--status-all" || parts[0] == "-a" {
        out_line("\x1b[1;36mService Status:\x1b[0m");
        let svcs = crate::init::list_services();
        for svc in svcs {
            let color = match svc.status.as_str() {
                "running" => "\x1b[1;32m",
                "stopped" => "\x1b[1;31m",
                _ => "\x1b[1;33m",
            };
            out_str("  ");
            out_str(&format!("{:<12}", svc.name));
            out_str(color);
            out_str(&svc.status.as_str());
            out_line("\x1b[0m");
        }
        return;
    }

    if parts.len() < 2 {
        out_str("Usage: service ");
        out_str(parts[0]);
        out_line(" {start|stop|restart|status}");
        return;
    }

    let name = parts[0];
    let cmd = parts[1];

    match cmd {
        "start" => {
            out_str("Starting ");
            out_str(name);
            out_line("...");
            match crate::init::start_service(name) {
                Ok(()) => out_line("\x1b[1;32m[OK]\x1b[0m"),
                Err(_) => out_line("\x1b[1;31m[FAIL]\x1b[0m Service not found or already running"),
            }
        }
        "stop" => {
            out_str("Stopping ");
            out_str(name);
            out_line("...");
            match crate::init::stop_service(name) {
                Ok(()) => out_line("\x1b[1;32m[OK]\x1b[0m"),
                Err(_) => out_line("\x1b[1;31m[FAIL]\x1b[0m Service not found or not running"),
            }
        }
        "restart" => {
            out_str("Restarting ");
            out_str(name);
            out_line("...");
            match crate::init::restart_service(name) {
                Ok(()) => out_line("\x1b[1;32m[OK]\x1b[0m"),
                Err(_) => out_line("\x1b[1;31m[FAIL]\x1b[0m"),
            }
        }
        "status" => {
            let status = crate::init::service_status(name);
            let svcs = crate::init::list_services();
            let mut found = false;

            for svc in svcs {
                if svc.name == name {
                    found = true;
                    out_str("● ");
                    out_line(name);
                    if let Some(ref s) = status {
                        if s.as_str() == "running" {
                            out_line("   \x1b[1;32mActive: running\x1b[0m");
                            out_str("   PID: ");
                            out_line(&format!("{}", svc.pid));
                        } else {
                            out_str("   \x1b[1;31mActive: ");
                            out_str(s.as_str());
                            out_line("\x1b[0m");
                        }
                    }
                }
            }
            if !found {
                out_str("Service '");
                out_str(name);
                out_line("' not found");
            }
        }
        _ => {
            out_str("Unknown command: ");
            out_line(cmd);
            out_line("Valid commands: start, stop, restart, status");
        }
    }
}

/// tail - Show last lines of a file (native implementation)
fn native_tail(args: &str) {
    let mut num_lines: usize = 10;
    let mut files: Vec<String> = Vec::new();

    let mut iter = args.split_whitespace().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-n" {
            if let Some(n) = iter.next() {
                num_lines = n.parse().unwrap_or(10);
            }
        } else if arg.starts_with("-n") {
            let n = &arg[2..];
            num_lines = n.parse().unwrap_or(10);
        } else if arg.starts_with('-') && arg.len() > 1 {
            // Try to parse as -NUM
            if let Ok(n) = arg[1..].parse::<usize>() {
                num_lines = n;
            }
        } else {
            files.push(String::from(arg));
        }
    }

    if files.is_empty() {
        out_line("Usage: tail [-n NUM] <file...>");
        return;
    }

    let fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
        let show_headers = files.len() > 1;

        for (i, file_arg) in files.iter().enumerate() {
            // Resolve path
            let filepath = if file_arg.starts_with('/') {
                file_arg.clone()
            } else {
                let cwd = cwd_get();
                if cwd == "/" {
                    format!("/{}", file_arg)
                } else {
                    format!("{}/{}", cwd, file_arg)
                }
            };

            match fs.read_file(dev, &filepath) {
                Some(content) => {
                    if show_headers {
                        if i > 0 { out_line(""); }
                        out_str("\x1b[1m==> ");
                        out_str(&filepath);
                        out_line(" <==\x1b[0m");
                    }

                    if let Ok(text) = core::str::from_utf8(&content) {
                        let lines: Vec<&str> = text.lines().collect();
                        let start = if lines.len() > num_lines {
                            lines.len() - num_lines
                        } else {
                            0
                        };
                        for line in &lines[start..] {
                            out_line(line);
                        }
                    }
                }
                None => {
                    out_str("\x1b[1;31mtail:\x1b[0m cannot open '");
                    out_str(&filepath);
                    out_line("': No such file");
                }
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

/// Format uptime for display
fn format_uptime(ms: i64) -> String {
    let secs = ms / 1000;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;

    if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins % 60)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

/// top - Process monitor (native implementation)
fn native_top(args: &str) {
    let mut iterations = 1;
    let mut batch_mode = false;

    let mut iter = args.split_whitespace().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-n" {
            if let Some(n) = iter.next() {
                iterations = n.parse().unwrap_or(1);
            }
        } else if arg == "-b" {
            batch_mode = true;
        }
    }

    for iter_num in 0..iterations {
        if !batch_mode && iter_num == 0 {
            // Clear screen
            out_str("\x1b[2J\x1b[H");
        }

        let uptime = get_time_ms();
        let version = env!("CARGO_PKG_VERSION");
        let harts = HARTS_ONLINE.load(Ordering::Relaxed);

        // Header
        if batch_mode {
            out_line("═══════════════════════════════════════════════════════════════════");
            out_str(&format!("BAVY OS v{} - {} up, {} hart(s)", version, format_uptime(uptime), harts));
            out_line("");
        } else {
            out_line("\x1b[1;36m═══════════════════════════════════════════════════════════════════\x1b[0m");
            out_str(&format!("\x1b[1;97m BAVY OS v{}\x1b[0m - {} up, \x1b[1;32m{}\x1b[0m hart(s)", version, format_uptime(uptime), harts));
            out_line("");
        }

        // Memory bar
        let (used, free) = allocator::heap_stats();
        let total = used + free;
        let pct = if total > 0 { (used * 100) / total } else { 0 };
        let bar_width = 30;
        let filled = (pct * bar_width) / 100;

        out_str("Mem: [");
        for j in 0..bar_width {
            if j < filled {
                if pct > 80 {
                    out_str("\x1b[1;31m█\x1b[0m");
                } else if pct > 60 {
                    out_str("\x1b[1;33m█\x1b[0m");
                } else {
                    out_str("\x1b[1;32m█\x1b[0m");
                }
            } else {
                out_str("\x1b[0;90m░\x1b[0m");
            }
        }
        out_str(&format!("] {}% ({}/{} KB)", pct, used / 1024, total / 1024));
        out_line("");

        // Tasks
        let mut tasks = scheduler::SCHEDULER.list_tasks();
        out_str(&format!("Tasks: \x1b[1m{}\x1b[0m total", tasks.len()));
        let running = tasks.iter().filter(|t| t.state.as_str() == "R+").count();
        let sleeping = tasks.iter().filter(|t| t.state.as_str() == "S").count();
        out_str(&format!(", \x1b[1;32m{}\x1b[0m running, \x1b[1;33m{}\x1b[0m sleeping", running, sleeping));
        out_line("");

        out_line("");
        out_line("\x1b[1;7m  PID  STATE  PRI     CPU    UPTIME  NAME                        \x1b[0m");

        // Sort by CPU time
        tasks.sort_by(|a, b| b.cpu_time.cmp(&a.cpu_time));

        for task in &tasks {
            let color = match task.state.as_str() {
                "R+" => "\x1b[1;32m",
                "S" => "\x1b[33m",
                "Z" => "\x1b[1;31m",
                _ => "",
            };
            out_str(color);
            out_str(&format!(
                "{:>5}  {:<6} {:<6} {:>6}ms {:>8}  {}",
                task.pid,
                task.state.as_str(),
                task.priority.as_str(),
                task.cpu_time,
                format_uptime(task.uptime as i64),
                task.name
            ));
            out_line("\x1b[0m");
        }

        out_line("");
        out_line("\x1b[1;36m─────────────────────────────────────────────────────────────────\x1b[0m");

        if iterations > 1 && iter_num < iterations - 1 {
            // Sleep between iterations (1 second)
            let start = get_time_ms();
            while get_time_ms() - start < 1000 {
                core::hint::spin_loop();
            }
            if !batch_mode {
                out_str("\x1b[2J\x1b[H");
            }
        }
    }
}

/// write - Write content to a file (native implementation)
fn native_write(args: &str) {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();

    if parts.len() < 2 {
        out_line("Usage: write <filename> <content...>");
        out_line("Example: write test.txt Hello World!");
        return;
    }

    let path_arg = parts[0];
    let content = parts[1];

    // Resolve path
    let filename = if path_arg.starts_with('/') {
        String::from(path_arg)
    } else {
        let cwd = cwd_get();
        if cwd == "/" {
            format!("/{}", path_arg)
        } else {
            format!("{}/{}", cwd, path_arg)
        }
    };

    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        match fs.write_file(dev, &filename, content.as_bytes()) {
            Ok(()) => {
                out_str("\x1b[1;32m✓\x1b[0m Written to ");
                out_line(&filename);
            }
            Err(_) => {
                out_str("\x1b[1;31mError:\x1b[0m Failed to write to ");
                out_line(&filename);
            }
        }
    } else {
        out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
    }
}

pub fn node(args: &[u8]) {
    let args_str = core::str::from_utf8(args).unwrap_or("").trim();

    if args_str.is_empty() || args_str == "info" {
        scripting::print_info();
    } else if args_str.starts_with("log ") {
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
        let expr = args_str.strip_prefix("eval").unwrap_or("").trim();
        if expr.is_empty() {
            out_line("Usage: node eval <expression>");
            out_line("Example: node eval 2 + 2 * 3");
            return;
        }
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
        let (script_name, script_args) = match args_str.split_once(' ') {
            Some((name, rest)) => (name, rest),
            None => (args_str, ""),
        };

        let resolved_path = if script_name.starts_with('/') {
            String::from(script_name)
        } else {
            resolve_path(script_name)
        };

        let script_result = {
            let fs_guard = FS_STATE.lock();
            let mut blk_guard = BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
                fs.read_file(dev, &resolved_path)
            } else {
                out_line("\x1b[1;31mError:\x1b[0m Filesystem not available");
                return;
            }
        };

        match script_result {
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
    }
}

pub fn help() {
    out_line("\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    out_line(
        "\x1b[1;36m│\x1b[0m                   \x1b[1;97mBAVY OS Commands\x1b[0m                        \x1b[1;36m│\x1b[0m",
    );
    out_line("\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m");
    out_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mBuilt-in:\x1b[0m                                                 \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    cd <dir>        Change directory                         \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    pwd             Print working directory                  \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    clear           Clear the screen                         \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    shutdown        Power off the system                     \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    ping <host>     Ping host (Ctrl+C to stop)               \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    nslookup <host> DNS lookup                               \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    node [info]     Scripting engine info/control            \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mUser Scripts:\x1b[0m  \x1b[0;90m(in /usr/bin/ - Rhai language)\x1b[0m            \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    help, ls, cat, echo, cowsay, sysinfo, ip, memstats, ...  \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mKernel API:\x1b[0m  \x1b[0;90m(available in scripts)\x1b[0m                      \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    ls(), read_file(), write_file(), file_exists()           \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    get_ip(), get_mac(), get_gateway(), net_available()      \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    time_ms(), sleep(ms), kernel_version(), arch()           \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m    heap_total(), heap_used(), heap_free()                   \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mRedirection:\x1b[0m  cmd > file | cmd >> file                    \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    out_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;32mTip:\x1b[0m  \x1b[1;97mCtrl+C\x1b[0m cancel  |  \x1b[1;97m↑/↓\x1b[0m history  |  \x1b[1;97mnode info\x1b[0m API  \x1b[1;36m│\x1b[0m",
    );
    out_line("\x1b[1;36m└─────────────────────────────────────────────────────────────┘\x1b[0m");
}

pub fn alloc(args: &[u8]) {
    let n = parse_usize(args);
    if n > 0 {
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

pub fn readsec(args: &[u8]) {
    let sector = parse_usize(args) as u64;
    let mut blk_guard = BLK_DEV.lock();
    if let Some(ref mut blk) = *blk_guard {
        let mut buf = [0u8; 512];
        if blk.read_sector(sector, &mut buf).is_ok() {
            uart::write_line("Sector contents (first 64 bytes):");
            for i in 0..64 {
                uart::write_hex_byte(buf[i]);
                if (i + 1) % 16 == 0 {
                    uart::write_line("");
                } else {
                    uart::write_str(" ");
                }
            }
        } else {
            uart::write_line("Read failed.");
        }
    } else {
        uart::write_line("No block device.");
    }
}

pub fn memtest(args: &[u8]) {
    let iterations = {
        let n = parse_usize(args);
        if n == 0 {
            10
        } else {
            n
        }
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
        let size = 1024;
        let pattern = ((i % 256) as u8).wrapping_add(0x42);

        let mut v: Vec<u8> = Vec::with_capacity(size);
        v.resize(size, pattern);

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

    if used_after <= used_before + 64 {
        uart::write_line("Memory deallocation: OK (memory reclaimed)");
    } else {
        uart::write_line("WARNING: Memory may not be properly reclaimed!");
        uart::write_str("  Leaked approximately ");
        uart::write_u64((used_after - used_before) as u64);
        uart::write_line(" bytes");
    }
}

pub fn cputest(args: &[u8]) {
    let limit = {
        let n = parse_usize(args);
        if n == 0 {
            100_000
        } else {
            n
        }
    };

    let num_harts = HARTS_ONLINE.load(Ordering::Relaxed);

    uart::write_line("");
    uart::write_line(
        "\x1b[1;36m╔═══════════════════════════════════════════════════════════════════════╗\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;36m║\x1b[0m                      \x1b[1;97mCPU BENCHMARK - Prime Counting\x1b[0m                  \x1b[1;36m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;36m╚═══════════════════════════════════════════════════════════════════════╝\x1b[0m",
    );
    uart::write_line("");

    uart::write_str("  \x1b[1;33mConfiguration:\x1b[0m");
    uart::write_line("");
    uart::write_str("    Range: 2 to ");
    uart::write_u64(limit as u64);
    uart::write_line("");
    uart::write_str("    Harts online: ");
    uart::write_u64(num_harts as u64);
    uart::write_line("");
    uart::write_line("");

    uart::write_line("  \x1b[1;33m[1/2] Serial Execution\x1b[0m (single hart)");
    uart::write_str("        Computing primes...");

    let serial_start = get_time_ms();
    let serial_count = count_primes_in_range(2, limit as u64);
    let serial_end = get_time_ms();
    let serial_time = serial_end - serial_start;

    uart::write_line(" done!");
    uart::write_str("        Result: \x1b[1;97m");
    uart::write_u64(serial_count);
    uart::write_str("\x1b[0m primes found in \x1b[1;97m");
    uart::write_u64(serial_time as u64);
    uart::write_line("\x1b[0m ms");
    uart::write_line("");

    if num_harts > 1 {
        uart::write_str("  \x1b[1;33m[2/2] Parallel Execution\x1b[0m (");
        uart::write_u64(num_harts as u64);
        uart::write_line(" harts)");
        uart::write_str("        Computing primes...");

        let parallel_start = get_time_ms();

        BENCHMARK.start(BenchmarkMode::PrimeCount, 2, limit as u64, num_harts);

        for hart in 1..num_harts {
            send_ipi(hart);
        }

        let (my_start, my_end) = BENCHMARK.get_work_range(0);
        let my_count = count_primes_in_range(my_start, my_end);
        BENCHMARK.report_result(0, my_count);

        let timeout = get_time_ms() + 60000;
        while !BENCHMARK.all_completed() {
            if get_time_ms() > timeout {
                uart::write_line(" TIMEOUT!");
                uart::write_line(
                    "        \x1b[1;31mError:\x1b[0m Some harts did not complete in time",
                );
                BENCHMARK.clear();
                return;
            }
            core::hint::spin_loop();
        }

        let parallel_end = get_time_ms();
        let parallel_time = parallel_end - parallel_start;
        let parallel_count = BENCHMARK.total_result();

        BENCHMARK.clear();

        uart::write_line(" done!");
        uart::write_str("        Result: \x1b[1;97m");
        uart::write_u64(parallel_count);
        uart::write_str("\x1b[0m primes found in \x1b[1;97m");
        uart::write_u64(parallel_time as u64);
        uart::write_line("\x1b[0m ms");

        uart::write_line("");
        uart::write_line("        \x1b[0;90mWork distribution:\x1b[0m");
        let chunk = (limit as u64 - 2) / num_harts as u64;
        for hart in 0..num_harts {
            let h_start = 2 + hart as u64 * chunk;
            let h_end = if hart == num_harts - 1 {
                limit as u64
            } else {
                h_start + chunk
            };
            uart::write_str("          Hart ");
            uart::write_u64(hart as u64);
            uart::write_str(": [");
            uart::write_u64(h_start);
            uart::write_str(", ");
            uart::write_u64(h_end);
            uart::write_line(")");
        }
        uart::write_line("");

        uart::write_line(
            "\x1b[1;36m────────────────────────────────────────────────────────────────────────\x1b[0m",
        );
        uart::write_line("  \x1b[1;33mResults Summary:\x1b[0m");
        uart::write_line("");

        if serial_count == parallel_count {
            uart::write_line("    \x1b[1;32m✓\x1b[0m Results match (verified correctness)");
        } else {
            uart::write_line("    \x1b[1;31m✗\x1b[0m Results MISMATCH (bug detected!)");
            uart::write_str("      Serial: ");
            uart::write_u64(serial_count);
            uart::write_str(", Parallel: ");
            uart::write_u64(parallel_count);
            uart::write_line("");
        }
        uart::write_line("");

        if parallel_time > 0 {
            let speedup_x10 = (serial_time * 10) / parallel_time;
            let speedup_whole = speedup_x10 / 10;
            let speedup_frac = speedup_x10 % 10;

            uart::write_str("    Serial time:   \x1b[1;97m");
            uart::write_u64(serial_time as u64);
            uart::write_line(" ms\x1b[0m");
            uart::write_str("    Parallel time: \x1b[1;97m");
            uart::write_u64(parallel_time as u64);
            uart::write_line(" ms\x1b[0m");
            uart::write_str("    Speedup:       \x1b[1;32m");
            uart::write_u64(speedup_whole as u64);
            uart::write_str(".");
            uart::write_u64(speedup_frac as u64);
            uart::write_str("x\x1b[0m (with ");
            uart::write_u64(num_harts as u64);
            uart::write_line(" harts)");

            let efficiency = (speedup_x10 * 100) / (num_harts as i64 * 10);
            uart::write_str("    Efficiency:    \x1b[1;97m");
            uart::write_u64(efficiency as u64);
            uart::write_line("%\x1b[0m (speedup / num_harts × 100)");
        }
        uart::write_line("");
    } else {
        uart::write_line("  \x1b[1;33m[2/2] Parallel Execution\x1b[0m");
        uart::write_line("        \x1b[0;90mSkipped - only 1 hart online\x1b[0m");
        uart::write_line("");
        uart::write_line(
            "\x1b[1;36m────────────────────────────────────────────────────────────────────────\x1b[0m",
        );
        uart::write_line("  \x1b[1;33mResults Summary:\x1b[0m");
        uart::write_line("");
        uart::write_str("    Serial time: \x1b[1;97m");
        uart::write_u64(serial_time as u64);
        uart::write_line(" ms\x1b[0m");
        uart::write_str("    Primes found: \x1b[1;97m");
        uart::write_u64(serial_count);
        uart::write_line("\x1b[0m");
        uart::write_line("");
        uart::write_line("    \x1b[0;90mNote: Enable more harts to see parallel comparison\x1b[0m");
        uart::write_line("");
    }

    uart::write_line(
        "\x1b[1;36m════════════════════════════════════════════════════════════════════════\x1b[0m",
    );
    uart::write_line("");
}

pub fn ping(args: &[u8]) {
    if args.is_empty() {
        uart::write_line("Usage: ping <ip|hostname>");
        uart::write_line("\x1b[0;90mExamples:\x1b[0m");
        uart::write_line("  ping 10.0.2.2");
        uart::write_line("  ping google.com");
        uart::write_line("\x1b[0;90mPress Ctrl+C to stop\x1b[0m");
        return;
    }

    let mut arg_len = args.len();
    while arg_len > 0 && (args[arg_len - 1] == b' ' || args[arg_len - 1] == b'\t') {
        arg_len -= 1;
    }
    let trimmed_args = &args[..arg_len];

    let target = match net::parse_ipv4(trimmed_args) {
        Some(ip) => ip,
        None => {
            uart::write_str("\x1b[0;90m[DNS]\x1b[0m Resolving ");
            uart::write_bytes(trimmed_args);
            uart::write_line("...");

            let resolve_result = {
                let mut net_guard = NET_STATE.lock();
                if let Some(ref mut state) = *net_guard {
                    dns::resolve(state, trimmed_args, net::DNS_SERVER, 5000, get_time_ms)
                } else {
                    uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
                    return;
                }
            };

            match resolve_result {
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
        }
    };

    let timestamp = get_time_ms();

    let mut ip_buf = [0u8; 16];
    let ip_len = net::format_ipv4(target, &mut ip_buf);
    uart::write_str("PING ");
    uart::write_bytes(&ip_buf[..ip_len]);
    uart::write_line(" 56(84) bytes of data.");

    let mut ping_state = PingState::new(target, timestamp);
    ping_state.seq = 1;
    ping_state.sent_time = timestamp;
    ping_state.last_send_time = timestamp;
    ping_state.packets_sent = 1;
    ping_state.waiting = true;

    let send_result = {
        let mut net_guard = NET_STATE.lock();
        if let Some(ref mut state) = *net_guard {
            state.send_ping(target, ping_state.seq, timestamp)
        } else {
            uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
            return;
        }
    };

    match send_result {
        Ok(()) => {
            *PING_STATE.lock() = Some(ping_state);
            *COMMAND_RUNNING.lock() = true;
        }
        Err(e) => {
            uart::write_str("ping: ");
            uart::write_line(e);
        }
    }
}

pub fn nslookup(args: &[u8]) {
    if args.is_empty() {
        uart::write_line("Usage: nslookup <hostname>");
        uart::write_line("\x1b[0;90mExample: nslookup google.com\x1b[0m");
        return;
    }

    let mut hostname_len = args.len();
    while hostname_len > 0 && (args[hostname_len - 1] == b' ' || args[hostname_len - 1] == b'\t') {
        hostname_len -= 1;
    }
    let hostname = &args[..hostname_len];

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

    let resolve_result = {
        let mut net_guard = NET_STATE.lock();
        if let Some(ref mut state) = *net_guard {
            dns::resolve(state, hostname, net::DNS_SERVER, 5000, get_time_ms)
        } else {
            uart::write_line("\x1b[1;31m✗\x1b[0m Network not initialized");
            return;
        }
    };

    match resolve_result {
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
}

pub fn cd(args: &str) {
    let path = args.trim();

    if path.is_empty() || path == "~" {
        cwd_set("/");
        return;
    }

    if path == "-" {
        out_line("cd: OLDPWD not set");
        return;
    }

    let new_path = resolve_path(path);

    if path_exists(&new_path) {
        cwd_set(&new_path);
    } else {
        out_str("\x1b[1;31mcd:\x1b[0m ");
        out_str(path);
        out_line(": No such directory");
    }
}

pub fn shutdown() {
    uart::write_line("");
    uart::write_line(
        "\x1b[1;31m╔═══════════════════════════════════════════════════════════════════╗\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                    \x1b[1;97mSystem Shutdown Initiated\x1b[0m                       \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m║\x1b[0m                                                                   \x1b[1;31m║\x1b[0m",
    );
    uart::write_line(
        "\x1b[1;31m╚═══════════════════════════════════════════════════════════════════╝\x1b[0m",
    );
    uart::write_line("");
    uart::write_line("    \x1b[0;90m[1/3]\x1b[0m Syncing filesystems...");
    uart::write_line("    \x1b[0;90m[2/3]\x1b[0m Stopping network services...");
    uart::write_line("    \x1b[0;90m[3/3]\x1b[0m Powering off CPU...");
    uart::write_line("");
    uart::write_line("    \x1b[1;32m✓ Goodbye!\x1b[0m");
    uart::write_line("");

    unsafe {
        ptr::write_volatile(TEST_FINISHER as *mut u32, 0x5555);
    }
    loop {}
}

fn parse_usize(args: &[u8]) -> usize {
    let mut n: usize = 0;
    let mut ok = false;
    for &b in args {
        if (b'0'..=b'9').contains(&b) {
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
    if ok {
        n
    } else {
        0
    }
}

fn path_exists(path: &str) -> bool {
    let mut fs_guard = FS_STATE.lock();
    let mut blk_guard = BLK_DEV.lock();
    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
        if path == "/" {
            return true;
        }

        let files = fs.list_dir(dev, "/");
        let path_with_slash = if path.ends_with('/') {
            String::from(path)
        } else {
            let mut s = String::from(path);
            s.push('/');
            s
        };

        for file in files {
            if file.name.starts_with(&path_with_slash) {
                return true;
            }
            if file.name == path {
                return true;
            }
        }
    }
    false
}
