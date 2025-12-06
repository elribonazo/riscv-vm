// dmesg - Display kernel ring buffer
//
// Usage:
//   dmesg           Show all kernel log messages
//   dmesg -n <N>    Show last N messages
//   dmesg -h        Show help

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

// Use the lib's panic handler
#[cfg(target_arch = "wasm32")]
extern crate mkfs;

#[cfg(target_arch = "wasm32")]
mod wasm {
    // Direct syscalls for minimal code size
    extern "C" {
        fn print(ptr: *const u8, len: usize);
        fn arg_count() -> i32;
        fn arg_get(index: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn klog_get(count: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_help() {
        log("\x1b[1mdmesg\x1b[0m - Display kernel ring buffer\n\n");
        log("\x1b[1mUSAGE:\x1b[0m\n");
        log("    dmesg [OPTIONS]\n\n");
        log("\x1b[1mOPTIONS:\x1b[0m\n");
        log("    -n <N>      Show last N messages (default: 100)\n");
        log("    -h, --help  Show this help message\n\n");
        log("\x1b[1mEXAMPLES:\x1b[0m\n");
        log("    dmesg           Show all kernel messages\n");
        log("    dmesg -n 10     Show last 10 messages\n");
    }

    fn parse_int(s: &[u8]) -> Option<i32> {
        if s.is_empty() {
            return None;
        }
        let mut result: i32 = 0;
        for &c in s {
            if c < b'0' || c > b'9' {
                return None;
            }
            result = result.checked_mul(10)?.checked_add((c - b'0') as i32)?;
        }
        Some(result)
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        // Default: show up to 100 messages (kernel limit)
        let mut count: i32 = 100;
        
        // Parse arguments
        let mut i = 0;
        while i < argc {
            let mut arg_buf = [0u8; 32];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 32) };
            
            if arg_len <= 0 {
                i += 1;
                continue;
            }
            
            let arg = &arg_buf[..arg_len as usize];
            
            // Check for help flag
            if arg == b"-h" || arg == b"--help" {
                print_help();
                return;
            }
            
            // Check for -n option
            if arg == b"-n" {
                // Next argument should be the count
                if i + 1 < argc {
                    let mut num_buf = [0u8; 16];
                    let num_len = unsafe { arg_get(i + 1, num_buf.as_mut_ptr(), 16) };
                    if num_len > 0 {
                        if let Some(n) = parse_int(&num_buf[..num_len as usize]) {
                            count = n.max(1).min(100);
                            i += 1; // Skip the number argument
                        } else {
                            log("\x1b[31mError: Invalid count for -n\x1b[0m\n");
                            return;
                        }
                    }
                } else {
                    log("\x1b[31mError: -n requires a number argument\x1b[0m\n");
                    return;
                }
            }
            
            i += 1;
        }
        
        // Fetch kernel log entries
        // Buffer size: ~400 bytes per colored log line, 100 max entries
        let mut buf = [0u8; 40960];
        let len = unsafe { klog_get(count, buf.as_mut_ptr(), buf.len() as i32) };
        
        if len < 0 {
            log("\x1b[31mError: Failed to read kernel log\x1b[0m\n");
            return;
        }
        
        if len == 0 {
            log("\x1b[90m(No kernel log entries)\x1b[0m\n");
            return;
        }
        
        // Print the log entries
        unsafe { print(buf.as_ptr(), len as usize) };
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
