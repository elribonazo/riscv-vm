/**
 * Web Worker entry point for RISC-V hart.
 *
 * This worker runs a secondary RISC-V hart (1, 2, 3, ...) sharing memory
 * with the main thread via SharedArrayBuffer.
 *
 * ## Architecture
 *
 * - Workers run secondary harts (1+)
 * - Hart 0 runs on main thread (handles I/O)
 * - Shared via SharedArrayBuffer:
 *   - Control region (halt flags)
 *   - CLINT (timer, IPI)
 *   - DRAM (kernel memory)
 *
 * ## Communication Protocol
 *
 * Main → Worker: WorkerInitMessage
 * Worker → Main: WorkerReadyMessage | WorkerHaltedMessage | WorkerErrorMessage
 */

// Import WASM as embedded buffer (converted to base64 by tsup wasmPlugin)
import wasmBuffer from "./pkg/riscv_vm_bg.wasm";
import { initSync, WorkerState } from "./pkg/riscv_vm.js";

// WorkerStepResult enum values (must match worker.rs)
// wasm-bindgen exports enums as numeric values
const WorkerStepResult = {
  Continue: 0,
  Halted: 1,
  Shutdown: 2,
  Error: 3,
} as const;

// ============================================================================
// Message Types
// ============================================================================

/** Message sent from main thread to initialize the worker */
export interface WorkerInitMessage {
  hartId: number;
  /** SharedArrayBuffer containing control + CLINT + DRAM */
  sharedMem: SharedArrayBuffer;
  entryPc: number;
}

/** Message sent when worker is ready to execute */
export interface WorkerReadyMessage {
  type: "ready";
  hartId: number;
}

/** Message sent when worker has halted */
export interface WorkerHaltedMessage {
  type: "halted";
  hartId: number;
  stepCount?: number;
}

/** Message sent when an error occurs */
export interface WorkerErrorMessage {
  type: "error";
  hartId?: number;
  error: string;
}

export type WorkerOutboundMessage =
  | WorkerReadyMessage
  | WorkerHaltedMessage
  | WorkerErrorMessage;

// ============================================================================
// Shared Memory Layout (must match shared_mem.rs)
// ============================================================================

/** Control region offsets (i32 indices) */
const CTRL_HALT_REQUESTED = 0;
const CTRL_HALTED = 1;

/** Shared control view for Atomics operations */
let controlView: Int32Array | null = null;

// ============================================================================
// Worker Context
// ============================================================================

// Worker global scope type (avoids needing WebWorker lib which conflicts with DOM)
interface WorkerGlobalScope {
  onmessage: ((event: MessageEvent<WorkerInitMessage>) => void) | null;
  onerror: ((event: ErrorEvent) => void) | null;
  postMessage(message: WorkerOutboundMessage): void;
}

declare const self: WorkerGlobalScope;

let initialized = false;
let currentHartId: number | undefined;
let workerState: WorkerState | null = null;

// Tuning parameters for execution
// Performance Note: Higher BATCH_SIZE reduces JS/WASM boundary crossing overhead.
// Combined with BATCHES_PER_YIELD, this determines how often we check for halt signals.
const BATCH_SIZE = 100_000;      // Instructions per batch (was 1024)

/**
 * Run loop using optimized blocking execution with periodic yields.
 * 
 * This replaces the previous setTimeout-based approach which had significant
 * overhead (~4ms minimum delay per batch in browsers). Instead, we:
 * 1. Execute multiple batches in a tight loop
 * 2. Use Atomics.wait with 0ms timeout to efficiently yield
 * 3. Only check for halt signals periodically
 * 
 * This significantly reduces scheduling overhead while still allowing
 * the worker to respond to external signals.
 */
function runLoop() {
  if (!workerState || currentHartId === undefined || !controlView) {
    console.error('[Worker] runLoop called without workerState, hartId, or controlView');
    return;
  }
  
  const hartId = currentHartId; // Capture for type narrowing
  
  // Number of batches to execute before yielding
  const BATCHES_PER_YIELD = 10;
  
  let shouldContinue = true;
  let batchCount = 0;
  
  while (shouldContinue) {
    const result = workerState.step_batch(BATCH_SIZE);
    
    switch (result) {
      case WorkerStepResult.Continue:
        batchCount++;
        
        // Yield periodically to allow halt signals to be processed
        if (batchCount >= BATCHES_PER_YIELD) {
          batchCount = 0;
          
          // Use Atomics.wait with 0ms timeout for efficient yielding
          // This is much faster than setTimeout(0) which has ~4ms minimum delay
          // Returns immediately but allows the thread to check for updates
          try {
            Atomics.wait(controlView, CTRL_HALT_REQUESTED, 0, 0);
          } catch {
            // Atomics.wait may not be available in all contexts, fall back gracefully
          }
        }
        break;
        
      case WorkerStepResult.Halted:
        console.log(`[Worker ${hartId}] Halted after ${workerState.step_count()} steps`);
        self.postMessage({ type: "halted", hartId });
        cleanup();
        shouldContinue = false;
        break;
        
      case WorkerStepResult.Shutdown:
        console.log(`[Worker ${hartId}] Shutdown after ${workerState.step_count()} steps`);
        self.postMessage({ type: "halted", hartId });
        cleanup();
        shouldContinue = false;
        break;
        
      case WorkerStepResult.Error:
        console.error(`[Worker ${hartId}] Error after ${workerState.step_count()} steps`);
        self.postMessage({ type: "error", hartId, error: "Execution error" });
        cleanup();
        shouldContinue = false;
        break;
    }
  }
}

