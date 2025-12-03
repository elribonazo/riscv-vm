//! WASM Binary Encoder for JIT'd Blocks
//!
//! This module uses `wasm-encoder` to emit valid WASM binary modules
//! containing JIT-compiled RISC-V blocks.

use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    ImportSection, Instruction, MemoryType, Module, TypeSection, ValType,
};

/// Import function indices for use in generated code.
///
/// After adding imports, function indices are assigned as follows:
/// - Memory import is index 0 (not a callable function)
/// - Imported functions get indices 0-7
/// - Our generated function gets the next index (8)
pub mod imports {
    /// Index of read_u64 import function.
    pub const READ_U64: u32 = 0;

    /// Index of read_u32 import function.
    pub const READ_U32: u32 = 1;

    /// Index of read_u16 import function.
    pub const READ_U16: u32 = 2;

    /// Index of read_u8 import function.
    pub const READ_U8: u32 = 3;

    /// Index of write_u64 import function.
    pub const WRITE_U64: u32 = 4;

    /// Index of write_u32 import function.
    pub const WRITE_U32: u32 = 5;

    /// Index of write_u16 import function.
    pub const WRITE_U16: u32 = 6;

    /// Index of write_u8 import function.
    pub const WRITE_U8: u32 = 7;

    /// Index of our generated function (first function after imports).
    pub const RUN_FUNC: u32 = 8;
}

/// WASM module builder for a single JIT'd block.
pub struct WasmModuleBuilder {
    /// Function body instructions
    instructions: Vec<Instruction<'static>>,
    /// Number of local variables (beyond parameters)
    num_locals: u32,
    /// Local variable types
    local_types: Vec<(u32, ValType)>,
}

