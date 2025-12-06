// pkg - Simple package manager
//
// Usage:
//   pkg list              List installed packages
//   pkg install <url>     Install package from URL
//   pkg help              Show help

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

// Use the lib's panic handler
#[cfg(target_arch = "wasm32")]
extern crate mkfs;

#[cfg(target_arch = "wasm32")]
mod wasm {
    extern "C" {
        fn print(ptr: *const u8, len: usize);
        fn arg_count() -> i32;
        fn arg_get(index: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn fs_list(buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn fs_read(path_ptr: *const u8, path_len: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn fs_write(path_ptr: *const u8, path_len: i32, data_ptr: *const u8, data_len: i32) -> i32;
        fn http_get(url_ptr: *const u8, url_len: i32, resp_ptr: *mut u8, resp_len: i32) -> i32;
        fn net_available() -> i32;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_num(mut n: i64) {
        if n == 0 {
            log("0");
            return;
        }
        let mut buf = [0u8; 20];
        let mut i = 19usize;
        while n > 0 && i > 0 {
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i -= 1;
        }
        unsafe { print(buf.as_ptr().add(i + 1), 19 - i) };
    }

    fn print_help() {
        log("\x1b[1;36mpkg\x1b[0m - BAVY Package Manager\n\n");
        log("\x1b[1mUSAGE:\x1b[0m\n");
        log("    pkg <command> [args]\n\n");
        log("\x1b[1mCOMMANDS:\x1b[0m\n");
        log("    list              List installed packages in /usr/bin\n");
        log("    install <url>     Download and install a WASM package\n");
        log("    info <name>       Show package info\n");
        log("    help              Show this help message\n\n");
        log("\x1b[1mEXAMPLES:\x1b[0m\n");
        log("    pkg list\n");
        log("    pkg install https://example.com/app.wasm\n");
        log("    pkg info cowsay\n");
    }

    fn cmd_list() {
        log("\x1b[1;36mInstalled Packages\x1b[0m\n");
        log("\x1b[90m─────────────────────────────────────\x1b[0m\n\n");
        
        // Read directory listing from /usr/bin
        // The fs_list syscall uses current directory, so we need to handle this
        // We'll read the directory contents
        let mut buf = [0u8; 4096];
        let len = unsafe { fs_list(buf.as_mut_ptr(), buf.len() as i32) };
        
        if len <= 0 {
            // Fallback: show known packages
            log("\x1b[33mNote: Directory listing not available.\x1b[0m\n");
            log("\x1b[33mKnown system packages:\x1b[0m\n\n");
            log("  \x1b[32m●\x1b[0m cowsay     ASCII art cow\n");
            log("  \x1b[32m●\x1b[0m dmesg      Kernel log viewer\n");
            log("  \x1b[32m●\x1b[0m hello      Test WASM binary\n");
            log("  \x1b[32m●\x1b[0m help       Show available commands\n");
            log("  \x1b[32m●\x1b[0m nano       Text file viewer\n");
            log("  \x1b[32m●\x1b[0m pkg        Package manager (this)\n");
            log("  \x1b[32m●\x1b[0m wget       Download files\n");
            log("\n\x1b[90mRun 'ls /usr/bin' to see all installed binaries.\x1b[0m\n");
            return;
        }
        
        // Parse and display the file list
        // Format is typically newline-separated filenames
        let content = &buf[..len as usize];
        let mut count = 0;
        let mut start = 0;
        
        for i in 0..content.len() {
            if content[i] == b'\n' || i == content.len() - 1 {
                let end = if content[i] == b'\n' { i } else { i + 1 };
                if end > start {
                    log("  \x1b[32m●\x1b[0m ");
                    unsafe { print(content.as_ptr().add(start), end - start) };
                    log("\n");
                    count += 1;
                }
                start = i + 1;
            }
        }
        
        log("\n\x1b[90m");
        print_num(count as i64);
        log(" package(s) installed\x1b[0m\n");
    }

    fn cmd_install(url: &[u8]) {
        // Check network
        if unsafe { net_available() } != 1 {
            log("\x1b[31mError: Network not available\x1b[0m\n");
            return;
        }
        
        // Extract filename from URL
        let mut name_start = 0;
        for i in (0..url.len()).rev() {
            if url[i] == b'/' {
                name_start = i + 1;
                break;
            }
        }
        
        if name_start >= url.len() {
            log("\x1b[31mError: Could not determine package name from URL\x1b[0m\n");
            return;
        }
        
        let name = &url[name_start..];
        
        // Remove .wasm extension if present for display
        let display_len = if name.len() > 5 && &name[name.len()-5..] == b".wasm" {
            name.len() - 5
        } else {
            name.len()
        };
        
        log("\x1b[1;36mInstalling package:\x1b[0m ");
        unsafe { print(name.as_ptr(), display_len) };
        log("\n\n");
        
        log("  \x1b[90m→\x1b[0m Downloading... ");
        
        // Fetch the package
        let mut resp_buf = [0u8; 65536]; // 64KB max package size
        let resp_len = unsafe { 
            http_get(url.as_ptr(), url.len() as i32, resp_buf.as_mut_ptr(), resp_buf.len() as i32) 
        };
        
        if resp_len < 0 {
            log("\x1b[31mFailed\x1b[0m\n");
            log("\x1b[31mError: Download failed\x1b[0m\n");
            return;
        }
        
        log("\x1b[32mOK\x1b[0m (");
        print_num(resp_len as i64);
        log(" bytes)\n");
        
        // Build destination path: /usr/bin/<name>
        let mut dest_path = [0u8; 256];
        let prefix = b"/usr/bin/";
        dest_path[..prefix.len()].copy_from_slice(prefix);
        
        // Use name without .wasm extension
        let dest_name_len = display_len.min(256 - prefix.len());
        dest_path[prefix.len()..prefix.len() + dest_name_len].copy_from_slice(&name[..dest_name_len]);
        let dest_len = prefix.len() + dest_name_len;
        
        log("  \x1b[90m→\x1b[0m Installing to ");
        unsafe { print(dest_path.as_ptr(), dest_len) };
        log("... ");
        
        // Write the file
        let written = unsafe {
            fs_write(dest_path.as_ptr(), dest_len as i32, resp_buf.as_ptr(), resp_len)
        };
        
        if written < 0 {
            log("\x1b[31mFailed\x1b[0m\n");
            log("\x1b[31mError: Could not write to /usr/bin/\x1b[0m\n");
            return;
        }
        
        log("\x1b[32mOK\x1b[0m\n\n");
        log("\x1b[32m✓ Package installed successfully!\x1b[0m\n");
        log("  Run '\x1b[1m");
        unsafe { print(name.as_ptr(), display_len) };
        log("\x1b[0m' to use it.\n");
    }

    fn cmd_info(name: &[u8]) {
        // Build path to /usr/bin/<name>
        let mut path = [0u8; 256];
        let prefix = b"/usr/bin/";
        path[..prefix.len()].copy_from_slice(prefix);
        let name_len = name.len().min(256 - prefix.len());
        path[prefix.len()..prefix.len() + name_len].copy_from_slice(&name[..name_len]);
        let path_len = prefix.len() + name_len;
        
        // Try to read the file to get its size
        let mut buf = [0u8; 1]; // Just check if it exists
        let len = unsafe {
            fs_read(path.as_ptr(), path_len as i32, buf.as_mut_ptr(), 65536)
        };
        
        log("\x1b[1;36mPackage Info:\x1b[0m ");
        unsafe { print(name.as_ptr(), name.len()) };
        log("\n");
        log("\x1b[90m─────────────────────────────────────\x1b[0m\n");
        
        if len < 0 {
            log("\x1b[31mPackage not found\x1b[0m\n");
            return;
        }
        
        log("  Location: ");
        unsafe { print(path.as_ptr(), path_len) };
        log("\n");
        log("  Size:     ");
        print_num(len as i64);
        log(" bytes\n");
        log("  Type:     WASM binary\n");
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        if argc < 1 {
            print_help();
            return;
        }
        
        // Get command
        let mut cmd_buf = [0u8; 32];
        let cmd_len = unsafe { arg_get(0, cmd_buf.as_mut_ptr(), 32) };
        
        if cmd_len <= 0 {
            print_help();
            return;
        }
        
        let cmd = &cmd_buf[..cmd_len as usize];
        
        match cmd {
            b"list" | b"ls" => cmd_list(),
            b"install" | b"i" => {
                if argc < 2 {
                    log("\x1b[31mError: Missing URL argument\x1b[0m\n");
                    log("Usage: pkg install <url>\n");
                    return;
                }
                let mut url_buf = [0u8; 512];
                let url_len = unsafe { arg_get(1, url_buf.as_mut_ptr(), 512) };
                if url_len > 0 {
                    cmd_install(&url_buf[..url_len as usize]);
                }
            }
            b"info" => {
                if argc < 2 {
                    log("\x1b[31mError: Missing package name\x1b[0m\n");
                    log("Usage: pkg info <name>\n");
                    return;
                }
                let mut name_buf = [0u8; 64];
                let name_len = unsafe { arg_get(1, name_buf.as_mut_ptr(), 64) };
                if name_len > 0 {
                    cmd_info(&name_buf[..name_len as usize]);
                }
            }
            b"help" | b"-h" | b"--help" => print_help(),
            _ => {
                log("\x1b[31mUnknown command: \x1b[0m");
                unsafe { print(cmd.as_ptr(), cmd.len()) };
                log("\n\nRun 'pkg help' for usage.\n");
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
