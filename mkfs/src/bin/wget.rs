// wget - Download files from the web

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

// Use the lib's panic handler
#[cfg(target_arch = "wasm32")]
extern crate mkfs;

#[cfg(target_arch = "wasm32")]
mod wasm {
    // Direct syscalls to minimize code size
    extern "C" {
        fn print(ptr: *const u8, len: usize);
        fn arg_count() -> i32;
        fn arg_get(index: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn cwd_get(buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn http_get(url_ptr: *const u8, url_len: i32, resp_ptr: *mut u8, resp_len: i32) -> i32;
        fn fs_write(path_ptr: *const u8, path_len: i32, data_ptr: *const u8, data_len: i32) -> i32;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        if argc < 1 {
            log("Usage: wget <url> [-O file]\n");
            return;
        }

        // Get URL (first arg)
        let mut url_buf = [0u8; 256];
        let url_len = unsafe { arg_get(0, url_buf.as_mut_ptr(), 256) };
        if url_len <= 0 {
            log("Error: Invalid URL\n");
            return;
        }

        // Check for -O option (args: url -O path)
        let mut rel_path_buf = [0u8; 128];
        let mut rel_path_len: i32 = 0;
        
        let mut i = 1;
        while i < argc {
            let mut opt_buf = [0u8; 8];
            let opt_len = unsafe { arg_get(i, opt_buf.as_mut_ptr(), 8) };
            if opt_len == 2 && opt_buf[0] == b'-' && opt_buf[1] == b'O' {
                // Next arg is the output path
                if i + 1 < argc {
                    rel_path_len = unsafe { arg_get(i + 1, rel_path_buf.as_mut_ptr(), 128) };
                }
                break;
            }
            i += 1;
        }

        log("Fetching: ");
        unsafe { print(url_buf.as_ptr(), url_len as usize) };
        log("\n");

        // Make HTTP request
        let mut resp_buf = [0u8; 16384];
        let resp_len = unsafe { http_get(url_buf.as_ptr(), url_len, resp_buf.as_mut_ptr(), 16384) };
        
        if resp_len < 0 {
            log("Error: Request failed\n");
            return;
        }

        log("Received ");
        print_num(resp_len as i64);
        log(" bytes\n");

        if rel_path_len > 0 {
            // Build absolute path
            let mut abs_path_buf = [0u8; 256];
            let abs_path_len: i32;
            
            // Check if path is already absolute
            if rel_path_buf[0] == b'/' {
                // Already absolute, just copy
                let len = (rel_path_len as usize).min(256);
                abs_path_buf[..len].copy_from_slice(&rel_path_buf[..len]);
                abs_path_len = len as i32;
            } else {
                // Relative path - prepend CWD
                let mut cwd_buf = [0u8; 128];
                let cwd_len = unsafe { cwd_get(cwd_buf.as_mut_ptr(), 128) };
                
                if cwd_len <= 0 {
                    // Default to root
                    abs_path_buf[0] = b'/';
                    let len = (rel_path_len as usize).min(255);
                    abs_path_buf[1..1+len].copy_from_slice(&rel_path_buf[..len]);
                    abs_path_len = (1 + len) as i32;
                } else {
                    // Combine CWD + "/" + relative path
                    let cwd_ulen = cwd_len as usize;
                    abs_path_buf[..cwd_ulen].copy_from_slice(&cwd_buf[..cwd_ulen]);
                    
                    let mut idx = cwd_ulen;
                    // Add slash if CWD doesn't end with one
                    if idx > 0 && abs_path_buf[idx - 1] != b'/' {
                        abs_path_buf[idx] = b'/';
                        idx += 1;
                    }
                    
                    // Append relative path
                    let rel_ulen = rel_path_len as usize;
                    let remaining = 256 - idx;
                    let to_copy = rel_ulen.min(remaining);
                    abs_path_buf[idx..idx+to_copy].copy_from_slice(&rel_path_buf[..to_copy]);
                    abs_path_len = (idx + to_copy) as i32;
                }
            }
            
            log("Saving to: ");
            unsafe { print(abs_path_buf.as_ptr(), abs_path_len as usize) };
            log("\n");
            
            let written = unsafe { 
                fs_write(abs_path_buf.as_ptr(), abs_path_len, resp_buf.as_ptr(), resp_len) 
            };
            if written >= 0 {
                log("OK: Wrote ");
                print_num(written as i64);
                log(" bytes\n");
            } else {
                log("Error: Write failed\n");
            }
        } else {
            // Print to stdout
            unsafe { print(resp_buf.as_ptr(), resp_len as usize) };
            if resp_len > 0 && resp_buf[(resp_len - 1) as usize] != b'\n' {
                log("\n");
            }
        }
    }

    fn print_num(mut n: i64) {
        if n < 0 {
            log("-");
            n = -n;
        }
        if n == 0 {
            log("0");
            return;
        }
        let mut buf = [0u8; 20];
        let mut i = 19usize;
        while n > 0 {
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if i == 0 { break; }
            i -= 1;
        }
        unsafe { print(buf.as_ptr().add(i + 1), 19 - i) };
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
