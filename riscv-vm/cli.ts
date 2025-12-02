#!/usr/bin/env node
/// <reference types="node" />
/**
 * RISC-V VM CLI
 *
 * This CLI mirrors the native Rust VM CLI interface:
 * - loads a kernel image (ELF or raw binary) via --kernel/-k
 * - optionally loads a VirtIO block disk image (e.g. xv6 `fs.img`) via --disk/-d
 * - optionally specifies number of harts via --harts/-n (0 = auto-detect as CPU/2)
 * - can optionally connect to a network relay via --net-webtransport
 * - runs the VM in a tight loop
 * - connects stdin → UART input and UART output → stdout
 *
 * Multi-hart support:
 * - Uses Node.js worker_threads for parallel execution
 * - Hart 0 runs on main thread (handles I/O)
 * - Harts 1+ run on worker threads
 * - All harts share memory via SharedArrayBuffer
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Worker } from 'node:worker_threads';
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';

// Default relay server URL and cert hash.
const DEFAULT_RELAY_URL =
  process.env.RELAY_URL || 'https://localhost:4433';
const DEFAULT_CERT_HASH =
  process.env.RELAY_CERT_HASH || '';

/**
 * Auto-detect the number of harts based on CPU cores.
 * Uses half of available cores, minimum 1.
 */
function detectHartCount(): number {
  const cpus = os.cpus().length;
  return Math.max(1, Math.floor(cpus / 2));
}

/**
 * Try to load the native WebTransport addon.
 * Returns the WebTransportClient class if available, null otherwise.
 */
async function loadNativeWebTransport(): Promise<any | null> {
  // Try to load from the native directory (built with npm run build:native)
  // ESM requires explicit file path, not directory imports
  const addonPath = path.resolve(__dirname, '..', 'native', 'index.js');
  
  try {
    const addon = await import(addonPath);
    if (addon.WebTransportClient) {
      console.error('[CLI] Native WebTransport addon loaded');
      return addon.WebTransportClient;
    }
    console.error('[CLI] Native addon loaded but WebTransportClient not found');
  } catch (e: any) {
    // Not available - likely not built yet
    console.error('[CLI] Native WebTransport addon not available');
    console.error(`[CLI] Tried to load from: ${addonPath}`);
    console.error(`[CLI] Error: ${e.message || e}`);
    console.error('[CLI] Build it with: cd riscv-vm && npm run build:native');
  }
  return null;
}

/**
 * Create and initialize a Wasm VM instance with multi-hart support:
 * - initializes the WASM module once via `WasmInternal`
 * - constructs `WasmVm` with the kernel bytes and specified hart count
 * - optionally attaches a VirtIO block device from a disk image
 * - optionally connects to a network relay (WebTransport/WebSocket)
 * - starts worker threads for secondary harts
 */
