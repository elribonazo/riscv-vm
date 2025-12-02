/**
 * Node.js Worker Thread entry point for RISC-V hart.
 *
 * This worker runs a secondary RISC-V hart (1, 2, 3, ...) sharing memory
 * with the main thread via SharedArrayBuffer using Node.js worker_threads.
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
 * Main → Worker: workerData (hartId, sharedMem, entryPc, wasmPath)
 * Worker → Main: WorkerReadyMessage | WorkerHaltedMessage | WorkerErrorMessage
 */

import { parentPort, workerData } from 'node:worker_threads';
import fs from 'node:fs';
import path from 'node:path';

// WorkerStepResult enum values (must match worker.rs)
const WorkerStepResult = {
  Continue: 0,
  Halted: 1,
  Shutdown: 2,
  Error: 3,
} as const;

// ============================================================================
// Message Types
// ============================================================================

interface WorkerData {
  hartId: number;
  sharedMem: SharedArrayBuffer;
  entryPc: number;
  wasmPath: string;
}

interface WorkerReadyMessage {
  type: "ready";
  hartId: number;
}

interface WorkerHaltedMessage {
  type: "halted";
  hartId: number;
  stepCount?: number;
}

interface WorkerErrorMessage {
  type: "error";
  hartId?: number;
  error: string;
}

type WorkerOutboundMessage =
  | WorkerReadyMessage
  | WorkerHaltedMessage
  | WorkerErrorMessage;

// ============================================================================
// Shared Memory Layout (must match shared_mem.rs)
// ============================================================================

const CTRL_HALT_REQUESTED = 0;

// ============================================================================
// Worker Context
// ============================================================================

let controlView: Int32Array | null = null;
let workerState: any = null;

// Tuning parameters for execution
const BATCH_SIZE = 100_000;
const BATCHES_PER_YIELD = 10;

function postMessage(msg: WorkerOutboundMessage): void {
  parentPort?.postMessage(msg);
}

/**
 * Run loop using optimized blocking execution with periodic yields.
 */
function runLoop(hartId: number): void {
  if (!workerState || !controlView) {
    console.error('[Worker] runLoop called without workerState or controlView');
    return;
  }

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
          try {
            Atomics.wait(controlView, CTRL_HALT_REQUESTED, 0, 0);
          } catch {
            // Fall back gracefully
          }
        }
        break;

      case WorkerStepResult.Halted:
        console.log(`[Worker ${hartId}] Halted after ${workerState.step_count()} steps`);
        postMessage({ type: "halted", hartId, stepCount: Number(workerState.step_count()) });
        cleanup();
        shouldContinue = false;
        break;

      case WorkerStepResult.Shutdown:
        console.log(`[Worker ${hartId}] Shutdown after ${workerState.step_count()} steps`);
        postMessage({ type: "halted", hartId, stepCount: Number(workerState.step_count()) });
        cleanup();
        shouldContinue = false;
        break;

      case WorkerStepResult.Error:
        console.error(`[Worker ${hartId}] Error after ${workerState.step_count()} steps`);
        postMessage({ type: "error", hartId, error: "Execution error" });
        cleanup();
        shouldContinue = false;
        break;
    }
  }
}

function cleanup(): void {
  workerState = null;
  controlView = null;
}

async function main(): Promise<void> {
  const { hartId, sharedMem, entryPc, wasmPath } = workerData as WorkerData;

  console.log(`[Worker ${hartId}] Starting with WASM from ${wasmPath}`);

  try {
    // Load WASM module
    const wasmBytes = fs.readFileSync(wasmPath);
    
    // Load the JS bindings
    const jsPath = wasmPath.replace('_bg.wasm', '.js');
    const wasmModule = await import(jsPath);
    
    // Initialize WASM with the bytes
    wasmModule.initSync(wasmBytes);
    
    console.log(`[Worker ${hartId}] WASM initialized`);

    // Verify SharedArrayBuffer
    if (!(sharedMem instanceof SharedArrayBuffer)) {
      throw new Error("sharedMem must be a SharedArrayBuffer");
    }

    // Notify main thread that we're ready
    postMessage({ type: "ready", hartId });

    // Convert entryPc to BigInt for u64
    const pc = BigInt(Math.floor(entryPc));
    console.log(`[Worker ${hartId}] Starting execution at PC=0x${pc.toString(16)}`);

    // Set up control view for Atomics
    controlView = new Int32Array(sharedMem);

    // Create worker state
    workerState = new wasmModule.WorkerState(hartId, sharedMem, pc);

    // Start the run loop
    runLoop(hartId);
  } catch (e) {
    console.error(`[Worker ${hartId}] Error:`, e);
    postMessage({
      type: "error",
      hartId,
      error: String(e),
    });
  }
}

// Start the worker
main().catch((e) => {
  console.error('[Worker] Fatal error:', e);
  postMessage({
    type: "error",
    error: String(e),
  });
});

