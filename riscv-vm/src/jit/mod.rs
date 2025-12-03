//! JIT Compilation Engine for RISC-V to WASM Translation
//!
//! This module implements a tiered JIT compilation strategy:
//!
//! - **Tier 0 (Interpreter):** Default execution via `Cpu::step()`
//! - **Tier 1 (Block JIT):** Hot blocks compiled to WASM functions
//! - **Tier 2 (Trace JIT):** [Future] Multiple blocks stitched together
//!
//! ## Architecture
//!
//! The JIT compiler translates pre-decoded `MicroOp` blocks into WASM
//! binary modules. Each compiled block becomes a WASM function that:
//!
//! 1. Imports the VM's `WebAssembly.Memory` for CPU state access
//! 2. Reads/writes registers via direct memory loads/stores
//! 3. Calls imported helper functions for complex operations (MMU, CSR)
//! 4. Returns the next PC or an error code on trap
//!
//! ## Memory Layout (in SharedArrayBuffer)
//!
//! The JIT'd code accesses CPU state at a fixed offset in shared memory.
//! See `JitCpuState` for the exact layout.

pub mod cache;
pub mod compiler;
pub mod disasm;
pub mod encoder;
pub mod helpers;
pub mod runtime;
pub mod state;
pub mod trace;
pub mod types;
pub mod worker;

// WASM exports for JIT worker (only on wasm32 target)
#[cfg(target_arch = "wasm32")]
pub mod wasm_exports;

pub use cache::{CacheStats, JitCache, JitFunction};
pub use compiler::JitCompiler;
pub use disasm::{disassemble_block, format_instruction_short, log_jit_compilation, DisasmConfig};
pub use runtime::JitExecResult;
pub use trace::{TraceBuffer, TraceEvent, TraceStats};
pub use types::{
    BlockStatus, CompilationResult, JitConfig, JitDiagnostics, JitRuntime, JitStats, JitTier,
};
pub use worker::{CompileRequest, CompileResponse, CompileStatus, SerializedMicroOp};

// Re-export JitWorkerContext for WASM builds
#[cfg(target_arch = "wasm32")]
pub use wasm_exports::JitWorkerContext;