async function createVm(
  kernelPath: string,
  options?: {
    disk?: string;
    harts?: number;
    netWebtransport?: string;
    certHash?: string;
    debug?: boolean;
  },
) {
  let kernelBytes: Uint8Array;

  if (kernelPath.startsWith('http://') || kernelPath.startsWith('https://')) {
    if (options?.debug) {
      console.error(`[CLI] Downloading kernel from ${kernelPath}...`);
    }
    const response = await fetch(kernelPath);
    if (!response.ok) {
      throw new Error(
        `Failed to fetch kernel from ${kernelPath}: ${response.statusText}`,
      );
    }
    const arrayBuffer = await response.arrayBuffer();
    kernelBytes = new Uint8Array(arrayBuffer);
  } else {
    const resolvedKernel = path.resolve(kernelPath);
    if (!fs.existsSync(resolvedKernel)) {
      throw new Error(`Kernel file not found at ${resolvedKernel}`);
    }
    const kernelBuf = fs.readFileSync(resolvedKernel);
    kernelBytes = new Uint8Array(kernelBuf);
  }

  const { WasmInternal } = await import('./');
  const wasm = await WasmInternal();
  const VmCtor = wasm.WasmVm;
  if (!VmCtor) {
    throw new Error('WasmVm class not found in wasm module');
  }

  // Create VM with requested number of harts
  const numHarts = options?.harts ?? 1;
  const vm = VmCtor.new_with_harts 
    ? VmCtor.new_with_harts(kernelBytes, numHarts) 
    : new VmCtor(kernelBytes);

  if (options?.disk) {
    const resolvedDisk = path.resolve(options.disk);
    const diskBuf = fs.readFileSync(resolvedDisk);
    const diskBytes = new Uint8Array(diskBuf);

    if (typeof vm.load_disk === 'function') {
      vm.load_disk(diskBytes);
      if (options?.debug) {
        console.error(`[VM] Loaded disk: ${resolvedDisk}`);
      }
    }
  }

  // Start worker threads for secondary harts (1..numHarts)
  const workers: Worker[] = [];
  if (numHarts > 1 && typeof vm.get_shared_buffer === 'function') {
    const sharedBuffer = vm.get_shared_buffer();
    
    if (sharedBuffer) {
      // Get entry PC for workers
      const entryPc = typeof (vm as any).entry_pc === 'function' ? (vm as any).entry_pc() : 0x80000000;
      
      // Path to WASM file and worker script
      const wasmPath = path.resolve(__dirname, '..', 'pkg', 'riscv_vm_bg.wasm');
      const workerPath = path.resolve(__dirname, 'node-worker.js');
      
      console.error(`[VM] Starting ${numHarts - 1} worker threads...`);
      
      for (let hartId = 1; hartId < numHarts; hartId++) {
        const worker = new Worker(workerPath, {
          workerData: {
            hartId,
            sharedMem: sharedBuffer,
            entryPc: Number(entryPc),
            wasmPath,
          },
        });
        
        worker.on('message', (msg: any) => {
          if (msg.type === 'ready') {
            console.error(`[VM] Worker ${msg.hartId} ready`);
          } else if (msg.type === 'halted') {
            console.error(`[VM] Worker ${msg.hartId} halted (${msg.stepCount} steps)`);
          } else if (msg.type === 'error') {
            console.error(`[VM] Worker ${msg.hartId} error: ${msg.error}`);
          }
        });
        
        worker.on('error', (err) => {
          console.error(`[VM] Worker ${hartId} error:`, err);
        });
        
        worker.on('exit', (code) => {
          if (code !== 0) {
            console.error(`[VM] Worker ${hartId} exited with code ${code}`);
          }
        });
        
        workers.push(worker);
      }
      
      console.error(`[VM] Started ${workers.length} worker threads`);
    } else {
      console.error('[VM] Warning: SharedArrayBuffer not available, running single-threaded');
    }
  }

  // Network setup via native WebTransport addon (Node.js)
  let nativeNetClient: any = null;
  if (options?.netWebtransport) {
    const relayUrl = options.netWebtransport;
    const certHash = options.certHash || DEFAULT_CERT_HASH || undefined;
    
    // Try to use native WebTransport addon
    const WebTransportClient = await loadNativeWebTransport();
    
    if (WebTransportClient) {
      // Use native addon for WebTransport
      nativeNetClient = new WebTransportClient(relayUrl, certHash);
      
      // Get MAC address from native client and set up external network
      const macBytes = nativeNetClient.macBytes();
      if (typeof vm.setup_external_network === 'function') {
        vm.setup_external_network(new Uint8Array(macBytes));
        console.error(`[Network] External network setup with native WebTransport`);
        console.error(`[Network] MAC: ${nativeNetClient.macAddress()}`);
        console.error(`[Network] Connecting to ${relayUrl}...`);
      } else {
        console.error('[Network] VM does not support external network (rebuild WASM)');
        nativeNetClient.shutdown();
        nativeNetClient = null;
      }
    } else {
      // Fall back to WASM WebTransport (won't work in Node.js but try anyway)
      console.error('[Network] Warning: Native WebTransport addon not available');
      console.error('[Network] WebTransport requires the native addon in Node.js');
      console.error('[Network] Build the addon with: cd riscv-vm && npm run build:native');
    }
  }

  return { vm, nativeNetClient, workers };
}

