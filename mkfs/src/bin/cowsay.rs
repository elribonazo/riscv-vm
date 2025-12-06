// cowsay - Make a cow say something!
//
// Usage:
//   cowsay              Say "Moo!"
//   cowsay <message>    Say a custom message

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
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_char(c: u8, count: usize) {
        for _ in 0..count {
            unsafe { print(&c as *const u8, 1) };
        }
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        // Collect message from arguments or use default
        let mut msg_buf = [0u8; 256];
        let mut msg_len: usize = 0;
        
        if argc > 0 {
            // Concatenate all arguments with spaces
            for i in 0..argc {
                if i > 0 && msg_len < 255 {
                    msg_buf[msg_len] = b' ';
                    msg_len += 1;
                }
                let len = unsafe { arg_get(i, msg_buf[msg_len..].as_mut_ptr(), (256 - msg_len) as i32) };
                if len > 0 {
                    msg_len += len as usize;
                }
            }
        }
        
        // Default message
        if msg_len == 0 {
            let default = b"Moo!";
            msg_buf[..default.len()].copy_from_slice(default);
            msg_len = default.len();
        }
        
        // Draw the speech bubble
        let bubble_width = msg_len + 2;
        
        // Top border
        log(" ");
        print_char(b'_', bubble_width);
        log("\n");
        
        // Message line
        log("< ");
        unsafe { print(msg_buf.as_ptr(), msg_len) };
        log(" >\n");
        
        // Bottom border
        log(" ");
        print_char(b'-', bubble_width);
        log("\n");
        
        // The cow
        log("        \\   ^__^\n");
        log("         \\  (oo)\\_______\n");
        log("            (__)\\       )\\/\\\n");
        log("                ||----w |\n");
        log("                ||     ||\n");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
