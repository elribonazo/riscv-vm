//! JIT Runtime: Execute Compiled WASM Modules
//!
//! Platform-specific execution of JIT-compiled WASM.

use super::types::exit_codes::*;

/// Result of executing a JIT'd block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitExecResult {
    /// Block completed, next PC
    Continue(u64),
    /// Block ended with a trap
    Trap { code: u32, fault_pc: u64 },
    /// Block needs interpreter fallback
    ExitToInterpreter { pc: u64 },
    /// Interrupt check required
    InterruptCheck { pc: u64 },
    /// Branch taken - PC already updated in shared memory
    Branch { new_pc: u64 },
}

impl JitExecResult {
    /// Parse the i64 return value from a JIT'd function.
    ///
    /// The JIT'd function returns an i64 where:
    /// - High 32 bits: Exit code type
    /// - Low 32 bits: Payload (e.g., PC offset or new PC for branches)
    pub fn from_i64(value: i64, base_pc: u64) -> Self {
        let hi = (value >> 32) as u32;
        let lo = (value & 0xFFFFFFFF) as u32;

        match hi {
            EXIT_NORMAL => {
                let next_pc = base_pc.wrapping_add(lo as u64);
                JitExecResult::Continue(next_pc)
            }
            EXIT_TRAP => JitExecResult::Trap {
                code: lo,
                fault_pc: base_pc,
            },
            EXIT_INTERPRETER => {
                let pc = base_pc.wrapping_add(lo as u64);
                JitExecResult::ExitToInterpreter { pc }
            }
            EXIT_INTERRUPT_CHECK => {
                let pc = base_pc.wrapping_add(lo as u64);
                JitExecResult::InterruptCheck { pc }
            }
            EXIT_BRANCH => {
                // For branches, lo contains the new PC directly (not an offset)
                JitExecResult::Branch { new_pc: lo as u64 }
            }
            _ => {
                // Unknown exit code - treat as interpreter fallback
                JitExecResult::ExitToInterpreter { pc: base_pc }
            }
        }
    }

    /// Create a return value for normal completion.
    pub fn make_continue(pc_offset: u32) -> i64 {
        ((EXIT_NORMAL as i64) << 32) | (pc_offset as i64)
    }

    /// Create a return value for a trap.
    pub fn make_trap(trap_code: u32) -> i64 {
        ((EXIT_TRAP as i64) << 32) | (trap_code as i64)
    }

    /// Create a return value for interpreter fallback.
    pub fn make_interpreter(pc_offset: u32) -> i64 {
        ((EXIT_INTERPRETER as i64) << 32) | (pc_offset as i64)
    }

    /// Create a return value for interrupt check.
    pub fn make_interrupt_check(pc_offset: u32) -> i64 {
        ((EXIT_INTERRUPT_CHECK as i64) << 32) | (pc_offset as i64)
    }

    /// Create a return value for a branch.
    pub fn make_branch(new_pc: u32) -> i64 {
        ((EXIT_BRANCH as i64) << 32) | (new_pc as i64)
    }
}

/// JIT runtime for native (non-WASM) builds.
#[cfg(not(target_arch = "wasm32"))]
pub mod native {
    use super::JitExecResult;

    /// Execute a JIT'd WASM module on native platforms.
    ///
    /// For native builds, we could use wasmtime or wasmer to execute
    /// the compiled WASM. For now, this is a placeholder that falls
    /// back to the interpreter.
    ///
    /// # Arguments
    /// * `_wasm_bytes` - The compiled WASM module bytes
    /// * `_cpu_state_ptr` - Pointer to CPU state in memory
    /// * `_base_pc` - Base PC of the block
    ///
    /// # Returns
    /// Always returns `ExitToInterpreter` as a placeholder.
    pub fn execute_jit_module(
        _wasm_bytes: &[u8],
        _cpu_state_ptr: *mut u8,
        _base_pc: u64,
    ) -> JitExecResult {
        // TODO: Implement native WASM execution via wasmtime/wasmer
        // For now, fall back to interpreter
        JitExecResult::ExitToInterpreter { pc: _base_pc }
    }

    /// Check if native JIT execution is available.
    ///
    /// Returns `false` until wasmtime/wasmer integration is implemented.
    pub fn is_available() -> bool {
        false
    }
}

