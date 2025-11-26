import { useEffect, useRef, useState, useCallback } from 'react';
import init, { WasmVm } from '../pkg/riscv_vm';

export function useVM() {
  const vmRef = useRef<WasmVm | null>(null);
  const [output, setOutput] = useState<string>("");
  const [status, setStatus] = useState<string>("Initializing...");
  const requestRef = useRef<number | null>(null);
  const [cpuLoad, setCpuLoad] = useState<number>(0);
  const [memUsage, setMemUsage] = useState<number>(0);

  useEffect(() => {
    let active = true;

    async function start() {
      try {
        // Let the bundler resolve the correct wasm asset path.
        await init('/riscv_vm_bg.wasm');
        
        if (!active) return;

        // Load kernel
        const kernelRes = await fetch('/kernel');
        if (!kernelRes.ok) throw new Error(`Failed to load kernel: ${kernelRes.statusText}`);
        const kernelBuf = await kernelRes.arrayBuffer();
        const kernelBytes = new Uint8Array(kernelBuf);
        
        const vm = new WasmVm(kernelBytes);
        
        // Try to load disk image (optional - some kernels don't need it)
        try {
          const diskRes = await fetch('/fs.img');
          if (diskRes.ok) {
            const diskBuf = await diskRes.arrayBuffer();
            const diskBytes = new Uint8Array(diskBuf);
            vm.load_disk(diskBytes);
          }
        } catch {
          // Disk image not available, continue without it
        }
        
        vmRef.current = vm;
        setStatus("Running");
        
        loop();
      } catch (err: any) {
        if (active) setStatus(`Error: ${err.message || err}`);
      }
    }
    start();
    
    return () => {
      active = false;
      if (requestRef.current !== null) cancelAnimationFrame(requestRef.current);
    };
  }, []);

  const loop = () => {
    const vm = vmRef.current;
    if (!vm) return;

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
      const anyVm = vm as any;
      if (typeof anyVm.get_memory_usage === 'function') {
        const usage = Number(anyVm.get_memory_usage());
        setMemUsage(usage);
      }
      
      // Drain output buffer (sanitize control chars)
      const codes: number[] = [];
      let ch = (vm as any).get_output?.();
      let limit = 2000;
      while (ch !== undefined && limit > 0) {
        codes.push(Number(ch));
        ch = (vm as any).get_output?.();
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
            } else {
              // Drop other control bytes
            }
          }
          return current;
        });
      }
      
      requestRef.current = requestAnimationFrame(loop);
    } catch (e: any) {
      setStatus(`Crashed: ${e}`);
      console.error(e);
    }
  };

  const sendInput = useCallback((key: string) => {
    const vm = vmRef.current;
    if (!vm) return;
    
    // Map Enter to \n
    if (key === 'Enter') {
        vm.input(10); // \n
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
  }, []);

  return { output, status, sendInput, cpuLoad, memUsage };
}

