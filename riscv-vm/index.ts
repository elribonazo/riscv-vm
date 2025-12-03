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
// JIT Worker Types
// ============================================================================

/**
 * Message sent to JIT worker to request compilation.
 */
export interface JitCompileRequest {
  type: "compile";
  /** Starting PC of the block */
  pc: number;
  /** Bincode-serialized CompileRequest bytes */
  requestBytes: ArrayBuffer;
}

/**
 * 
 * 
 * Message received from JIT worker with compilation result.
 */
export interface JitCompileResponse {
  type: "compiled";
  /** Starting PC of the compiled block */
  pc: number;
  /** Whether compilation succeeded */
  success: boolean;
  /** Compiled WASM bytes (if successful) */
  wasmBytes?: Uint8Array;
  /** Status if not successful */
  status?: "unsuitable" | "error";
  /** Compilation time in microseconds */
  compileTimeUs?: number;
}

/**
 * JIT worker ready message.
 */
export interface JitWorkerReadyMessage {
  type: "ready";
}

/**
 * JIT worker error message.
 */
export interface JitWorkerErrorMessage {
  type: "error";
  message: string;
}

/**
 * All possible JIT worker outbound messages.
 */
export type JitWorkerOutboundMessage =
  | JitCompileResponse
  | JitWorkerReadyMessage
  | JitWorkerErrorMessage;

/**
 * All possible JIT worker inbound messages.
 */
export type JitWorkerInboundMessage = JitCompileRequest;

// ============================================================================
// JIT Diagnostic Types
// ============================================================================

/**
 * Cache statistics from the JIT system.
 */
export interface JitCacheStats {
  /** Number of cache hits */
  hits: number;
  /** Number of cache misses */
  misses: number;
  /** Number of blocks inserted into cache */
  insertions: number;
  /** Number of blocks evicted from cache */
  evictions: number;
  /** Total bytes of WASM compiled */
  bytesCompiled: number;
  /** Number of cache invalidations */
  invalidations: number;
}

/**
 * Execution trace statistics.
 */
export interface JitTraceStats {
  /** Number of JIT block executions */
  jitExecutions: number;
  /** Number of interpreter block executions */
  interpExecutions: number;
  /** Number of successful compilations */
  compilations: number;
  /** Number of failed compilations */
  compilationFailures: number;
  /** Total WASM bytes compiled */
  totalWasmBytes: number;
  /** Total compilation time in microseconds */
  totalCompileTimeUs: number;
  /** Number of traps during JIT execution */
  traps: number;
  /** Number of cache invalidations */
  invalidations: number;
  /** Number of cache hits */
  cacheHits: number;
  /** Number of cache misses */
  cacheMisses: number;
  /** Number of interrupt-triggered exits */
  interruptExits: number;
}

/**
 * Complete JIT diagnostics information.
 */
export interface JitDiagnostics {
  /** Whether JIT is currently enabled and operational */
  enabled: boolean;
  /** Whether JIT was disabled due to errors */
  disabledByError: boolean;
  /** Reason JIT was disabled (if applicable) */
  disabledReason: string | null;
  /** Number of consecutive compilation failures */
  consecutiveFailures: number;
  /** Total number of compilation failures */
  totalFailures: number;
  /** Total number of successful compilations */
  successfulCompilations: number;
  /** Number of blocks that failed to compile (blacklisted) */
  failedBlocksCount: number;
  /** Number of blocks currently in cache */
  cacheEntries: number;
  /** Current cache memory usage in bytes */
  cacheBytes: number;
  /** Cache statistics */
  cacheStats: JitCacheStats;
  /** Trace statistics */
  traceStats: JitTraceStats;
}

/**
 * JIT configuration options.
 */
export interface JitConfig {
  /** Enable JIT compilation (default: true) */
  enabled?: boolean;
  /** Minimum execution count before compiling (default: 50) */
  compileThreshold?: number;
  /** Maximum block size in ops (default: 256) */
  maxBlockSize?: number;
  /** Maximum WASM module size in bytes (default: 65536) */
  maxWasmSize?: number;
  /** Maximum cache entries (default: 1024) */
  cacheMaxEntries?: number;
  /** Maximum cache size in bytes (default: 16MB) */
  cacheMaxBytes?: number;
  /** Enable debug WAT output to console (default: false) */
  debugWat?: boolean;
  /** Enable execution tracing (default: false) */
  traceEnabled?: boolean;
  /** Compilation timeout in ms (default: 100, 0=no timeout) */
  compileTimeoutMs?: number;
  /** Max consecutive failures before disabling (default: 10) */
  maxConsecutiveFailures?: number;
  /** Enable TLB fast-path optimization (default: false) */
  enableTlbFastPath?: boolean;
  /** Interrupt check interval in ops (default: 32, 0=disabled) */
  interruptCheckInterval?: number;
}

// ============================================================================
// WasmVm JIT Interface Extension
// ============================================================================

/**
 * Extended WasmVm interface with JIT methods.
 *
 * This interface extends the base WasmVm class exported from the WASM module
 * with type definitions for all JIT-related methods.
 */
export interface WasmVmJitMethods {
  // ═══════════════════════════════════════════════════════════════════════════
  // JIT Enable/Disable
  // ═══════════════════════════════════════════════════════════════════════════

  /** Enable or disable JIT compilation */
  setJitEnabled(enabled: boolean): void;

  /** Check if JIT is currently enabled and operational */
  isJitEnabled(): boolean;

  /** Re-enable JIT after it was disabled by errors */
  reenableJit(): void;

  // ═══════════════════════════════════════════════════════════════════════════
  // Debug Output Control
  // ═══════════════════════════════════════════════════════════════════════════

  /** Enable/disable JIT debug WAT output to console */
  setJitDebug(debug: boolean): void;

