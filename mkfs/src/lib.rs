// mkfs/src/lib.rs
//
// This file serves two purposes:
// 1. It is mostly ignored by the host tool (mkfs binary)
// 2. For WASM targets, it provides the System Call API and Panic Handler

// Use no_std when targeting WASM
#![cfg_attr(target_arch = "wasm32", no_std)]

// Only compile this module logic when targeting WASM
#[cfg(target_arch = "wasm32")]
pub mod syscalls {
    use core::panic::PanicInfo;

    // --- System Calls provided by Kernel ---
    extern "C" {
        /// Print a string to the console
        pub fn print(ptr: *const u8, len: usize);
        /// Get current time in milliseconds
        pub fn time() -> i64;
        /// Get number of command-line arguments
        pub fn arg_count() -> i32;
        /// Get argument at index into buffer, returns actual length or -1 on error
        pub fn arg_get(index: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        /// Get current working directory into buffer, returns length or -1
        pub fn cwd_get(buf_ptr: *mut u8, buf_len: i32) -> i32;
        /// Check if file exists (1 = yes, 0 = no)
        pub fn fs_exists(path_ptr: *const u8, path_len: i32) -> i32;
        /// Read file into buffer, returns bytes read or -1 on error
        pub fn fs_read(path_ptr: *const u8, path_len: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        /// Write data to file, returns bytes written or -1 on error
        pub fn fs_write(path_ptr: *const u8, path_len: i32, data_ptr: *const u8, data_len: i32)
            -> i32;
        /// List files in directory, returns JSON-like list into buffer
        pub fn fs_list(buf_ptr: *mut u8, buf_len: i32) -> i32;
        /// Get kernel log entries, returns data into buffer
        pub fn klog_get(count: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        /// Check if network is available (1 = yes, 0 = no)
        pub fn net_available() -> i32;
        /// HTTP GET request, returns response length or -1 on error
        pub fn http_get(
            url_ptr: *const u8,
            url_len: i32,
            resp_ptr: *mut u8,
            resp_len: i32,
        ) -> i32;
    }

    // --- Helper Wrappers ---

    /// Print a string to the console
    pub fn console_log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    /// Get current time in milliseconds
    pub fn get_time() -> i64 {
        unsafe { time() }
    }

    /// Get number of arguments
    pub fn argc() -> usize {
        unsafe { arg_count() as usize }
    }

    /// Get argument at index (returns None if out of bounds or buffer too small)
    pub fn argv(index: usize, buf: &mut [u8]) -> Option<usize> {
        let len = unsafe { arg_get(index as i32, buf.as_mut_ptr(), buf.len() as i32) };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Get current working directory
    pub fn get_cwd(buf: &mut [u8]) -> Option<usize> {
        let len = unsafe { cwd_get(buf.as_mut_ptr(), buf.len() as i32) };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Check if file exists
    pub fn file_exists(path: &str) -> bool {
        unsafe { fs_exists(path.as_ptr(), path.len() as i32) == 1 }
    }

    /// Read file contents into buffer, returns bytes read
    pub fn read_file(path: &str, buf: &mut [u8]) -> Option<usize> {
        let len =
            unsafe { fs_read(path.as_ptr(), path.len() as i32, buf.as_mut_ptr(), buf.len() as i32) };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Write data to file
    pub fn write_file(path: &str, data: &[u8]) -> bool {
        let written = unsafe {
            fs_write(
                path.as_ptr(),
                path.len() as i32,
                data.as_ptr(),
                data.len() as i32,
            )
        };
        written >= 0
    }

    /// List files (returns raw data into buffer)
    pub fn list_files(buf: &mut [u8]) -> Option<usize> {
        let len = unsafe { fs_list(buf.as_mut_ptr(), buf.len() as i32) };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Get kernel log entries
    pub fn get_klog(count: usize, buf: &mut [u8]) -> Option<usize> {
        let len = unsafe { klog_get(count as i32, buf.as_mut_ptr(), buf.len() as i32) };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Check if network is available
    pub fn is_net_available() -> bool {
        unsafe { net_available() == 1 }
    }

    /// HTTP GET request
    pub fn http_fetch(url: &str, buf: &mut [u8]) -> Option<usize> {
        let len = unsafe {
            http_get(
                url.as_ptr(),
                url.len() as i32,
                buf.as_mut_ptr(),
                buf.len() as i32,
            )
        };
        if len >= 0 {
            Some(len as usize)
        } else {
            None
        }
    }

    /// Print an integer
    pub fn print_int(n: i64) {
        let mut buf = [0u8; 20];
        let s = int_to_str(n, &mut buf);
        console_log(s);
    }

    /// Convert integer to string (helper)
    pub fn int_to_str(mut n: i64, buf: &mut [u8]) -> &str {
        if n == 0 {
            buf[0] = b'0';
            return unsafe { core::str::from_utf8_unchecked(&buf[..1]) };
        }

        let negative = n < 0;
        if negative {
            n = -n;
        }

        let mut i = buf.len();
        while n > 0 && i > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }

        if negative && i > 0 {
            i -= 1;
            buf[i] = b'-';
        }

        unsafe { core::str::from_utf8_unchecked(&buf[i..]) }
    }

    // --- Mandatory Panic Handler for no_std WASM ---
    #[panic_handler]
    fn panic(_info: &PanicInfo) -> ! {
        let msg = "WASM Panic!\n";
        unsafe { print(msg.as_ptr(), msg.len()) };
        loop {}
    }
}

// Re-export for easier access in scripts
#[cfg(target_arch = "wasm32")]
pub use syscalls::*;
