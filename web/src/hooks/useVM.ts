import { useRef, useState, useCallback } from 'react';
import init, { WasmVm } from '../pkg/riscv_vm';

export type KernelType = 'custom_kernel' | 'kernel';
export type VMStatus = 'off' | 'booting' | 'running' | 'error';

let wasmInitialized = false;

// Get the base path for assets (handles GitHub Pages deployment)
function getBasePath(): string {
  // In production on GitHub Pages, use the repo name as base path
  if (typeof window !== 'undefined') {
    const path = window.location.pathname;
    // Check if we're on GitHub Pages (path starts with /repo-name/)
    const match = path.match(/^\/([^/]+)\//);
    if (match && match[1] !== '_next') {
      return `/${match[1]}`;
    }
  }
  return '';
}

function assetPath(path: string): string {
  const base = getBasePath();
  return `${base}${path}`;
}

export function useVM() {
  const vmRef = useRef<WasmVm | null>(null);
  const [output, setOutput] = useState<string>("");
  const [status, setStatus] = useState<VMStatus>("off");
  const [errorMessage, setErrorMessage] = useState<string>("");
  const requestRef = useRef<number | null>(null);
  const [cpuLoad, setCpuLoad] = useState<number>(0);
  const [memUsage, setMemUsage] = useState<number>(0);
  const [currentKernel, setCurrentKernel] = useState<KernelType | null>(null);
  const activeRef = useRef<boolean>(false);

  const loop = useCallback(() => {
    const vm = vmRef.current;
    if (!vm || !activeRef.current) return;

    const INSTRUCTIONS_PER_FRAME = 100000;

    try {
      const t0 = performance.now();
      for (let i = 0; i < INSTRUCTIONS_PER_FRAME; i++) {
        vm.step();
      }
      const t1 = performance.now();
      const duration = t1 - t0;
      const load = Math.min(100, (duration / 16.67) * 100);
      setCpuLoad(load);

      // Query memory usage if the wasm exposes it
      const anyVm = vm;
      if (typeof anyVm.get_memory_usage === 'function') {
        const usage = Number(anyVm.get_memory_usage());
        setMemUsage(usage);
      }

      // Drain output buffer (sanitize control chars)
      const codes: number[] = [];
      let ch = (vm).get_output?.();
      let limit = 2000;
      while (ch !== undefined && limit > 0) {
        codes.push(Number(ch));
        ch = (vm).get_output?.();
        limit--;
      }

      if (codes.length) {
        setOutput(prev => {
          let current = prev;
          for (const code of codes) {
            if (code === 8) {
              // Backspace
              current = current.slice(0, -1);
            } else if (code === 10 || code === 13 || (code >= 32 && code <= 126)) {
              current += String.fromCharCode(code);
            }
          }
          return current;
        });
      }

      if (activeRef.current) {
        requestRef.current = requestAnimationFrame(loop);
      }
    } catch (e: any) {
      setStatus('error');
      setErrorMessage(`Crashed: ${e}`);
      console.error(e);
    }
  }, []);

  const startVM = useCallback(async (kernelType: KernelType) => {
    // Stop any existing VM
    activeRef.current = false;
    if (requestRef.current !== null) {
      cancelAnimationFrame(requestRef.current);
      requestRef.current = null;
    }
    vmRef.current = null;
    
    setOutput("");
    setStatus("booting");
    setErrorMessage("");
    setCpuLoad(0);
    setMemUsage(0);

    try {
      // Initialize WASM only once
      if (!wasmInitialized) {
        await init(assetPath('/riscv_vm_bg.wasm'));
        wasmInitialized = true;
      }

      // Load kernel
      const kernelRes = await fetch(assetPath(`/${kernelType}`));
      if (!kernelRes.ok) throw new Error(`Failed to load kernel: ${kernelRes.statusText}`);
      const kernelBuf = await kernelRes.arrayBuffer();
      const kernelBytes = new Uint8Array(kernelBuf);

      const vm = new WasmVm(kernelBytes);
      
      // Load disk image for xv6 kernel
      if (kernelType === 'kernel') {
        try {
          const diskRes = await fetch(assetPath('/fs.img'));
          if (diskRes.ok) {
            const diskBuf = await diskRes.arrayBuffer();
            const diskBytes = new Uint8Array(diskBuf);
            vm.load_disk(diskBytes);
          }
        } catch {
          // Disk image not available, continue without it
        }
      }

      vmRef.current = vm;
      setCurrentKernel(kernelType);
      setStatus("running");
      activeRef.current = true;
      
      loop();
    } catch (err: any) {
      setStatus('error');
      setErrorMessage(err.message || String(err));
    }
  }, [loop]);

  const shutdownVM = useCallback(() => {
    activeRef.current = false;
    if (requestRef.current !== null) {
      cancelAnimationFrame(requestRef.current);
      requestRef.current = null;
    }
    vmRef.current = null;
    setStatus("off");
    setOutput("");
    setCpuLoad(0);
    setMemUsage(0);
    setCurrentKernel(null);
    setErrorMessage("");
  }, []);

  const sendInput = useCallback((key: string) => {
    const vm = vmRef.current;
    if (!vm || status !== 'running') return;

    // Map Enter to \n
    if (key === 'Enter') {
      vm.input(10);
      return;
    }

    // Map Backspace to 8
    if (key === 'Backspace') {
      vm.input(8);
      return;
    }

    if (key.length === 1) {
      vm.input(key.charCodeAt(0));
    }
  }, [status]);

  return { 
    output, 
    status, 
    errorMessage,
    sendInput, 
    cpuLoad, 
    memUsage, 
    currentKernel,
    startVM,
    shutdownVM
  };
}
