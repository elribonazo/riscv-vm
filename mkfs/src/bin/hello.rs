// mkfs/src/bin/hello.rs
//
// Example WASM script that runs inside the kernel.
// Build with: cargo build --release --bin hello --target wasm32-unknown-unknown

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

// Import from our own lib.rs
#[cfg(target_arch = "wasm32")]
use mkfs::{console_log, get_time};

#[no_mangle]
#[cfg(target_arch = "wasm32")]
pub extern "C" fn _start() {
    console_log("Hello from inside MKFS project!\n");

    let t = get_time();
    if t > 0 {
        console_log("Time syscall works.\n");
    }
}

// Dummy main for host compilation to stop cargo complaining
// when you run `cargo build` without specific targets
#[cfg(not(target_arch = "wasm32"))]
fn main() {}

