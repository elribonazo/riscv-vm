import wasmBuffer from "./pkg/riscv_vm_bg.wasm";

let loaded: typeof import("./pkg/riscv_vm") | undefined;

export async function WasmInternal() {
  if (!loaded) {
    const module = await import("./pkg/riscv_vm");
    const wasmInstance = module.initSync(wasmBuffer);
    await module.default(wasmInstance);
    loaded = module;
  }
  return loaded;
}

export { NetworkStatus, WasmVm } from "./pkg/riscv_vm";

// Re-export worker message types for consumers (from side-effect-free module)
export type {
  WorkerInitMessage,
  WorkerReadyMessage,
  WorkerHaltedMessage,
  WorkerErrorMessage,
  WorkerOutboundMessage,
} from "./worker-utils";

// Re-export worker utilities (from side-effect-free module)
export { isHaltRequested, requestHalt, isHalted } from "./worker-utils";

// ============================================================================
// Multi-Hart Worker Support
// ============================================================================

export interface VmOptions {
  /** Number of harts (auto-detected if not specified) */
  harts?: number;
  /** Path to worker script (default: '/worker.js') */
  workerScript?: string;
}

/**
 * Create a VM instance with optional SMP support.
 *
 * If SharedArrayBuffer is available (requires COOP/COEP headers), the VM
 * will run in true parallel mode with Web Workers for secondary harts.
 *
 * NOTE: In WASM, multi-hart mode is significantly slower due to
 * SharedArrayBuffer/Atomics overhead (see tasks/improvements.md).
 * Default is 1 hart unless explicitly specified.
 *
 * @param kernelData - ELF kernel binary
 * @param options - VM configuration options
 * @returns WasmVm instance
 */
export async function createVM(
  kernelData: Uint8Array,
  options: VmOptions = {}
): Promise<import("./pkg/riscv_vm").WasmVm> {
  const module = await WasmInternal();

  // In WASM, default to 1 hart due to SharedArrayBuffer/Atomics overhead
  // Multi-hart mode is ~8x slower than single hart (see tasks/improvements.md)
  // Users can explicitly request multiple harts if needed
  const defaultHarts = typeof window !== 'undefined' ? 1 : undefined;
  const harts = options.harts ?? defaultHarts;

  // Create VM with specified hart count (0 = auto-detect)
  const vm = harts !== undefined && harts > 0
    ? module.WasmVm.new_with_harts(kernelData, harts)
    : new module.WasmVm(kernelData);

  // Start workers if in SMP mode
  const workerScript = options.workerScript || "/worker.js";
  if (vm.is_smp()) {
    try {
      vm.start_workers(workerScript);
      console.log(`[VM] Started workers for ${vm.num_harts()} harts`);
    } catch (e) {
      console.warn("[VM] Failed to start workers, falling back to single-threaded:", e);
    }
  }

  console.log(`[VM] Created VM instance (SMP: ${vm.is_smp()}, harts: ${vm.num_harts()})`);

  return vm;
}

/**
 * Run the VM with an output callback for UART data.
 *
 * This function manages the main execution loop, stepping hart 0 on the
 * main thread. Secondary harts (if any) run in Web Workers.
 *
 * @param vm - WasmVm instance
 * @param onOutput - Callback for each character output
 * @param options - Run options
 * @returns Stop function to halt execution
 */
export function runVM(
  vm: import("./pkg/riscv_vm").WasmVm,
  onOutput: (char: string) => void,
  options: { stepsPerFrame?: number } = {}
): () => void {
  const stepsPerFrame = options.stepsPerFrame || 10000;
  let running = true;

  const loop = () => {
    if (!running) return;

    // Step primary hart (I/O coordination)
    for (let i = 0; i < stepsPerFrame; i++) {
      if (!vm.step()) {
        console.log("[VM] Halted");
        running = false;
        return;
      }
    }

    // Collect output
    let byte: number | undefined;
    while ((byte = vm.get_output()) !== undefined) {
      onOutput(String.fromCharCode(byte));
    }

    // Schedule next batch
    requestAnimationFrame(loop);
  };

  loop();

  // Return stop function
  return () => {
    running = false;
    vm.terminate_workers();
  };
}

