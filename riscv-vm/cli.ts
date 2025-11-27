#!/usr/bin/env node
/// <reference types="node" />
/**
 * RISC-V VM CLI
 *
 * This CLI mirrors the browser VM wiring in `useVM`:
 * - loads a kernel image (ELF or raw binary)
 * - optionally loads a VirtIO block disk image (e.g. xv6 `fs.img`)
 * - can optionally connect to a network relay (WebTransport/WebSocket)
 * - runs the VM in a tight loop
 * - connects stdin → UART input and UART output → stdout
 */

import fs from 'node:fs';
import path from 'node:path';
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';

// Default relay server URL and cert hash, mirroring the React hook.
const DEFAULT_RELAY_URL =
  process.env.RELAY_URL || 'https://localhost:4433';
const DEFAULT_CERT_HASH =
  process.env.RELAY_CERT_HASH || '';

/**
 * Create and initialize a Wasm VM instance, mirroring the React `useVM` hook:
 * - initializes the WASM module once via `WasmInternal`
 * - constructs `WasmVm` with the kernel bytes
 * - optionally attaches a VirtIO block device from a disk image
 * - optionally connects to a network relay (WebTransport/WebSocket)
 */
async function createVm(
  kernelPath: string,
  diskPath?: string,
  options?: {
    network?: boolean;
    relayUrl?: string;
    certHash?: string;
  },
) {
  const resolvedKernel = path.resolve(kernelPath);
  const kernelBuf =  fs.readFileSync(resolvedKernel);
  const kernelBytes = new Uint8Array(kernelBuf);

  const { WasmInternal } = await import('./');
  const wasm = await WasmInternal();
  const VmCtor = wasm.WasmVm;
  if (!VmCtor) {
    throw new Error('WasmVm class not found in wasm module');
  }

  const vm = new VmCtor(kernelBytes);

  if (diskPath) {
    const resolvedDisk = path.resolve(diskPath);
    const diskBuf =  fs.readFileSync(resolvedDisk);
    const diskBytes = new Uint8Array(diskBuf);

    if (typeof vm.load_disk === 'function') {
      vm.load_disk(diskBytes);
    }
  }

  // Optional network setup (pre-boot), mirroring `useVM.startVM`.
  if (options?.network) {
    const relayUrl = options.relayUrl || DEFAULT_RELAY_URL;
    const certHash = options.certHash || DEFAULT_CERT_HASH || undefined;

    try {
      if (typeof (vm as any).connect_webtransport === 'function') {
        (vm as any).connect_webtransport(relayUrl, certHash);
        console.error(
          `[Network] Initiating WebTransport connection to ${relayUrl}`,
        );
      } else if (typeof (vm as any).connect_network === 'function') {
        (vm as any).connect_network(relayUrl);
        console.error(
          `[Network] Initiating WebSocket connection to ${relayUrl}`,
        );
      } else {
        console.error(
          '[Network] No network methods available on VM (rebuild WASM with networking)',
        );
      }
    } catch (err) {
      console.error('[Network] Pre-boot connection failed:', err);
    }
  }

  return vm;
}

/**
 * Run the VM in a loop and wire stdin/stdout to the UART, similar to the browser loop:
 * - executes a fixed number of instructions per tick
 * - drains the UART output buffer and writes to stdout
 * - feeds raw stdin bytes into the VM's UART input
 */
function runVmLoop(vm: any) {
  let running = true;

  const shutdown = (code: number) => {
    if (!running) return;
    running = false;

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

  const loop = () => {
    if (!running) return;

    try {
      // Execute a batch of instructions
      for (let i = 0; i < INSTRUCTIONS_PER_TICK; i++) {
        vm.step();
      }

      // Drain UART output buffer, similar to `useVM`
      const outChunks: string[] = [];
      let limit = 2000;
      let code = typeof vm.get_output === 'function' ? vm.get_output() : undefined;

      while (code !== undefined && limit-- > 0) {
        const c = Number(code);

        if (c === 8) {
          // Backspace – move cursor back, erase, move back
          outChunks.push('\b \b');
        } else if (c === 10 || c === 13) {
          outChunks.push('\n');
        } else if (c >= 32 && c <= 126) {
          outChunks.push(String.fromCharCode(c));
        }

        code = vm.get_output();
      }

      if (outChunks.length) {
        process.stdout.write(outChunks.join(''));
      }
    } catch (err) {
      console.error('\n[VM] Error while executing:', err);
      shutdown(1);
      return;
    }

    // Schedule the next tick; run as fast as the event loop allows.
    setImmediate(loop);
  };

  loop();
}

(yargs(hideBin(process.argv)) as any)
  .command(
    'run <kernel>',
    'Runs a RISC-V kernel inside the virtual machine',
    (y: any) =>
      y
        .positional('kernel', {
          type: 'string',
          describe: 'Path to the RISC-V kernel (ELF or raw binary)',
          demandOption: true,
        })
        .option('disk', {
          alias: 'd',
          type: 'string',
          describe: 'Optional path to a VirtIO block disk image (e.g. xv6 fs.img)',
        })
        .option('network', {
          alias: 'n',
          type: 'boolean',
          describe:
            'Enable network and connect to relay at boot (uses WebTransport/WebSocket)',
        })
        .option('relay-url', {
          alias: 'r',
          type: 'string',
          describe: `Relay URL for WebTransport/WebSocket (default: ${DEFAULT_RELAY_URL})`,
        })
        .option('cert-hash', {
          alias: 'c',
          type: 'string',
          describe:
            'Optional certificate SHA-256 hash for self-signed TLS (used with WebTransport)',
        }),
    async (argv: any) => {
      const kernelPath = argv.kernel as string;
      const diskPath = (argv.disk ?? undefined) as string | undefined;
      const relayUrlArg = (argv['relay-url'] ?? undefined) as string | undefined;
      const certHashArg = (argv['cert-hash'] ?? undefined) as string | undefined;
      // If user explicitly passes --network or a relay URL, enable networking.
      const enableNetwork =
        (argv.network as boolean | undefined) ?? !!relayUrlArg;

      try {
        const vm = await createVm(kernelPath, diskPath, {
          network: enableNetwork,
          relayUrl: relayUrlArg,
          certHash: certHashArg,
        });
        runVmLoop(vm);
      } catch (err) {
        console.error('[CLI] Failed to start VM:', err);
        process.exit(1);
      }
    },
  )
  .demandCommand(1, 'You need to specify a command')
  .strict()
  .help()
  .parse();
