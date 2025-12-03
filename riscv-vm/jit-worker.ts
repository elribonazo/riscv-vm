/**
 * JIT Worker: Compiles RISC-V blocks to WASM in a separate thread.
 *
 * This worker receives compilation requests from the main thread,
 * calls into the Rust WASM module to compile MicroOp blocks to WASM,
 * and returns the compiled bytes.
 *
 * ## Architecture
 *
 * - Main thread identifies hot blocks and sends CompileRequest
 * - Worker deserializes request, calls JitCompiler, returns WASM bytes
 * - Main thread instantiates WASM module and caches for execution
 *
 * ## Communication Protocol
 *
 * Main → Worker: CompileRequestMessage (type: 'compile')
 * Worker → Main: ReadyMessage | CompileResultMessage | ErrorMessage
 */

// Import WASM as embedded buffer (converted to base64 by tsup wasmPlugin)
import wasmBuffer from "./pkg/riscv_vm_bg.wasm";
import { initSync } from "./pkg/riscv_vm.js";

// ============================================================================
// Message Types
// ============================================================================

/** Message sent from main thread to request compilation */
export interface CompileRequestMessage {
    type: 'compile';
    /** Block starting PC (number for u64 compatibility via f64) */
    pc: number;
    /** bincode-serialized CompileRequest from Rust */
    requestBytes: ArrayBuffer;
}

/** Message sent when worker is ready */
export interface ReadyMessage {
    type: 'ready';
}

/** Message sent with successful compilation result */
export interface CompileSuccessMessage {
    type: 'compiled';
    pc: number;
    wasmBytes: Uint8Array;
    compileTimeUs: number;
}

/** Message sent when compilation fails or block is unsuitable */
export interface CompileFailureMessage {
    type: 'compiled';
    pc: number;
    status: 'unsuitable' | 'error';
    error?: string;
    compileTimeUs?: number;
}

/** Message sent on initialization or runtime error */
export interface WorkerErrorMessage {
    type: 'error';
    error: string;
}

export type JitWorkerOutboundMessage =
    | ReadyMessage
    | CompileSuccessMessage
    | CompileFailureMessage
    | WorkerErrorMessage;

export type JitWorkerInboundMessage = CompileRequestMessage;

// ============================================================================
// Worker Context
// ============================================================================

// Worker global scope type (avoids needing WebWorker lib which conflicts with DOM)
interface WorkerGlobalScope {
    onmessage: ((event: MessageEvent<JitWorkerInboundMessage>) => void) | null;
    onerror: ((event: ErrorEvent) => void) | null;
    postMessage(message: JitWorkerOutboundMessage): void;
}

declare const self: WorkerGlobalScope;

// Track initialization state
let wasmReady = false;
let jitContext: JitWorkerContextInterface | null = null;

/**
 * Interface for the Rust-side JitWorkerContext.
 * This is exposed via wasm-bindgen from the Rust WASM module.
 */
interface JitWorkerContextInterface {
    /** Compile a block from serialized request bytes */
    compile(requestBytes: Uint8Array): JitCompileResult;
    /** Free resources */
    free(): void;
}

/**
 * Result returned from JitWorkerContext.compile()
 */
interface JitCompileResult {
    /** Whether compilation succeeded */
    success: boolean;
    /** Compiled WASM bytes (if success) */
    wasmBytes?: Uint8Array;
    /** Error message (if failed) */
    error?: string;
}

// Declare the JitWorkerContext class that will be imported from WASM
// This type declaration tells TypeScript about the class exported from Rust
declare class JitWorkerContext implements JitWorkerContextInterface {
    constructor();
    compile(requestBytes: Uint8Array): JitCompileResult;
    free(): void;
}

// ============================================================================
// WASM Initialization
// ============================================================================

/**
 * Initialize the WASM module and create JIT context.
 */
async function initWasm(): Promise<void> {
    try {
        // Initialize WASM module with embedded buffer
        const exports = initSync(wasmBuffer);
        
        // Check if JitWorkerContext is exported
        // The export will be available on the module exports after bindgen
        const wasmModule = await import("./pkg/riscv_vm.js");
        
        if (typeof wasmModule.JitWorkerContext === 'function') {
            jitContext = new wasmModule.JitWorkerContext();
            wasmReady = true;
            console.log('[JIT Worker] WASM initialized, JitWorkerContext ready');
        } else {
            // JitWorkerContext not yet implemented - use fallback
            console.warn('[JIT Worker] JitWorkerContext not available, compilation disabled');
            wasmReady = true; // Mark ready but without context
        }
        
        self.postMessage({ type: 'ready' });
    } catch (err) {
        const errorMsg = err instanceof Error ? err.message : String(err);
        console.error('[JIT Worker] WASM init failed:', errorMsg);
        self.postMessage({ 
            type: 'error', 
            error: `WASM initialization failed: ${errorMsg}` 
        });
    }
}

// Start initialization immediately
initWasm();

// ============================================================================
// Message Handler
// ============================================================================

self.onmessage = async (event: MessageEvent<JitWorkerInboundMessage>) => {
    const msg = event.data;
    
    // Ignore messages from browser extensions (React DevTools, etc.)
    if (!msg || typeof msg !== 'object' || 'source' in msg) {
        return;
    }
    
    if (msg.type === 'compile') {
        await handleCompileRequest(msg);
    }
};

/**
 * Handle a compilation request from the main thread.
 */
async function handleCompileRequest(msg: CompileRequestMessage): Promise<void> {
    const startTime = performance.now();
    
    if (!wasmReady) {
        self.postMessage({
            type: 'compiled',
            pc: msg.pc,
            status: 'error',
            error: 'WASM not initialized',
        });
        return;
    }
    
    if (!jitContext) {
        // JitWorkerContext not available - report as unsuitable
        self.postMessage({
            type: 'compiled',
            pc: msg.pc,
            status: 'unsuitable',
            error: 'JIT compilation not available',
            compileTimeUs: 0,
        });
        return;
    }
    
    try {
        // Convert ArrayBuffer to Uint8Array for Rust
        const requestBytes = new Uint8Array(msg.requestBytes);
        
        // Call Rust JIT compiler
        const result = jitContext.compile(requestBytes);
        
        const endTime = performance.now();
        const compileTimeUs = Math.floor((endTime - startTime) * 1000);
        
        if (result.success && result.wasmBytes) {
            self.postMessage({
                type: 'compiled',
                pc: msg.pc,
                wasmBytes: result.wasmBytes,
                compileTimeUs,
            });
        } else {
            self.postMessage({
                type: 'compiled',
                pc: msg.pc,
                status: 'unsuitable',
                error: result.error,
                compileTimeUs,
            });
        }
    } catch (err) {
        const endTime = performance.now();
        const compileTimeUs = Math.floor((endTime - startTime) * 1000);
        const errorMsg = err instanceof Error ? err.message : String(err);
        
        console.error(`[JIT Worker] Compilation error for PC 0x${msg.pc.toString(16)}:`, errorMsg);
        
        self.postMessage({
            type: 'compiled',
            pc: msg.pc,
            status: 'error',
            error: errorMsg,
            compileTimeUs,
        });
    }
}

// ============================================================================
// Error Handler
// ============================================================================

self.onerror = (e: ErrorEvent) => {
    console.error('[JIT Worker] Uncaught error:', e);
    self.postMessage({
        type: 'error',
        error: e.message || String(e),
    });
};

