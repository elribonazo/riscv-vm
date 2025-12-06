// cat - Display file contents
//
// Usage:
//   cat <file>       Display contents of a file
//   cat -n <file>    Display with line numbers

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

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
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_num(mut n: usize) {
        if n == 0 {
            log("0");
            return;
        }
        let mut buf = [0u8; 10];
        let mut i = buf.len();
        while n > 0 && i > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        unsafe { print(buf[i..].as_ptr(), buf.len() - i) };
    }

    fn resolve_path(arg: &[u8], out: &mut [u8]) -> usize {
        // Get current working directory
        let mut cwd = [0u8; 256];
        let cwd_len = unsafe { cwd_get(cwd.as_mut_ptr(), cwd.len() as i32) };
        
        if arg.starts_with(b"/") {
            // Absolute path
            let len = arg.len().min(out.len());
            out[..len].copy_from_slice(&arg[..len]);
            len
        } else if cwd_len > 0 {
            // Relative path
            let cwd_len = cwd_len as usize;
            
            // Copy cwd
            let copy_len = cwd_len.min(out.len());
            out[..copy_len].copy_from_slice(&cwd[..copy_len]);
            let mut pos = copy_len;
            
            // Add separator if needed
            if pos < out.len() && pos > 0 && out[pos - 1] != b'/' {
                out[pos] = b'/';
                pos += 1;
            }
            
            // Copy filename
            let remaining = out.len() - pos;
            let copy_len = arg.len().min(remaining);
            out[pos..pos + copy_len].copy_from_slice(&arg[..copy_len]);
            pos + copy_len
        } else {
            // Fallback: treat as absolute from root
            if out.len() > 0 {
                out[0] = b'/';
            }
            let copy_len = arg.len().min(out.len() - 1);
            out[1..1 + copy_len].copy_from_slice(&arg[..copy_len]);
            1 + copy_len
        }
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        if argc < 1 {
            log("Usage: cat <filename>\n");
            return;
        }
        
        let mut show_line_numbers = false;
        let mut file_arg_idx: i32 = -1;
        
        // Parse arguments
        for i in 0..argc {
            let mut arg_buf = [0u8; 256];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 256) };
            if arg_len <= 0 {
                continue;
            }
            let arg = &arg_buf[..arg_len as usize];
            
            if arg == b"-n" {
                show_line_numbers = true;
            } else if !arg.starts_with(b"-") {
                file_arg_idx = i;
            }
        }
        
        if file_arg_idx < 0 {
            log("Usage: cat <filename>\n");
            return;
        }
        
        // Get filename
        let mut filename_buf = [0u8; 256];
        let filename_len = unsafe { arg_get(file_arg_idx, filename_buf.as_mut_ptr(), 256) };
        if filename_len <= 0 {
            log("\x1b[1;31mError:\x1b[0m Invalid filename\n");
            return;
        }
        
        // Resolve path
        let mut path_buf = [0u8; 512];
        let path_len = resolve_path(&filename_buf[..filename_len as usize], &mut path_buf);
        
        // Read file
        let mut content = [0u8; 65536];
        let read_len = unsafe {
            fs_read(path_buf.as_ptr(), path_len as i32, content.as_mut_ptr(), content.len() as i32)
        };
        
        if read_len < 0 {
            log("\x1b[1;31mError:\x1b[0m File not found: ");
            unsafe { print(path_buf.as_ptr(), path_len) };
            log("\n");
            return;
        }
        
        let content = &content[..read_len as usize];
        
        if show_line_numbers {
            let mut line_num = 1usize;
            let mut line_start = 0;
            
            for (i, &c) in content.iter().enumerate() {
                if c == b'\n' || i == content.len() - 1 {
                    let end = if c == b'\n' { i } else { i + 1 };
                    
                    // Print line number
                    log("\x1b[0;90m");
                    // Right-align line number in 4 chars
                    if line_num < 10 {
                        log("   ");
                    } else if line_num < 100 {
                        log("  ");
                    } else if line_num < 1000 {
                        log(" ");
                    }
                    print_num(line_num);
                    log("\x1b[0m | ");
                    
                    // Print line content
                    unsafe { print(content[line_start..end].as_ptr(), end - line_start) };
                    log("\n");
                    
                    line_num += 1;
                    line_start = i + 1;
                }
            }
        } else {
            // Print content directly
            unsafe { print(content.as_ptr(), content.len()) };
            
            // Add newline if file doesn't end with one
            if !content.is_empty() && content[content.len() - 1] != b'\n' {
                log("\n");
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

