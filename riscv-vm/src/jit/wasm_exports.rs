//! WASM-bindgen exports for JIT worker context.
//!
//! This module exposes the JIT compiler to Web Workers, allowing JavaScript
//! to call into Rust for block compilation. The worker receives serialized
//! `CompileRequest` messages, compiles them, and returns JavaScript objects
//! with the compilation results.

use std::cell::RefCell;

use js_sys::{Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

use super::compiler::JitCompiler;
use super::types::{CompilationResult, JitConfig};
use super::worker::CompileRequest;
use crate::engine::block::Block;

/// JIT compilation context for Web Worker.
///
/// This struct wraps the `JitCompiler` and provides a WASM-bindgen interface
/// for JavaScript to request block compilation.
#[wasm_bindgen]
pub struct JitWorkerContext {
    compiler: RefCell<JitCompiler>,
}

#[wasm_bindgen]
impl JitWorkerContext {
    /// Create a new JIT worker context with default configuration.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            compiler: RefCell::new(JitCompiler::new(JitConfig::default())),
        }
    }

    /// Create a new JIT worker context with custom configuration.
    ///
    /// # Arguments
    /// * `min_block_size` - Minimum number of instructions for JIT compilation
    /// * `debug_wat` - Enable debug WAT output
    #[wasm_bindgen(js_name = "newWithConfig")]
    pub fn new_with_config(min_block_size: usize, debug_wat: bool) -> Self {
        let config = JitConfig {
            min_block_size,
            debug_wat,
            ..JitConfig::default()
        };
        Self {
            compiler: RefCell::new(JitCompiler::new(config)),
        }
    }

    /// Compile a block from serialized request bytes.
    ///
    /// # Arguments
    /// * `request_bytes` - Bincode-serialized `CompileRequest`
    ///
    /// # Returns
    /// A JavaScript object with the compilation result:
    /// - On success: `{ success: true, pc: number, wasmBytes: Uint8Array }`
    /// - On unsuitable: `{ success: false, pc: number, status: 'unsuitable' }`
    /// - On error: `{ success: false, pc: number, status: 'error' }`
    pub fn compile(&self, request_bytes: &[u8]) -> JsValue {
        // Deserialize request
        let request: CompileRequest = match bincode::deserialize(request_bytes) {
            Ok(r) => r,
            Err(_) => {
                return self.make_error_response(0);
            }
        };

        // Reconstruct Block from serialized ops
        let block = match self.reconstruct_block(&request) {
            Some(b) => b,
            None => return self.make_error_response(request.pc),
        };

        // Compile the block
        let result = self.compiler.borrow_mut().compile(&block);

        match result {
            CompilationResult::Success(wasm_bytes) => {
                self.make_success_response(request.pc, wasm_bytes)
            }
            CompilationResult::Unsuitable => self.make_unsuitable_response(request.pc),
            CompilationResult::Error(_) => self.make_error_response(request.pc),
        }
    }

    /// Reconstruct a Block from a CompileRequest.
    fn reconstruct_block(&self, request: &CompileRequest) -> Option<Block> {
        let mut block = Block::new(request.pc, request.pa, 0);
        block.byte_len = request.byte_len;

        for serialized_op in &request.ops {
            if let Some(op) = serialized_op.to_microop() {
                // Calculate instruction length from the serialized op
                // Most instructions are 4 bytes, compressed are 2 bytes
                // For now, we estimate based on block byte length / op count
                let avg_insn_len = if request.ops.is_empty() {
                    4
                } else {
                    (request.byte_len as usize / request.ops.len()).max(2).min(4) as u8
                };
                block.push(op, avg_insn_len);
            } else {
                // Failed to deserialize a MicroOp
                return None;
            }
        }

        Some(block)
    }

    /// Create a success response with compiled WASM bytes.
    fn make_success_response(&self, pc: u64, wasm_bytes: Vec<u8>) -> JsValue {
        let obj = Object::new();

        // Set success flag
        Reflect::set(&obj, &"success".into(), &JsValue::TRUE).ok();

        // Set PC (using f64 since JS doesn't have native u64)
        Reflect::set(&obj, &"pc".into(), &JsValue::from(pc as f64)).ok();

        // Set WASM bytes as Uint8Array
        let arr = Uint8Array::new_with_length(wasm_bytes.len() as u32);
        arr.copy_from(&wasm_bytes);
        Reflect::set(&obj, &"wasmBytes".into(), &arr).ok();

        obj.into()
    }

    /// Create an unsuitable response (block cannot be JIT'd).
    fn make_unsuitable_response(&self, pc: u64) -> JsValue {
        let obj = Object::new();

        Reflect::set(&obj, &"success".into(), &JsValue::FALSE).ok();
        Reflect::set(&obj, &"pc".into(), &JsValue::from(pc as f64)).ok();
        Reflect::set(&obj, &"status".into(), &"unsuitable".into()).ok();

        obj.into()
    }

    /// Create an error response (deserialization or compilation error).
    fn make_error_response(&self, pc: u64) -> JsValue {
        let obj = Object::new();

        Reflect::set(&obj, &"success".into(), &JsValue::FALSE).ok();
        Reflect::set(&obj, &"pc".into(), &JsValue::from(pc as f64)).ok();
        Reflect::set(&obj, &"status".into(), &"error".into()).ok();

        obj.into()
    }
}

impl Default for JitWorkerContext {
    fn default() -> Self {
        Self::new()
    }
}

