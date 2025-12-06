// nano - Text file viewer
//
// Usage:
//   nano <filename>     View file contents with line numbers
//   nano -h             Show help

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
        fn cwd_get(buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn fs_read(path_ptr: *const u8, path_len: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn fs_exists(path_ptr: *const u8, path_len: i32) -> i32;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_help() {
        log("\x1b[1mnano\x1b[0m - Text file viewer (BAVY Edition)\n\n");
        log("\x1b[1mUSAGE:\x1b[0m\n");
        log("    nano <filename>\n\n");
        log("\x1b[1mOPTIONS:\x1b[0m\n");
        log("    -h, --help  Show this help message\n\n");
        log("\x1b[1mEXAMPLES:\x1b[0m\n");
        log("    nano /etc/init.d/startup\n");
        log("    nano README.md\n\n");
        log("\x1b[90mNote: This is a read-only viewer. Use echo > file to write.\x1b[0m\n");
    }

    fn print_num(mut n: i32) {
        if n == 0 {
            log("0");
            return;
        }
        let mut buf = [0u8; 10];
        let mut i = 9usize;
        while n > 0 && i > 0 {
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i -= 1;
        }
        unsafe { print(buf.as_ptr().add(i + 1), 9 - i) };
    }

    fn print_num_padded(n: i32, width: usize) {
        // Count digits
        let mut digits = 0;
        let mut tmp = n;
        if tmp == 0 {
            digits = 1;
        } else {
            while tmp > 0 {
                digits += 1;
                tmp /= 10;
            }
        }
        // Print padding spaces
        for _ in digits..width {
            log(" ");
        }
        print_num(n);
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        if argc < 1 {
            print_help();
            return;
        }
        
        // Get filename argument
        let mut arg_buf = [0u8; 256];
        let arg_len = unsafe { arg_get(0, arg_buf.as_mut_ptr(), 256) };
        
        if arg_len <= 0 {
            print_help();
            return;
        }
        
        let arg = &arg_buf[..arg_len as usize];
        
        // Check for help flag
        if arg == b"-h" || arg == b"--help" {
            print_help();
            return;
        }
        
        // Build absolute path if needed
        let mut path_buf = [0u8; 512];
        let path_len: i32;
        
        if arg[0] == b'/' {
            // Already absolute
            path_buf[..arg_len as usize].copy_from_slice(arg);
            path_len = arg_len;
        } else {
            // Prepend CWD
            let mut cwd_buf = [0u8; 256];
            let cwd_len = unsafe { cwd_get(cwd_buf.as_mut_ptr(), 256) };
            
            if cwd_len <= 0 {
                // Default to root
                path_buf[0] = b'/';
                path_buf[1..1 + arg_len as usize].copy_from_slice(arg);
                path_len = 1 + arg_len;
            } else {
                let cwd_ulen = cwd_len as usize;
                path_buf[..cwd_ulen].copy_from_slice(&cwd_buf[..cwd_ulen]);
                let mut idx = cwd_ulen;
                if idx > 0 && path_buf[idx - 1] != b'/' {
                    path_buf[idx] = b'/';
                    idx += 1;
                }
                let arg_ulen = arg_len as usize;
                path_buf[idx..idx + arg_ulen].copy_from_slice(arg);
                path_len = (idx + arg_ulen) as i32;
            }
        }
        
        // Check if file exists
        let exists = unsafe { fs_exists(path_buf.as_ptr(), path_len) };
        if exists != 1 {
            log("\x1b[31mError: File not found: \x1b[0m");
            unsafe { print(path_buf.as_ptr(), path_len as usize) };
            log("\n");
            return;
        }
        
        // Read file contents
        let mut content_buf = [0u8; 32768]; // 32KB max file size
        let content_len = unsafe { 
            fs_read(path_buf.as_ptr(), path_len, content_buf.as_mut_ptr(), content_buf.len() as i32) 
        };
        
        if content_len < 0 {
            log("\x1b[31mError: Failed to read file\x1b[0m\n");
            return;
        }
        
        // Print header
        log("\x1b[7m  File: ");
        unsafe { print(path_buf.as_ptr(), path_len as usize) };
        log(" \x1b[0m\n");
        log("\x1b[90m");
        for _ in 0..60 {
            log("─");
        }
        log("\x1b[0m\n");
        
        if content_len == 0 {
            log("\x1b[90m(empty file)\x1b[0m\n");
            return;
        }
        
        // Count lines for padding calculation
        let content = &content_buf[..content_len as usize];
        let mut line_count = 1;
        for &c in content {
            if c == b'\n' {
                line_count += 1;
            }
        }
        
        // Calculate width needed for line numbers
        let num_width = if line_count >= 1000 { 4 } else if line_count >= 100 { 3 } else { 2 };
        
        // Print content with line numbers
        let mut line_num = 1;
        let mut line_start = 0;
        
        for i in 0..content.len() {
            if content[i] == b'\n' || i == content.len() - 1 {
                let line_end = if content[i] == b'\n' { i } else { i + 1 };
                
                // Print line number
                log("\x1b[90m");
                print_num_padded(line_num, num_width);
                log(" │\x1b[0m ");
                
                // Print line content
                if line_end > line_start {
                    unsafe { print(content.as_ptr().add(line_start), line_end - line_start) };
                }
                log("\n");
                
                line_num += 1;
                line_start = i + 1;
            }
        }
        
        // Print footer
        log("\x1b[90m");
        for _ in 0..60 {
            log("─");
        }
        log("\x1b[0m\n");
        log("\x1b[90m");
        print_num(content_len);
        log(" bytes, ");
        print_num(line_count);
        log(" lines\x1b[0m\n");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
