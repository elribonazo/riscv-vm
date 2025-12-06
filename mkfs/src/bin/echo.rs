// echo - Print arguments to stdout
//
// Usage:
//   echo <text>       Print text followed by newline
//   echo -n <text>    Print text without newline

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
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        let mut no_newline = false;
        let mut start_idx = 0;
        
        // Check for -n flag
        if argc > 0 {
            let mut arg_buf = [0u8; 16];
            let arg_len = unsafe { arg_get(0, arg_buf.as_mut_ptr(), 16) };
            if arg_len == 2 && arg_buf[0] == b'-' && arg_buf[1] == b'n' {
                no_newline = true;
                start_idx = 1;
            }
        }
        
        // Print all arguments
        let mut first = true;
        for i in start_idx..argc {
            let mut arg_buf = [0u8; 1024];
            let arg_len = unsafe { arg_get(i, arg_buf.as_mut_ptr(), 1024) };
            
            if arg_len > 0 {
                if !first {
                    log(" ");
                }
                first = false;
                unsafe { print(arg_buf.as_ptr(), arg_len as usize) };
            }
        }
        
        if !no_newline {
            log("\n");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

