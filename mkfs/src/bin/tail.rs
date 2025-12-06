// tail - Show last lines of a file
//
// Usage:
//   tail <file>           Show last 10 lines
//   tail -n <N> <file>    Show last N lines
//   tail -<N> <file>      Show last N lines (shorthand)

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

    fn parse_num(s: &[u8]) -> Option<usize> {
        if s.is_empty() {
            return None;
        }
        let mut result = 0usize;
        for &c in s {
            if c < b'0' || c > b'9' {
                return None;
            }
            result = result.checked_mul(10)?.checked_add((c - b'0') as usize)?;
        }
        Some(result)
    }

    fn resolve_path(arg: &[u8], out: &mut [u8]) -> usize {
        let mut cwd = [0u8; 256];
        let cwd_len = unsafe { cwd_get(cwd.as_mut_ptr(), cwd.len() as i32) };
        
        if arg.starts_with(b"/") {
            let len = arg.len().min(out.len());
            out[..len].copy_from_slice(&arg[..len]);
            len
        } else if cwd_len > 0 {
            let cwd_len = cwd_len as usize;
            let copy_len = cwd_len.min(out.len());
            out[..copy_len].copy_from_slice(&cwd[..copy_len]);
            let mut pos = copy_len;
            
            if pos < out.len() && pos > 0 && out[pos - 1] != b'/' {
                out[pos] = b'/';
                pos += 1;
            }
            
            let remaining = out.len() - pos;
            let copy_len = arg.len().min(remaining);
            out[pos..pos + copy_len].copy_from_slice(&arg[..copy_len]);
            pos + copy_len
        } else {
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
            log("Usage: tail [-n NUM] <file...>\n");
            return;
        }
        
        let mut num_lines = 10usize;
        let mut files: [(usize, usize); 16] = [(0, 0); 16];
        let mut file_count = 0usize;
        let mut args_storage = [0u8; 4096];
        let mut storage_pos = 0usize;
        
        // Parse arguments
        let mut i = 0i32;
        while i < argc {
            let mut arg_buf = [0u8; 256];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 256) };
            if arg_len <= 0 {
                i += 1;
                continue;
            }
            let arg = &arg_buf[..arg_len as usize];
            
            if arg == b"-n" {
                // Next argument is the number
                i += 1;
                if i < argc {
                    let mut num_buf = [0u8; 16];
                    let num_len = unsafe { arg_get(i, num_buf.as_mut_ptr(), 16) };
                    if num_len > 0 {
                        if let Some(n) = parse_num(&num_buf[..num_len as usize]) {
                            num_lines = n.max(1);
                        }
                    }
                }
            } else if arg.starts_with(b"-n") && arg.len() > 2 {
                // -nNUM format
                if let Some(n) = parse_num(&arg[2..]) {
                    num_lines = n.max(1);
                }
            } else if arg.starts_with(b"-") && arg.len() > 1 && arg[1] >= b'0' && arg[1] <= b'9' {
                // -NUM format
                if let Some(n) = parse_num(&arg[1..]) {
                    num_lines = n.max(1);
                }
            } else if !arg.starts_with(b"-") && file_count < 16 {
                // File argument
                let remaining = args_storage.len() - storage_pos;
                let copy_len = arg.len().min(remaining);
                if copy_len > 0 {
                    args_storage[storage_pos..storage_pos + copy_len].copy_from_slice(&arg[..copy_len]);
                    files[file_count] = (storage_pos, copy_len);
                    storage_pos += copy_len;
                    file_count += 1;
                }
            }
            
            i += 1;
        }
        
        if file_count == 0 {
            log("Usage: tail [-n NUM] <file...>\n");
            return;
        }
        
        let show_headers = file_count > 1;
        
        // Process each file
        for f in 0..file_count {
            let (start, len) = files[f];
            let file_arg = &args_storage[start..start + len];
            
            // Resolve path
            let mut path_buf = [0u8; 512];
            let path_len = resolve_path(file_arg, &mut path_buf);
            
            // Read file
            let mut content = [0u8; 65536];
            let read_len = unsafe {
                fs_read(path_buf.as_ptr(), path_len as i32, content.as_mut_ptr(), content.len() as i32)
            };
            
            if read_len < 0 {
                log("\x1b[1;31mtail:\x1b[0m cannot open '");
                unsafe { print(path_buf.as_ptr(), path_len) };
                log("': No such file\n");
                continue;
            }
            
            if show_headers {
                if f > 0 {
                    log("\n");
                }
                log("\x1b[1m==> ");
                unsafe { print(path_buf.as_ptr(), path_len) };
                log(" <==\x1b[0m\n");
            }
            
            let content = &content[..read_len as usize];
            
            // Count lines and find positions
            let mut line_positions: [usize; 1024] = [0; 1024];
            let mut line_count = 0usize;
            line_positions[0] = 0;
            
            for (idx, &c) in content.iter().enumerate() {
                if c == b'\n' && idx + 1 < content.len() && line_count + 1 < 1024 {
                    line_count += 1;
                    line_positions[line_count] = idx + 1;
                }
            }
            line_count += 1; // Total number of lines
            
            // Calculate start line
            let start_line = if line_count > num_lines {
                line_count - num_lines
            } else {
                0
            };
            
            // Print lines from start_line onwards
            for line_idx in start_line..line_count {
                let line_start = line_positions[line_idx];
                let line_end = if line_idx + 1 < line_count {
                    line_positions[line_idx + 1] - 1 // Exclude newline
                } else {
                    content.len()
                };
                
                if line_start < content.len() {
                    let end = line_end.min(content.len());
                    unsafe { print(content[line_start..end].as_ptr(), end - line_start) };
                    log("\n");
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