  /** Enable/disable JIT execution tracing */
  setJitTracing(enabled: boolean): void;

  /** Check if JIT tracing is enabled */
  isJitTracingEnabled(): boolean;

  // ═══════════════════════════════════════════════════════════════════════════
  // Diagnostics
  // ═══════════════════════════════════════════════════════════════════════════

  /** Get JIT diagnostics as JSON string */
  getJitDiagnostics(): string;

  /** Get JIT statistics as JSON string */
  getJitStats(): string;

  /** Dump recent JIT trace events to console */
  dumpJitTrace(count?: number): void;

  // ═══════════════════════════════════════════════════════════════════════════
  // Cache Management
  // ═══════════════════════════════════════════════════════════════════════════

  /** Clear all entries from JIT cache */
  clearJitCache(): void;

  /** Invalidate JIT cache for a specific PC */
  invalidateJitCacheAt(pc: bigint): boolean;

  /** Get number of cached JIT blocks */
  getJitCacheSize(): number;

  /** Get JIT cache memory usage in bytes */
  getJitCacheMemoryUsage(): number;

  // ═══════════════════════════════════════════════════════════════════════════
  // Configuration
  // ═══════════════════════════════════════════════════════════════════════════

  /** Set JIT compilation threshold */
  setJitThreshold(threshold: number): void;

  /** Get current JIT compilation threshold */
  getJitThreshold(): number;

  /** Set maximum block size for JIT compilation */
  setJitMaxBlockSize(size: number): void;

  /** Add a PC to the JIT blacklist */
  blacklistJitBlock(pc: bigint): void;

  /** Clear the JIT blacklist */
  clearJitBlacklist(): void;

  /** Reset all JIT error tracking */
  resetJitErrors(): void;

  // ═══════════════════════════════════════════════════════════════════════════
  // Performance
  // ═══════════════════════════════════════════════════════════════════════════

  /** Enable TLB fast-path optimization */
  setJitTlbFastPath(enabled: boolean): void;

  /** Set interrupt check interval (0 = disabled) */
  setJitInterruptCheckInterval(interval: number): void;

  /** Get JIT execution ratio (0.0 to 1.0) */
  getJitExecutionRatio(): number;

  /** Get JIT cache hit ratio (0.0 to 1.0) */
  getJitCacheHitRatio(): number;
}

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

// ============================================================================
// JIT Helper Functions
// ============================================================================

/**
 * Parse JIT diagnostics from the VM.
 *
 * @param vm - WasmVm instance with JIT methods
 * @returns Parsed JIT diagnostics object
 */
export function parseJitDiagnostics(vm: WasmVmJitMethods): JitDiagnostics {
  const json = vm.getJitDiagnostics();
  return JSON.parse(json) as JitDiagnostics;
}

/**
 * Parse JIT trace statistics from the VM.
 *
 * @param vm - WasmVm instance with JIT methods
 * @returns Parsed JIT trace statistics object
 */
export function parseJitStats(vm: WasmVmJitMethods): JitTraceStats {
  const json = vm.getJitStats();
  return JSON.parse(json) as JitTraceStats;
}

/**
 * Configure JIT with a partial configuration object.
 *
 * @param vm - WasmVm instance with JIT methods
 * @param config - Partial JIT configuration
 */
export function configureJit(vm: WasmVmJitMethods, config: JitConfig): void {
  if (config.enabled !== undefined) {
    vm.setJitEnabled(config.enabled);
  }
  if (config.compileThreshold !== undefined) {
    vm.setJitThreshold(config.compileThreshold);
  }
  if (config.maxBlockSize !== undefined) {
    vm.setJitMaxBlockSize(config.maxBlockSize);
  }
  if (config.debugWat !== undefined) {
    vm.setJitDebug(config.debugWat);
  }
  if (config.traceEnabled !== undefined) {
    vm.setJitTracing(config.traceEnabled);
  }
  if (config.enableTlbFastPath !== undefined) {
    vm.setJitTlbFastPath(config.enableTlbFastPath);
  }
  if (config.interruptCheckInterval !== undefined) {
    vm.setJitInterruptCheckInterval(config.interruptCheckInterval);
  }
}

/**
 * Print JIT diagnostic summary to console.
 *
 * Displays a formatted summary of JIT status, compilations,
 * cache statistics, and hit ratios.
 *
 * @param vm - WasmVm instance with JIT methods
 */
export function printJitSummary(vm: WasmVmJitMethods): void {
  const diag = parseJitDiagnostics(vm);

  console.log("╔═══════════════════════════════════════════╗");
  console.log("║           JIT Diagnostic Summary          ║");
  console.log("╠═══════════════════════════════════════════╣");
  console.log(
    `║ Status: ${diag.enabled ? "✅ Enabled" : "❌ Disabled"}`.padEnd(44) + "║"
  );
  if (diag.disabledByError) {
    console.log(`║ Reason: ${diag.disabledReason}`.padEnd(44) + "║");
  }
  console.log("╠═══════════════════════════════════════════╣");
  console.log(
    `║ Compilations: ${diag.successfulCompilations} success / ${diag.totalFailures} failed`.padEnd(
      44
    ) + "║"
  );
  console.log(
    `║ Cache: ${diag.cacheEntries} entries (${(diag.cacheBytes / 1024).toFixed(1)} KB)`.padEnd(
      44
    ) + "║"
  );
  const totalRequests = diag.cacheStats.hits + diag.cacheStats.misses;
  const hitRatio =
    totalRequests > 0 ? (diag.cacheStats.hits / totalRequests) * 100 : 0;
  console.log(`║ Hit ratio: ${hitRatio.toFixed(1)}%`.padEnd(44) + "║");
  console.log("╚═══════════════════════════════════════════╝");
}