impl WasmModuleBuilder {
    /// Create a new module builder.
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            num_locals: 0,
            local_types: Vec::new(),
        }
    }

    /// Add a local variable and return its index.
    /// Parameter is index 0 (cpu_state_ptr), so locals start at 1.
    pub fn add_local(&mut self, ty: ValType) -> u32 {
        let idx = 1 + self.num_locals; // 0 is the parameter
        self.num_locals += 1;
        
        // Merge with existing type if possible
        if let Some((count, last_ty)) = self.local_types.last_mut() {
            if *last_ty == ty {
                *count += 1;
                return idx;
            }
        }
        self.local_types.push((1, ty));
        idx
    }

    /// Emit an instruction.
    pub fn emit(&mut self, insn: Instruction<'static>) {
        self.instructions.push(insn);
    }

    /// Emit multiple instructions.
    pub fn emit_all(&mut self, insns: impl IntoIterator<Item = Instruction<'static>>) {
        self.instructions.extend(insns);
    }

    /// Get or create a local variable of the given type.
    /// Returns the local index (parameter is 0, locals start at 1).
    pub fn get_or_create_local(&mut self, ty: ValType) -> u32 {
        // For now, just create a new local each time
        // Could optimize to reuse locals of same type
        self.add_local(ty)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Memory Access Helpers
    // ═══════════════════════════════════════════════════════════════════════════

    /// Emit a call to the read_u64 import.
    /// Assumes vaddr (i64) is on the stack.
    /// Leaves result (i64) on the stack.
    pub fn emit_read_u64(&mut self) {
        self.emit(Instruction::Call(imports::READ_U64));
    }

    /// Emit a call to the read_u32 import.
    /// Assumes vaddr (i64) is on the stack.
    /// Leaves result (i32) on the stack.
    pub fn emit_read_u32(&mut self) {
        self.emit(Instruction::Call(imports::READ_U32));
    }

    /// Emit a call to the read_u16 import.
    /// Assumes vaddr (i64) is on the stack.
    /// Leaves result (i32) on the stack.
    pub fn emit_read_u16(&mut self) {
        self.emit(Instruction::Call(imports::READ_U16));
    }

    /// Emit a call to the read_u8 import.
    /// Assumes vaddr (i64) is on the stack.
    /// Leaves result (i32) on the stack.
    pub fn emit_read_u8(&mut self) {
        self.emit(Instruction::Call(imports::READ_U8));
    }

    /// Emit a call to the write_u64 import.
    /// Assumes vaddr (i64) and value (i64) are on the stack.
    /// Leaves result code (i32) on the stack (0 = success).
    pub fn emit_write_u64(&mut self) {
        self.emit(Instruction::Call(imports::WRITE_U64));
    }

    /// Emit a call to the write_u32 import.
    /// Assumes vaddr (i64) and value (i32) are on the stack.
    /// Leaves result code (i32) on the stack (0 = success).
    pub fn emit_write_u32(&mut self) {
        self.emit(Instruction::Call(imports::WRITE_U32));
    }

    /// Emit a call to the write_u16 import.
    /// Assumes vaddr (i64) and value (i32) are on the stack.
    /// Leaves result code (i32) on the stack (0 = success).
    pub fn emit_write_u16(&mut self) {
        self.emit(Instruction::Call(imports::WRITE_U16));
    }

    /// Emit a call to the write_u8 import.
    /// Assumes vaddr (i64) and value (i32) are on the stack.
    /// Leaves result code (i32) on the stack (0 = success).
    pub fn emit_write_u8(&mut self) {
        self.emit(Instruction::Call(imports::WRITE_U8));
    }

    /// Emit a direct memory load from shared memory (i64).
    /// This bypasses MMU translation - only use for known-safe regions
    /// like the JIT state area.
    /// Assumes base address (i32) is on the stack.
    pub fn emit_direct_load_i64(&mut self, offset: u64) {
        self.emit(Instruction::I64Load(wasm_encoder::MemArg {
            offset,
            align: 3, // 8-byte alignment
            memory_index: 0,
        }));
    }

    /// Emit a direct memory load from shared memory (i32).
    /// Assumes base address (i32) is on the stack.
    pub fn emit_direct_load_i32(&mut self, offset: u64) {
        self.emit(Instruction::I32Load(wasm_encoder::MemArg {
            offset,
            align: 2, // 4-byte alignment
            memory_index: 0,
        }));
    }

    /// Emit a direct memory store to shared memory (i64).
    /// This bypasses MMU translation - only use for known-safe regions.
    /// Assumes base address (i32) and value (i64) are on the stack.
    pub fn emit_direct_store_i64(&mut self, offset: u64) {
        self.emit(Instruction::I64Store(wasm_encoder::MemArg {
            offset,
            align: 3, // 8-byte alignment
            memory_index: 0,
        }));
    }

    /// Emit a direct memory store to shared memory (i32).
    /// Assumes base address (i32) and value (i32) are on the stack.
    pub fn emit_direct_store_i32(&mut self, offset: u64) {
        self.emit(Instruction::I32Store(wasm_encoder::MemArg {
            offset,
            align: 2, // 4-byte alignment
            memory_index: 0,
        }));
    }

    /// Build the final WASM module bytes (simple version, no helper imports).
    ///
    /// The generated module has:
    /// - 1 import: memory from "env"
    /// - 1 function: execute_block(cpu_state_ptr: i32) -> i64
    /// - 1 export: the function as "run"
    pub fn build(self) -> Vec<u8> {
        let mut module = Module::new();

        // Type section: function signature
        // (cpu_state_ptr: i32) -> i64
        let mut types = TypeSection::new();
        types.ty().function(
            vec![ValType::I32],  // params
            vec![ValType::I64],  // results
        );
        module.section(&types);

        // Import section: import memory from "env"
        let mut imports = ImportSection::new();
        imports.import(
            "env",
            "memory",
            MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: true, // SharedArrayBuffer
                page_size_log2: None,
            },
        );
        module.section(&imports);

        // Function section: declare function 0 uses type 0
        let mut functions = FunctionSection::new();
        functions.function(0); // type index 0
        module.section(&functions);

        // Export section: export the function as "run"
        let mut exports = ExportSection::new();
        exports.export("run", ExportKind::Func, 0);
        module.section(&exports);

        // Code section: function body
        let mut codes = CodeSection::new();
        let mut func = Function::new(self.local_types);
        for insn in self.instructions {
            func.instruction(&insn);
        }
        func.instruction(&Instruction::End);
        codes.function(&func);
        module.section(&codes);

        module.finish()
    }

    /// Build the final WASM module bytes with memory helper imports.
    ///
    /// This version imports helper functions for MMU-translated memory access.
    /// Use this when the block contains load/store operations.
    ///
    /// The generated module has:
    /// - Memory import from "env"
    /// - 8 helper function imports (read_u64/32/16/8, write_u64/32/16/8)
    /// - 1 function: execute_block(cpu_state_ptr: i32) -> i64
    /// - 1 export: the function as "run"
    pub fn build_with_imports(self) -> Vec<u8> {
        let mut module = Module::new();

        // ═══════════════════════════════════════════════════════════════════
        // Type Section - Function Signatures
        // ═══════════════════════════════════════════════════════════════════
        let mut types = TypeSection::new();

        // Type 0: Main function (cpu_state_ptr: i32) -> i64
        types.ty().function(vec![ValType::I32], vec![ValType::I64]);

        // Type 1: read_u64(vaddr: i64) -> i64
        types.ty().function(vec![ValType::I64], vec![ValType::I64]);

        // Type 2: read_u32(vaddr: i64) -> i32
        types.ty().function(vec![ValType::I64], vec![ValType::I32]);

        // Type 3: read_u16(vaddr: i64) -> i32
        types.ty().function(vec![ValType::I64], vec![ValType::I32]);

        // Type 4: read_u8(vaddr: i64) -> i32
        types.ty().function(vec![ValType::I64], vec![ValType::I32]);

        // Type 5: write_u64(vaddr: i64, value: i64) -> i32
        types
            .ty()
            .function(vec![ValType::I64, ValType::I64], vec![ValType::I32]);

        // Type 6: write_u32(vaddr: i64, value: i32) -> i32
        types
            .ty()
            .function(vec![ValType::I64, ValType::I32], vec![ValType::I32]);

        // Type 7: write_u16(vaddr: i64, value: i32) -> i32
        types
            .ty()
            .function(vec![ValType::I64, ValType::I32], vec![ValType::I32]);

        // Type 8: write_u8(vaddr: i64, value: i32) -> i32
        types
            .ty()
            .function(vec![ValType::I64, ValType::I32], vec![ValType::I32]);

        module.section(&types);

        // ═══════════════════════════════════════════════════════════════════
        // Import Section
        // ═══════════════════════════════════════════════════════════════════
        let mut import_section = ImportSection::new();

        // Import 0: shared memory
        import_section.import(
            "env",
            "memory",
            MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: true, // SharedArrayBuffer
                page_size_log2: None,
            },
        );

        // Import helper functions
        // Function index 0: read_u64 (type 1)
        import_section.import("env", "read_u64", EntityType::Function(1));

        // Function index 1: read_u32 (type 2)
        import_section.import("env", "read_u32", EntityType::Function(2));

        // Function index 2: read_u16 (type 3)
        import_section.import("env", "read_u16", EntityType::Function(3));

        // Function index 3: read_u8 (type 4)
        import_section.import("env", "read_u8", EntityType::Function(4));

        // Function index 4: write_u64 (type 5)
        import_section.import("env", "write_u64", EntityType::Function(5));

        // Function index 5: write_u32 (type 6)
        import_section.import("env", "write_u32", EntityType::Function(6));

        // Function index 6: write_u16 (type 7)
        import_section.import("env", "write_u16", EntityType::Function(7));

        // Function index 7: write_u8 (type 8)
        import_section.import("env", "write_u8", EntityType::Function(8));

        module.section(&import_section);

        // ═══════════════════════════════════════════════════════════════════
        // Function Section
        // ═══════════════════════════════════════════════════════════════════
        let mut functions = FunctionSection::new();
        // Our main function uses type 0
        // It gets function index 8 (after 8 imported functions)
        functions.function(0);
        module.section(&functions);

        // ═══════════════════════════════════════════════════════════════════
        // Export Section
        // ═══════════════════════════════════════════════════════════════════
        let mut exports = ExportSection::new();
        // Export our function (index 8) as "run"
        exports.export("run", ExportKind::Func, imports::RUN_FUNC);
        module.section(&exports);

        // ═══════════════════════════════════════════════════════════════════
        // Code Section
        // ═══════════════════════════════════════════════════════════════════
        let mut codes = CodeSection::new();
        let mut func = Function::new(self.local_types);
        for insn in self.instructions {
            func.instruction(&insn);
        }
        func.instruction(&Instruction::End);
        codes.function(&func);
        module.section(&codes);

        module.finish()
    }
}

