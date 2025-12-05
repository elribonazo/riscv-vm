pub mod bus;
pub mod cpu;
pub mod devices;
pub mod dram;
pub mod engine;
pub mod mmu;
pub use devices::{clint, plic, uart};
pub mod loader;
pub mod net;
pub mod shared_mem;
pub mod snapshot;
pub mod vm;

pub use cpu::{Mode, Trap, csr};

#[cfg(all(feature = "napi", not(target_arch = "wasm32")))]
pub mod napi_bindings;

#[cfg(not(target_arch = "wasm32"))]
pub mod console;

#[cfg(target_arch = "wasm32")]
pub mod worker;

// Re-export specific VM types for consumers
pub use vm::emulator::Emulator;

#[cfg(target_arch = "wasm32")]
pub use vm::wasm::{NetworkStatus, WasmVm};

#[cfg(not(target_arch = "wasm32"))]
pub use vm::native::NativeVm;