/// JIT runtime for WASM builds (browser).
#[cfg(target_arch = "wasm32")]
pub mod wasm {
    use super::JitExecResult;
    use crate::bus::Bus;
    use crate::cpu::types::Mode;
    use crate::jit::helpers;
    use crate::jit::state::{offsets, JIT_STATE_OFFSET};
    use crate::mmu::Tlb;
    use js_sys::{Function, Object, Reflect, Uint8Array, WebAssembly};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    thread_local! {
        /// Cache of instantiated modules keyed by base PC.
        static MODULE_CACHE: RefCell<HashMap<u64, WebAssembly::Instance>> = RefCell::new(HashMap::new());
    }

    /// Execution context passed to helper functions.
    ///
    /// This is stored in a thread-local to allow closures to access it.
    pub struct JitContext {
        pub bus: *const dyn Bus,
        pub tlb: *mut Tlb,
        pub mode: Mode,
        pub satp: u64,
        pub mstatus: u64,
        pub shared_buffer: js_sys::SharedArrayBuffer,
    }

    // SAFETY: JitContext contains raw pointers but is only accessed from
    // thread-local storage within a single WASM thread/worker.
    unsafe impl Send for JitContext {}
    unsafe impl Sync for JitContext {}

    thread_local! {
        static JIT_CONTEXT: RefCell<Option<JitContext>> = RefCell::new(None);
    }

    /// Set the execution context before running JIT'd code.
    pub fn set_context(ctx: JitContext) {
        JIT_CONTEXT.with(|c| {
            *c.borrow_mut() = Some(ctx);
        });
    }

    /// Clear the execution context after JIT execution.
    pub fn clear_context() {
        JIT_CONTEXT.with(|c| {
            *c.borrow_mut() = None;
        });
    }