impl Default for WasmModuleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_module() {
        let mut builder = WasmModuleBuilder::new();
        
        // Minimal function: return 0
        builder.emit(Instruction::I64Const(0));
        
        let bytes = builder.build();
        
        // Check WASM magic bytes
        assert_eq!(&bytes[0..4], b"\x00asm");
        // Check version (1)
        assert_eq!(&bytes[4..8], &[0x01, 0x00, 0x00, 0x00]);
        
        println!("Generated {} bytes of WASM", bytes.len());
    }

    #[test]
    fn test_with_locals() {
        let mut builder = WasmModuleBuilder::new();

        // Add some locals
        let local_a = builder.add_local(ValType::I64);
        let local_b = builder.add_local(ValType::I64);

        assert_eq!(local_a, 1); // After parameter
        assert_eq!(local_b, 2);

        // Store a value in local_a
        builder.emit(Instruction::I64Const(42));
        builder.emit(Instruction::LocalSet(local_a));

        // Return local_a
        builder.emit(Instruction::LocalGet(local_a));

        let bytes = builder.build();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn test_build_with_imports() {
        let mut builder = WasmModuleBuilder::new();

        // Simulate a load operation:
        // 1. Push virtual address
        builder.emit(Instruction::I64Const(0x8000_0000));
        // 2. Call read_u64 helper
        builder.emit_read_u64();
        // 3. Return the loaded value

        let bytes = builder.build_with_imports();

        // Check WASM magic bytes
        assert_eq!(&bytes[0..4], b"\x00asm");
        // Check version (1)
        assert_eq!(&bytes[4..8], &[0x01, 0x00, 0x00, 0x00]);

        println!("Generated {} bytes of WASM with imports", bytes.len());
        // Should be larger than simple module due to imports
        assert!(bytes.len() > 50);
    }

    #[test]
    fn test_memory_helper_emit() {
        let mut builder = WasmModuleBuilder::new();

        // Test all helper emitters compile correctly
        builder.emit(Instruction::I64Const(0x1000));
        builder.emit_read_u64();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x1004));
        builder.emit_read_u32();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x1006));
        builder.emit_read_u16();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x1007));
        builder.emit_read_u8();
        builder.emit(Instruction::Drop);

        // Writes
        builder.emit(Instruction::I64Const(0x2000));
        builder.emit(Instruction::I64Const(0xDEADBEEF));
        builder.emit_write_u64();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x2008));
        builder.emit(Instruction::I32Const(0x1234));
        builder.emit_write_u32();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x200C));
        builder.emit(Instruction::I32Const(0x5678));
        builder.emit_write_u16();
        builder.emit(Instruction::Drop);

        builder.emit(Instruction::I64Const(0x200E));
        builder.emit(Instruction::I32Const(0xAB));
        builder.emit_write_u8();
        builder.emit(Instruction::Drop);

        // Return success
        builder.emit(Instruction::I64Const(0));

        let bytes = builder.build_with_imports();
        assert!(!bytes.is_empty());
        println!("Memory helper test module: {} bytes", bytes.len());
    }

    #[test]
    fn test_direct_memory_ops() {
        let mut builder = WasmModuleBuilder::new();

        // Direct load from JIT state area
        builder.emit(Instruction::I32Const(0)); // base address
        builder.emit_direct_load_i64(0x13000); // JIT_STATE_OFFSET

        // Direct store
        builder.emit(Instruction::I32Const(0));
        builder.emit(Instruction::I64Const(42));
        builder.emit_direct_store_i64(0x13008);

        // Direct i32 ops
        builder.emit(Instruction::I32Const(0));
        builder.emit_direct_load_i32(0x13200); // TRAP_PENDING offset

        builder.emit(Instruction::I32Const(0));
        builder.emit(Instruction::I32Const(1));
        builder.emit_direct_store_i32(0x13200);

        // Return
        builder.emit(Instruction::I64Const(0));

        let bytes = builder.build_with_imports();
        assert!(!bytes.is_empty());
    }
}

