// write - Write content to a file
//
// Usage:
//   write <filename> <content...>    Write content to file

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
        fn fs_write(path_ptr: *const u8, path_len: i32, data_ptr: *const u8, data_len: i32) -> i32;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
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
            log("Usage: write <filename> <content...>\n");
            log("Example: write test.txt Hello World!\n");
            return;
        }
        
        // Get filename (first argument)
        let mut filename_buf = [0u8; 256];
        let filename_len = unsafe { arg_get(0, filename_buf.as_mut_ptr(), 256) };
        if filename_len <= 0 {
            log("\x1b[1;31mError:\x1b[0m Invalid filename\n");
            return;
        }
        
        // Resolve path
        let mut path_buf = [0u8; 512];
        let path_len = resolve_path(&filename_buf[..filename_len as usize], &mut path_buf);
        
        // Collect content from remaining arguments
        let mut content = [0u8; 8192];
        let mut content_len = 0usize;
        
        for i in 1..argc {
            let mut arg_buf = [0u8; 1024];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 1024) };
            if arg_len <= 0 {
                continue;
            }
            
            // Add space between arguments
            if content_len > 0 && content_len < content.len() {
                content[content_len] = b' ';
                content_len += 1;
            }
            
            // Copy argument
            let copy_len = (arg_len as usize).min(content.len() - content_len);
            if copy_len > 0 {
                content[content_len..content_len + copy_len].copy_from_slice(&arg_buf[..copy_len]);
                content_len += copy_len;
            }
        }
        
        // Write file
        let result = unsafe {
            fs_write(path_buf.as_ptr(), path_len as i32, content.as_ptr(), content_len as i32)
        };
        
        if result >= 0 {
            log("\x1b[1;32m✓\x1b[0m Written to ");
            unsafe { print(path_buf.as_ptr(), path_len) };
            log("\n");
        } else {
            log("\x1b[1;31mError:\x1b[0m Failed to write to ");
            unsafe { print(path_buf.as_ptr(), path_len) };
            log("\n");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

