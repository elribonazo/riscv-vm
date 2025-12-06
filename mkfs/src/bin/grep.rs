// grep - Search for patterns in files
//
// Usage:
//   grep <pattern> <file...>     Search for pattern in files
//   grep -i <pattern> <file>     Case-insensitive search
//   grep -n <pattern> <file>     Show line numbers
//   grep -v <pattern> <file>     Invert match (show non-matching lines)

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

    fn to_lower(c: u8) -> u8 {
        if c >= b'A' && c <= b'Z' {
            c + 32
        } else {
            c
        }
    }

    fn contains_pattern(line: &[u8], pattern: &[u8], case_insensitive: bool) -> Option<usize> {
        if pattern.is_empty() || pattern.len() > line.len() {
            return None;
        }
        
        'outer: for i in 0..=line.len() - pattern.len() {
            for j in 0..pattern.len() {
                let a = if case_insensitive { to_lower(line[i + j]) } else { line[i + j] };
                let b = if case_insensitive { to_lower(pattern[j]) } else { pattern[j] };
                if a != b {
                    continue 'outer;
                }
            }
            return Some(i);
        }
        None
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
        
        if argc < 2 {
            log("Usage: grep [OPTIONS] <pattern> [file...]\n");
            log("Options: -i (case-insensitive), -n (line numbers), -v (invert)\n");
            return;
        }
        
        let mut case_insensitive = false;
        let mut show_line_numbers = false;
        let mut invert_match = false;
        let mut pattern_buf = [0u8; 256];
        let mut pattern_len = 0usize;
        let mut files: [(usize, usize); 16] = [(0, 0); 16]; // (start_idx in args, len)
        let mut file_count = 0usize;
        let mut args_storage = [0u8; 4096];
        let mut storage_pos = 0usize;
        
        // Parse arguments
        for i in 0..argc {
            let mut arg_buf = [0u8; 256];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 256) };
            if arg_len <= 0 {
                continue;
            }
            let arg = &arg_buf[..arg_len as usize];
            
            if arg.starts_with(b"-") && pattern_len == 0 {
                for &c in &arg[1..] {
                    match c {
                        b'i' => case_insensitive = true,
                        b'n' => show_line_numbers = true,
                        b'v' => invert_match = true,
                        _ => {}
                    }
                }
            } else if pattern_len == 0 {
                let len = arg.len().min(pattern_buf.len());
                pattern_buf[..len].copy_from_slice(&arg[..len]);
                pattern_len = len;
            } else if file_count < 16 {
                // Store file path
                let remaining = args_storage.len() - storage_pos;
                let copy_len = arg.len().min(remaining);
                if copy_len > 0 {
                    args_storage[storage_pos..storage_pos + copy_len].copy_from_slice(&arg[..copy_len]);
                    files[file_count] = (storage_pos, copy_len);
                    storage_pos += copy_len;
                    file_count += 1;
                }
            }
        }
        
        if pattern_len == 0 {
            log("Usage: grep [OPTIONS] <pattern> <file...>\n");
            return;
        }
        
        if file_count == 0 {
            log("Usage: grep [OPTIONS] <pattern> <file...>\n");
            return;
        }
        
        let pattern = &pattern_buf[..pattern_len];
        let show_filename = file_count > 1;
        
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
                log("\x1b[1;31mgrep:\x1b[0m ");
                unsafe { print(path_buf.as_ptr(), path_len) };
                log(": No such file\n");
                continue;
            }
            
            let content = &content[..read_len as usize];
            let mut line_num = 1usize;
            let mut line_start = 0;
            
            for (i, &c) in content.iter().enumerate() {
                if c == b'\n' || i == content.len() - 1 {
                    let end = if c == b'\n' { i } else { i + 1 };
                    let line = &content[line_start..end];
                    
                    let match_pos = contains_pattern(line, pattern, case_insensitive);
                    let matches = match_pos.is_some();
                    let should_print = if invert_match { !matches } else { matches };
                    
                    if should_print {
                        if show_filename {
                            log("\x1b[1;35m");
                            unsafe { print(path_buf.as_ptr(), path_len) };
                            log("\x1b[0m:");
                        }
                        if show_line_numbers {
                            log("\x1b[1;32m");
                            print_num(line_num);
                            log("\x1b[0m:");
                        }
                        
                        if !invert_match {
                            if let Some(pos) = match_pos {
                                // Highlight match
                                unsafe { print(line[..pos].as_ptr(), pos) };
                                log("\x1b[1;31m");
                                unsafe { print(line[pos..pos + pattern_len].as_ptr(), pattern_len) };
                                log("\x1b[0m");
                                unsafe { print(line[pos + pattern_len..].as_ptr(), line.len() - pos - pattern_len) };
                            } else {
                                unsafe { print(line.as_ptr(), line.len()) };
                            }
                        } else {
                            unsafe { print(line.as_ptr(), line.len()) };
                        }
                        log("\n");
                    }
                    
                    line_num += 1;
                    line_start = i + 1;
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