function cleanup() {
  workerState = null;
  controlView = null;
}

self.onmessage = async (event: MessageEvent<WorkerInitMessage>) => {
  const data = event.data;
  
  // Ignore messages from browser extensions (React DevTools, etc.)
  if (!data || typeof data !== 'object' || 'source' in data) {
    return;
  }
  
  const { hartId, sharedMem, entryPc } = data;
  
  // Validate required fields
  if (hartId === undefined || !sharedMem || entryPc === undefined) {
    console.warn('[Worker] Invalid init message, missing fields');
    return;
  }
  
  currentHartId = hartId;
  console.log(`[Worker ${hartId}] Received init message`);

  if (!initialized) {
    try {
      // Initialize WASM module with embedded buffer
      console.log(`[Worker ${hartId}] Initializing WASM...`);
      initSync(wasmBuffer);
      initialized = true;
      console.log(`[Worker ${hartId}] WASM initialized`);
    } catch (e) {
      console.error(`[Worker ${hartId}] WASM init failed:`, e);
      const msg: WorkerErrorMessage = {
        type: "error",
        hartId,
        error: String(e),
      };
      self.postMessage(msg);
      return;
    }
  }

  // Verify SharedArrayBuffer
  if (!(sharedMem instanceof SharedArrayBuffer)) {
    const msg: WorkerErrorMessage = {
      type: "error",
      hartId,
      error: "sharedMem must be a SharedArrayBuffer",
    };
    self.postMessage(msg);
    return;
  }

  // Notify main thread that we're ready
  const readyMsg: WorkerReadyMessage = { type: "ready", hartId };
  self.postMessage(readyMsg);

  try {
    // Convert entryPc (number/float64) to BigInt for u64
    const pc = BigInt(Math.floor(entryPc));
    console.log(`[Worker ${hartId}] Starting execution at PC=0x${pc.toString(16)}`);
    
    // Set up control view for efficient Atomics.wait-based yielding
    controlView = new Int32Array(sharedMem);
    
    // Create worker state for cooperative scheduling
    workerState = new WorkerState(hartId, sharedMem, pc);
    
    // Start the optimized blocking run loop
    runLoop();
  } catch (e) {
    console.error(`[Worker ${hartId}] Execution error:`, e);
    const msg: WorkerErrorMessage = {
      type: "error",
      hartId,
      error: String(e),
    };
    self.postMessage(msg);
    cleanup();
    return;
  }
};

self.onerror = (e: ErrorEvent) => {
  console.error("[Worker] Uncaught error:", e);
  const msg: WorkerErrorMessage = {
    type: "error",
    hartId: currentHartId,
    error: e.message || String(e),
  };
  self.postMessage(msg);
};

// ============================================================================
// Utility: Check if halt was requested (can be called from JS if needed)
// ============================================================================

/**
 * Check if halt has been requested in the shared control region.
 * This can be used for JS-side polling if needed.
 */
export function isHaltRequested(sharedMem: SharedArrayBuffer): boolean {
  const view = new Int32Array(sharedMem);
  return Atomics.load(view, CTRL_HALT_REQUESTED) !== 0;
}

/**
 * Request halt by setting the flag in shared memory.
 * This can be called from any thread.
 */
export function requestHalt(sharedMem: SharedArrayBuffer): void {
  const view = new Int32Array(sharedMem);
  Atomics.store(view, CTRL_HALT_REQUESTED, 1);
  Atomics.notify(view, CTRL_HALT_REQUESTED);
}

/**
 * Check if VM has halted.
 */
export function isHalted(sharedMem: SharedArrayBuffer): boolean {
  const view = new Int32Array(sharedMem);
  return Atomics.load(view, CTRL_HALTED) !== 0;
}