/**
 * Run the VM in a loop and wire stdin/stdout to the UART, similar to the browser loop:
 * - executes a fixed number of instructions per tick
 * - drains the UART output buffer and writes to stdout
 * - feeds raw stdin bytes into the VM's UART input
 * - bridges packets between native WebTransport addon and VM
 * - manages worker threads for secondary harts
 */
function runVmLoop(vm: any, nativeNetClient: any | null, workers: Worker[] = []) {
  let running = true;
  let networkConnected = false;

  const shutdown = (code: number) => {
    if (!running) return;
    running = false;

    // Terminate worker threads
    for (const worker of workers) {
      worker.terminate();
    }

    // Shutdown native network client
    if (nativeNetClient) {
      nativeNetClient.shutdown();
    }

    if (process.stdin.isTTY && (process.stdin as any).setRawMode) {
      (process.stdin as any).setRawMode(false);
    }
    process.stdin.pause();

    process.exit(code);
  };

  // Handle Ctrl+C via signal as a fallback
  process.on('SIGINT', () => {
    shutdown(0);
  });

  // Configure stdin → VM UART input
  if (process.stdin.isTTY && (process.stdin as any).setRawMode) {
    (process.stdin as any).setRawMode(true);
  }
  process.stdin.resume();

  process.stdin.on('data', (chunk) => {
    // In raw mode `chunk` is typically a Buffer; iterate its bytes.
    for (const byte of chunk as any as Uint8Array) {
      // Ctrl+C (ETX) – terminate the VM and exit
      if (byte === 3) {
        shutdown(0);
        return;
      }

      // Map CR to LF as in the React hook
      if (byte === 13) {
        vm.input(10);
      } else if (byte === 127 || byte === 8) {
        // Backspace
        vm.input(8);
      } else {
        vm.input(byte);
      }
    }
  });

  const INSTRUCTIONS_PER_TICK = 100_000;

  const drainOutput = () => {
    // Drain UART output buffer
    // In raw terminal mode, we need \r\n for proper line breaks
    const outChunks: string[] = [];
    let limit = 2000;
    let code = typeof vm.get_output === 'function' ? vm.get_output() : undefined;

    while (code !== undefined && limit-- > 0) {
      const c = Number(code);

      if (c === 8) {
        // Backspace – move cursor back, erase, move back
        outChunks.push('\b \b');
      } else if (c === 10) {
        // LF -> CRLF for raw terminal mode (like native CLI)
        outChunks.push('\r\n');
      } else if (c === 13) {
        // CR - just output CR
        outChunks.push('\r');
      } else {
        // Pass through all other characters including ANSI escape sequences
        outChunks.push(String.fromCharCode(c));
      }

      code = vm.get_output();
    }

    if (outChunks.length) {
      process.stdout.write(outChunks.join(''));
    }
  };

  // Bridge packets between native WebTransport and VM
  const bridgeNetwork = () => {
    if (!nativeNetClient) return;
    
    // Check connection status
    if (!networkConnected && nativeNetClient.isRegistered()) {
      networkConnected = true;
      const ip = nativeNetClient.assignedIp();
      console.error(`\r\n[Network] Connected! IP: ${ip}`);
      
      // Set IP in VM's external network backend
      const ipBytes = nativeNetClient.assignedIpBytes();
      if (ipBytes && typeof vm.set_external_network_ip === 'function') {
        vm.set_external_network_ip(new Uint8Array(ipBytes));
      }
    }
    
    // Forward packets from native client to VM (RX)
    let packet = nativeNetClient.recv();
    while (packet) {
      if (typeof vm.inject_network_packet === 'function') {
        // Convert Buffer to Uint8Array for WASM
        vm.inject_network_packet(new Uint8Array(packet));
      }
      packet = nativeNetClient.recv();
    }
    
    // Forward packets from VM to native client (TX)
    if (typeof vm.extract_network_packet === 'function') {
      let txPacket = vm.extract_network_packet();
      while (txPacket) {
        // txPacket is Uint8Array from WASM, convert to Buffer for native addon
        nativeNetClient.send(Buffer.from(txPacket));
        txPacket = vm.extract_network_packet();
      }
    }
  };

  const loop = () => {
    if (!running) return;

    try {
      // Execute a batch of instructions
      for (let i = 0; i < INSTRUCTIONS_PER_TICK; i++) {
        vm.step();
      }

      // Drain output
      drainOutput();
      
      // Bridge network packets
      bridgeNetwork();

      // Check if VM has halted (e.g., shutdown command)
      if (typeof vm.is_halted === 'function' && vm.is_halted()) {
        // Drain any remaining output
        drainOutput();
        
        const haltCode = typeof vm.halt_code === 'function' ? vm.halt_code() : 0n;
        if (haltCode === 0x5555n) {
          console.log('\r\n[VM] Clean shutdown (PASS)');
        } else {
          console.log(`\r\n[VM] Shutdown with code: 0x${haltCode.toString(16)}`);
        }
        shutdown(0);
        return;
      }
    } catch (err) {
      console.error('\r\n[VM] Error while executing:', err);
      shutdown(1);
      return;
    }

    // Schedule the next tick; run as fast as the event loop allows.
    setImmediate(loop);
  };

  loop();
}

