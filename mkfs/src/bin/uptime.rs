// uptime - Show system uptime
//
// Usage:
//   uptime        Show how long the system has been running

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
extern crate mkfs;

#[cfg(target_arch = "wasm32")]
mod wasm {
    extern "C" {
        fn print(ptr: *const u8, len: usize);
        fn time() -> i64;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn print_num(mut n: i64) {
        if n == 0 {
            log("0");
            return;
        }
        if n < 0 {
            log("-");
            n = -n;
        }
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        while n > 0 && i > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        unsafe { print(buf[i..].as_ptr(), buf.len() - i) };
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let ms = unsafe { time() };
        let total_sec = ms / 1000;
        let hours = total_sec / 3600;
        let minutes = (total_sec % 3600) / 60;
        let seconds = total_sec % 60;
        
        log("Uptime: ");
        
        if hours > 0 {
            print_num(hours);
            log("h ");
            print_num(minutes);
            log("m ");
            print_num(seconds);
            log("s\n");
        } else if minutes > 0 {
            print_num(minutes);
            log("m ");
            print_num(seconds);
            log("s\n");
        } else {
            print_num(seconds);
            log("s\n");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