    /// Create the imports object for JIT'd modules.
    fn create_imports(memory: &WebAssembly::Memory) -> Result<Object, JsValue> {
        let imports = Object::new();
        let env = Object::new();

        // Add shared memory
        Reflect::set(&env, &"memory".into(), memory)?;

        // Create read_u64 helper
        let read_u64_fn = Closure::wrap(Box::new(|vaddr: i64| -> i64 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (result, ok) =
                        helpers::mmu_read_u64(bus, tlb, ctx.mode, ctx.satp, ctx.mstatus, vaddr as u64);

                    if ok {
                        result as i64
                    } else {
                        // Set trap flag in shared memory
                        set_trap_flag(&ctx.shared_buffer, result as u32, vaddr as u64);
                        -1i64
                    }
                }
            })
        }) as Box<dyn Fn(i64) -> i64>);

        Reflect::set(&env, &"read_u64".into(), read_u64_fn.as_ref())?;
        read_u64_fn.forget(); // Leak - lives for program lifetime

        // Create read_u32 helper
        let read_u32_fn = Closure::wrap(Box::new(|vaddr: i64| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (result, ok) =
                        helpers::mmu_read_u32(bus, tlb, ctx.mode, ctx.satp, ctx.mstatus, vaddr as u64);

                    if ok {
                        result as i32
                    } else {
                        set_trap_flag(&ctx.shared_buffer, result, vaddr as u64);
                        -1i32
                    }
                }
            })
        }) as Box<dyn Fn(i64) -> i32>);

        Reflect::set(&env, &"read_u32".into(), read_u32_fn.as_ref())?;
        read_u32_fn.forget();

        // Create read_u16 helper
        let read_u16_fn = Closure::wrap(Box::new(|vaddr: i64| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (result, ok) =
                        helpers::mmu_read_u16(bus, tlb, ctx.mode, ctx.satp, ctx.mstatus, vaddr as u64);

                    if ok {
                        result as i32
                    } else {
                        set_trap_flag(&ctx.shared_buffer, result as u32, vaddr as u64);
                        -1i32
                    }
                }
            })
        }) as Box<dyn Fn(i64) -> i32>);

        Reflect::set(&env, &"read_u16".into(), read_u16_fn.as_ref())?;
        read_u16_fn.forget();

        // Create read_u8 helper
        let read_u8_fn = Closure::wrap(Box::new(|vaddr: i64| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (result, ok) =
                        helpers::mmu_read_u8(bus, tlb, ctx.mode, ctx.satp, ctx.mstatus, vaddr as u64);

                    if ok {
                        result as i32
                    } else {
                        set_trap_flag(&ctx.shared_buffer, result as u32, vaddr as u64);
                        -1i32
                    }
                }
            })
        }) as Box<dyn Fn(i64) -> i32>);

        Reflect::set(&env, &"read_u8".into(), read_u8_fn.as_ref())?;
        read_u8_fn.forget();

        // Create write_u64 helper
        let write_u64_fn = Closure::wrap(Box::new(|vaddr: i64, value: i64| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (trap_code, ok) = helpers::mmu_write_u64(
                        bus,
                        tlb,
                        ctx.mode,
                        ctx.satp,
                        ctx.mstatus,
                        vaddr as u64,
                        value as u64,
                    );

                    if ok {
                        0
                    } else {
                        set_trap_flag(&ctx.shared_buffer, trap_code, vaddr as u64);
                        trap_code as i32
                    }
                }
            })
        }) as Box<dyn Fn(i64, i64) -> i32>);

        Reflect::set(&env, &"write_u64".into(), write_u64_fn.as_ref())?;
        write_u64_fn.forget();

        // Create write_u32 helper
        let write_u32_fn = Closure::wrap(Box::new(|vaddr: i64, value: i32| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (trap_code, ok) = helpers::mmu_write_u32(
                        bus,
                        tlb,
                        ctx.mode,
                        ctx.satp,
                        ctx.mstatus,
                        vaddr as u64,
                        value as u32,
                    );

                    if ok {
                        0
                    } else {
                        set_trap_flag(&ctx.shared_buffer, trap_code, vaddr as u64);
                        trap_code as i32
                    }
                }
            })
        }) as Box<dyn Fn(i64, i32) -> i32>);

        Reflect::set(&env, &"write_u32".into(), write_u32_fn.as_ref())?;
        write_u32_fn.forget();

        // Create write_u16 helper
        let write_u16_fn = Closure::wrap(Box::new(|vaddr: i64, value: i32| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (trap_code, ok) = helpers::mmu_write_u16(
                        bus,
                        tlb,
                        ctx.mode,
                        ctx.satp,
                        ctx.mstatus,
                        vaddr as u64,
                        value as u16,
                    );

                    if ok {
                        0
                    } else {
                        set_trap_flag(&ctx.shared_buffer, trap_code, vaddr as u64);
                        trap_code as i32
                    }
                }
            })
        }) as Box<dyn Fn(i64, i32) -> i32>);

        Reflect::set(&env, &"write_u16".into(), write_u16_fn.as_ref())?;
        write_u16_fn.forget();

        // Create write_u8 helper
        let write_u8_fn = Closure::wrap(Box::new(|vaddr: i64, value: i32| -> i32 {
            JIT_CONTEXT.with(|ctx| {
                let ctx = ctx.borrow();
                let ctx = ctx.as_ref().expect("JIT context not set");

                unsafe {
                    let bus = &*ctx.bus;
                    let tlb = &mut *ctx.tlb;
                    let (trap_code, ok) = helpers::mmu_write_u8(
                        bus,
                        tlb,
                        ctx.mode,
                        ctx.satp,
                        ctx.mstatus,
                        vaddr as u64,
                        value as u8,
                    );

                    if ok {
                        0
                    } else {
                        set_trap_flag(&ctx.shared_buffer, trap_code, vaddr as u64);
                        trap_code as i32
                    }
                }
            })
        }) as Box<dyn Fn(i64, i32) -> i32>);

        Reflect::set(&env, &"write_u8".into(), write_u8_fn.as_ref())?;
        write_u8_fn.forget();

        Reflect::set(&imports, &"env".into(), &env)?;
        Ok(imports)
    }

    /// Set trap flag in shared memory.
    fn set_trap_flag(buffer: &js_sys::SharedArrayBuffer, code: u32, value: u64) {
        let view = js_sys::DataView::new_with_shared_array_buffer(buffer, 0, buffer.byte_length() as usize);
        let base = JIT_STATE_OFFSET;

        // Set trap_pending
        view.set_uint32_endian(base + offsets::TRAP_PENDING as usize, 1, true);
        // Set trap_code
        view.set_uint32_endian(base + offsets::TRAP_CODE as usize, code, true);
        // Set trap_value (as two 32-bit writes)
        view.set_uint32_endian(base + offsets::TRAP_VALUE as usize, value as u32, true);
        view.set_uint32_endian(
            base + offsets::TRAP_VALUE as usize + 4,
            (value >> 32) as u32,
            true,
        );
    }

    /// Compile and instantiate a JIT'd module.
    pub async fn compile_and_instantiate(
        wasm_bytes: &[u8],
        memory: &WebAssembly::Memory,
        base_pc: u64,
    ) -> Result<WebAssembly::Instance, JsValue> {
        // Check cache first
        let cached = MODULE_CACHE.with(|cache| cache.borrow().get(&base_pc).cloned());

        if let Some(instance) = cached {
            return Ok(instance);
        }

        // Create imports
        let imports = create_imports(memory)?;

        // Compile module
        let module_promise = WebAssembly::compile(&Uint8Array::from(wasm_bytes));
        let module: WebAssembly::Module =
            wasm_bindgen_futures::JsFuture::from(module_promise)
                .await?
                .dyn_into()?;

        // Instantiate
        let instance_promise = WebAssembly::instantiate_module(&module, &imports);
        let instance: WebAssembly::Instance =
            wasm_bindgen_futures::JsFuture::from(instance_promise)
                .await?
                .dyn_into()?;

        // Cache the instance
        MODULE_CACHE.with(|cache| {
            cache.borrow_mut().insert(base_pc, instance.clone());
        });

        Ok(instance)
    }

    /// Execute a JIT'd block synchronously.
    ///
    /// This assumes the module is already cached.
    pub fn execute_cached(base_pc: u64) -> Option<JitExecResult> {
        MODULE_CACHE.with(|cache| {
            let cache = cache.borrow();
            let instance = cache.get(&base_pc)?;

            // Get the "run" export
            let exports = instance.exports();
            let run_fn: Function = Reflect::get(&exports, &"run".into())
                .ok()?
                .dyn_into()
                .ok()?;

            // Call with CPU state offset
            let cpu_state_offset = JIT_STATE_OFFSET as u32;
            let result = run_fn.call1(&JsValue::NULL, &JsValue::from(cpu_state_offset));

            match result {
                Ok(val) => {
                    let ret_val = val.as_f64().unwrap_or(0.0) as i64;
                    Some(JitExecResult::from_i64(ret_val, base_pc))
                }
                Err(_) => Some(JitExecResult::ExitToInterpreter { pc: base_pc }),
            }
        })
    }

    /// Invalidate the JIT cache (e.g., on TLB flush).
    pub fn invalidate_cache() {
        MODULE_CACHE.with(|cache| {
            cache.borrow_mut().clear();
        });
    }

    /// Check if a block is cached.
    pub fn is_cached(base_pc: u64) -> bool {
        MODULE_CACHE.with(|cache| cache.borrow().contains_key(&base_pc))
    }

    /// Instantiate a WASM module and return the "run" function.
    ///
    /// This is an async operation that compiles and instantiates the
    /// JIT-generated WASM module in the browser.
    ///
    /// # Arguments
    /// * `wasm_bytes` - The compiled WASM module bytes
    /// * `shared_memory` - The shared WebAssembly.Memory for CPU state
    ///
    /// # Returns
    /// The "run" function from the instantiated module.
    pub async fn instantiate_module(
        wasm_bytes: &[u8],
        shared_memory: &JsValue,
    ) -> Result<Function, JsValue> {
        // Create imports object with shared memory
        let imports = Object::new();
        let env = Object::new();
        Reflect::set(&env, &"memory".into(), shared_memory)?;
        Reflect::set(&imports, &"env".into(), &env)?;

        // Compile and instantiate
        let module_promise = WebAssembly::instantiate_buffer(wasm_bytes, &imports);
        let result = wasm_bindgen_futures::JsFuture::from(module_promise).await?;

        // Extract the "run" function
        let instance = Reflect::get(&result, &"instance".into())?;
        let exports = Reflect::get(&instance, &"exports".into())?;
        let run_fn = Reflect::get(&exports, &"run".into())?;

        Ok(run_fn.dyn_into::<Function>()?)
    }

    /// Execute a JIT'd function.
    ///
    /// # Arguments
    /// * `run_fn` - The instantiated "run" function
    /// * `cpu_state_offset` - Offset of CPU state in shared memory
    /// * `base_pc` - Base PC of the block
    ///
    /// # Returns
    /// The execution result parsed from the function's return value.
    pub fn execute_jit_function(
        run_fn: &Function,
        cpu_state_offset: u32,
        base_pc: u64,
    ) -> JitExecResult {
        // Call the function with cpu_state_offset as argument
        let result = run_fn.call1(&JsValue::NULL, &JsValue::from(cpu_state_offset));

        match result {
            Ok(val) => {
                // Convert JS Number to i64
                // Note: JS numbers are f64, so we may lose precision for large values.
                // For proper BigInt support, we'd need additional handling.
                let ret_val = if let Some(n) = val.as_f64() {
                    n as i64
                } else {
                    // Handle BigInt or invalid return
                    // For now, treat as interpreter fallback
                    0i64
                };

                JitExecResult::from_i64(ret_val, base_pc)
            }
            Err(_) => {
                // JS exception during execution - fall back to interpreter
                JitExecResult::ExitToInterpreter { pc: base_pc }
            }
        }
    }

    /// Check if WASM JIT execution is available in this environment.
    pub fn is_available() -> bool {
        // In a WASM build, WebAssembly APIs are always available
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_i64_normal() {
        let base_pc = 0x8000_0000u64;
        let pc_offset = 16u32;
        let value = JitExecResult::make_continue(pc_offset);

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(result, JitExecResult::Continue(base_pc + pc_offset as u64));
    }

    #[test]
    fn test_from_i64_trap() {
        let base_pc = 0x8000_0000u64;
        let trap_code = 2u32; // IllegalInstruction
        let value = JitExecResult::make_trap(trap_code);

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(
            result,
            JitExecResult::Trap {
                code: trap_code,
                fault_pc: base_pc
            }
        );
    }

    #[test]
    fn test_from_i64_interpreter() {
        let base_pc = 0x8000_0000u64;
        let pc_offset = 4u32;
        let value = JitExecResult::make_interpreter(pc_offset);

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(
            result,
            JitExecResult::ExitToInterpreter {
                pc: base_pc + pc_offset as u64
            }
        );
    }

    #[test]
    fn test_from_i64_interrupt_check() {
        let base_pc = 0x8000_0000u64;
        let pc_offset = 8u32;
        let value = JitExecResult::make_interrupt_check(pc_offset);

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(
            result,
            JitExecResult::InterruptCheck {
                pc: base_pc + pc_offset as u64
            }
        );
    }

    #[test]
    fn test_from_i64_unknown_exit_code() {
        let base_pc = 0x8000_0000u64;
        // Unknown exit code (99)
        let value = ((99i64) << 32) | 0;

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(result, JitExecResult::ExitToInterpreter { pc: base_pc });
    }

    #[test]
    fn test_round_trip_continue() {
        let pc_offset = 100u32;
        let base_pc = 0x1000u64;

        let encoded = JitExecResult::make_continue(pc_offset);
        let decoded = JitExecResult::from_i64(encoded, base_pc);

        assert_eq!(
            decoded,
            JitExecResult::Continue(base_pc + pc_offset as u64)
        );
    }

    #[test]
    fn test_from_i64_branch() {
        let base_pc = 0x8000_0000u64;
        let new_pc = 0x8000_1000u32;
        let value = JitExecResult::make_branch(new_pc);

        let result = JitExecResult::from_i64(value, base_pc);
        assert_eq!(result, JitExecResult::Branch { new_pc: new_pc as u64 });
    }

    #[test]
    fn test_round_trip_branch() {
        let new_pc = 0x8000_2000u32;
        let base_pc = 0x8000_0000u64;

        let encoded = JitExecResult::make_branch(new_pc);
        let decoded = JitExecResult::from_i64(encoded, base_pc);

        assert_eq!(decoded, JitExecResult::Branch { new_pc: new_pc as u64 });
    }
}