/**
 * Print banner matching native VM output
 */
function printBanner(kernelPath: string, numHarts: number, netWebtransport?: string) {
  const kernelName = path.basename(kernelPath);
  
  console.log();
  console.log('╔══════════════════════════════════════════════════════════════╗');
  console.log('║              RISC-V Emulator (WASM + Worker Threads)         ║');
  console.log('╠══════════════════════════════════════════════════════════════╣');
  console.log(`║  Kernel: ${kernelName.padEnd(50)} ║`);
  console.log(`║  Harts:  ${String(numHarts).padEnd(50)} ║`);
  if (netWebtransport) {
    console.log(`║  Network: ${netWebtransport.padEnd(49)} ║`);
  }
  console.log('╚══════════════════════════════════════════════════════════════╝');
  console.log();
}

const argv = (yargs(hideBin(process.argv)) as any)
  .usage('Usage: $0 [options]')
  .option('kernel', {
    alias: 'k',
    type: 'string',
    describe: 'Path to kernel ELF or binary',
    demandOption: true,
  })
  .option('disk', {
    alias: 'd',
    type: 'string',
    describe: 'Path to disk image (optional)',
  })
  .option('harts', {
    alias: 'n',
    type: 'number',
    describe: 'Number of harts (0 = auto-detect as CPU/2)',
    default: 0,
  })
  .option('net-webtransport', {
    type: 'string',
    describe: 'WebTransport relay URL for networking (e.g., https://127.0.0.1:4433)',
  })
  .option('cert-hash', {
    type: 'string',
    describe: 'Certificate hash for WebTransport (for self-signed certs)',
  })
  .option('debug', {
    type: 'boolean',
    describe: 'Enable debug output',
    default: false,
  })
  .help()
  .version()
  .parseSync();

(async () => {
  const kernelPath = argv.kernel as string;
  const diskPath = argv.disk as string | undefined;
  const hartsArg = argv.harts as number;
  const netWebtransport = argv['net-webtransport'] as string | undefined;
  const certHash = argv['cert-hash'] as string | undefined;
  const debug = argv.debug as boolean;

  // Auto-detect harts if set to 0, otherwise use user-specified value
  const numHarts = hartsArg === 0 ? detectHartCount() : hartsArg;

  // Print banner
  printBanner(kernelPath, numHarts, netWebtransport);

  try {
    const { vm, nativeNetClient, workers } = await createVm(kernelPath, {
      disk: diskPath,
      harts: numHarts,
      netWebtransport,
      certHash,
      debug,
    });
    
    // Signal workers that they can start executing
    // This is done after the main hart has had a chance to initialize
    if (typeof (vm as any).allow_workers_to_start === 'function') {
      // Give hart 0 a head start to initialize kernel data structures
      setTimeout(() => {
        (vm as any).allow_workers_to_start();
        console.error('[VM] Workers allowed to start');
      }, 100);
    }
    
    runVmLoop(vm, nativeNetClient, workers);
  } catch (err) {
    console.error('[CLI] Failed to start VM:', err);
    process.exit(1);
  }
})();