// ============================================================================
// SharedArrayBuffer Support Detection
// ============================================================================

export interface SharedMemorySupport {
  supported: boolean;
  crossOriginIsolated: boolean;
  message: string;
}

/**
 * Check if SharedArrayBuffer is available for multi-threaded execution.
 *
 * SharedArrayBuffer requires Cross-Origin Isolation (COOP/COEP headers).
 * If not available, the VM will run in single-threaded mode.
 */
export function checkSharedMemorySupport(): SharedMemorySupport {
  const crossOriginIsolated = isCrossOriginIsolated();

  if (typeof SharedArrayBuffer === "undefined") {
    return {
      supported: false,
      crossOriginIsolated,
      message: "SharedArrayBuffer not defined. Browser may be too old.",
    };
  }

  if (!crossOriginIsolated) {
    return {
      supported: false,
      crossOriginIsolated,
      message:
        "Not cross-origin isolated. Add headers:\n" +
        "  Cross-Origin-Opener-Policy: same-origin\n" +
        "  Cross-Origin-Embedder-Policy: require-corp",
    };
  }

  // Try to create a SharedArrayBuffer
  try {
    new SharedArrayBuffer(8);
    return {
      supported: true,
      crossOriginIsolated,
      message: "SharedArrayBuffer available for SMP execution",
    };
  } catch (e) {
    return {
      supported: false,
      crossOriginIsolated,
      message: `SharedArrayBuffer blocked: ${e}`,
    };
  }
}

/**
 * Check if the page is cross-origin isolated (required for SharedArrayBuffer).
 */
export function isCrossOriginIsolated(): boolean {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}

// ============================================================================
// COOP/COEP Headers Reference
// ============================================================================

/**
 * Headers required for SharedArrayBuffer support.
 *
 * For Vite dev server, add to vite.config.ts:
 * ```ts
 * server: {
 *   headers: {
 *     "Cross-Origin-Opener-Policy": "same-origin",
 *     "Cross-Origin-Embedder-Policy": "require-corp",
 *   },
 * },
 * ```
 *
 * For production, configure your web server to add these headers.
 */
export const REQUIRED_HEADERS = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
} as const;

// ============================================================================
// Worker Management Utilities
// ============================================================================

/**
 * Manually create and manage workers for advanced use cases.
 *
 * Most users should use createVM() which handles workers automatically.
 */
export interface WorkerManager {
  /** Start a worker for a specific hart */
  startWorker(
    hartId: number,
    sharedMem: SharedArrayBuffer,
    entryPc: number,
    workerScript?: string
  ): Worker;
  /** Terminate all workers */
  terminateAll(): void;
  /** Get number of active workers */
  count(): number;
}

/**
 * Create a worker manager for manual worker control.
 */
export function createWorkerManager(): WorkerManager {
  const workers: Worker[] = [];

  return {
    startWorker(
      hartId: number,
      sharedMem: SharedArrayBuffer,
      entryPc: number,
      workerScript = "/worker.js"
    ): Worker {
      const worker = new Worker(workerScript, { type: "module" });

      worker.onmessage = (event) => {
        const { type, hartId: id, error } = event.data;
        switch (type) {
          case "ready":
            console.log(`[WorkerManager] Hart ${id} ready`);
            break;
          case "halted":
            console.log(`[WorkerManager] Hart ${id} halted`);
            break;
          case "error":
            console.error(`[WorkerManager] Hart ${id} error:`, error);
            break;
        }
      };

      worker.onerror = (e) => {
        console.error(`[WorkerManager] Worker error:`, e);
      };

      // Send init message
      worker.postMessage({
        hartId,
        sharedMem,
        entryPc,
      });

      workers.push(worker);
      return worker;
    },

    terminateAll(): void {
      for (const worker of workers) {
        worker.terminate();
      }
      workers.length = 0;
    },

    count(): number {
      return workers.length;
    },
  };
}
