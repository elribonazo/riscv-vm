import { useRef, useState, useCallback } from 'react';
import init, { WasmVm, NetworkStatus as WasmNetworkStatus } from '../pkg/riscv_vm';

export type KernelType = 'custom' | 'kernel';
export type VMStatus = 'off' | 'booting' | 'running' | 'error';
export type NetworkStatus = 'disconnected' | 'connecting' | 'connected' | 'error';

// Map WASM NetworkStatus enum to our string type
function mapNetworkStatus(wasmStatus: WasmNetworkStatus): NetworkStatus {
  switch (wasmStatus) {
    case WasmNetworkStatus.Disconnected: return 'disconnected';
    case WasmNetworkStatus.Connecting: return 'connecting';
    case WasmNetworkStatus.Connected: return 'connected';
    case WasmNetworkStatus.Error: return 'error';
    default: return 'disconnected';
  }
}

let wasmInitialized = false;

// Default relay server URL and cert hash (WebTransport)
const DEFAULT_RELAY_URL = process.env.NEXT_PUBLIC_RELAY_URL || 'https://localhost:4433';
const DEFAULT_CERT_HASH = process.env.NEXT_PUBLIC_RELAY_CERT_HASH || '';

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
  
  // Network state - enabled by default for better UX
  const [networkStatus, setNetworkStatus] = useState<NetworkStatus>("disconnected");
  const [relayUrl, setRelayUrl] = useState<string>(DEFAULT_RELAY_URL);
  const [certHash, setCertHash] = useState<string>(DEFAULT_CERT_HASH);
  const [networkEnabled, setNetworkEnabled] = useState<boolean>(true);

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
      if (typeof vm.get_memory_usage === 'function') {
        const usage = Number(vm.get_memory_usage());
        setMemUsage(usage);
      }
      
      // Poll network status from WASM
      if (typeof vm.network_status === 'function') {
        const wasmNetStatus = vm.network_status();
        const mappedStatus = mapNetworkStatus(wasmNetStatus);
        setNetworkStatus(mappedStatus);
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
      const kernelRes = await fetch(assetPath(`/images/${kernelType}/kernel`));
      if (!kernelRes.ok) throw new Error(`Failed to load kernel: ${kernelRes.statusText}`);
      const kernelBuf = await kernelRes.arrayBuffer();
      const kernelBytes = new Uint8Array(kernelBuf);

      const vm = new WasmVm(kernelBytes);
      
      // Load disk image for xv6 kernel
      if (kernelType === 'kernel') {
        try {
          const diskRes = await fetch(assetPath('/images/fs.img'));
          if (diskRes.ok) {
            const diskBuf = await diskRes.arrayBuffer();
            const diskBytes = new Uint8Array(diskBuf);
            vm.load_disk(diskBytes);
          }
        } catch {
          // Disk image not available, continue without it
        }
      }
      
      // Connect network BEFORE starting execution (so kernel sees VirtIO device at boot)
      if (networkEnabled) {
        try {
          // Prefer WebTransport if available
          if (typeof vm.connect_webtransport === 'function') {
            const hashParam = certHash || undefined;
            vm.connect_webtransport(relayUrl, hashParam);
            console.log(`[Network] Initiating WebTransport connection to ${relayUrl}`);
          } else if (typeof vm.connect_network === 'function') {
            // Fallback to WebSocket if WebTransport not available
            vm.connect_network(relayUrl);
            console.log(`[Network] Initiating WebSocket connection to ${relayUrl}`);
          }
          // Status will be updated by polling in the main loop
        } catch (err: any) {
          console.warn('[Network] Pre-boot connection failed:', err);
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
  }, [loop, networkEnabled, relayUrl, certHash]);

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
    setNetworkStatus("disconnected");
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

  // Connect to network relay server
  const connectNetwork = useCallback((url?: string, hash?: string) => {
    const vm = vmRef.current;
    if (!vm || status !== 'running') {
      console.warn('Cannot connect network: VM not running');
      return;
    }

    const targetUrl = url || relayUrl;
    const targetHash = hash || certHash || undefined;
    
    try {
      // Prefer WebTransport
      if (typeof vm.connect_webtransport === 'function') {
        vm.connect_webtransport(targetUrl, targetHash);
        console.log(`[Network] Initiating WebTransport connection to ${targetUrl}`);
      } else if (typeof vm.connect_network === 'function') {
        vm.connect_network(targetUrl);
        console.log(`[Network] Initiating WebSocket connection to ${targetUrl}`);
      } else {
        console.warn('[Network] No network method available - rebuild WASM');
      }
      // Status will be updated by polling in the main loop
    } catch (err: any) {
      console.error('[Network] Connection error:', err);
    }
  }, [status, relayUrl, certHash]);

  // Disconnect from network
  const disconnectNetwork = useCallback(() => {
    const vm = vmRef.current;
    if (!vm) return;

    try {
      if (typeof vm.disconnect_network === 'function') {
        vm.disconnect_network();
        console.log('[Network] Disconnected');
        // Status will be updated by polling in the main loop
      }
    } catch (err: any) {
      console.error('[Network] Disconnect error:', err);
    }
  }, []);

  // Update relay URL
  const updateRelayUrl = useCallback((url: string) => {
    setRelayUrl(url);
  }, []);

  // Update cert hash
  const updateCertHash = useCallback((hash: string) => {
    setCertHash(hash);
  }, []);

  // Toggle network enabled (before boot)
  const toggleNetworkEnabled = useCallback((enabled: boolean) => {
    if (status === 'off') {
      setNetworkEnabled(enabled);
      if (!enabled) {
        setNetworkStatus('disconnected');
      }
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
    shutdownVM,
    // Network exports
    networkStatus,
    networkEnabled,
    relayUrl,
    certHash,
    updateRelayUrl,
    updateCertHash,
    connectNetwork,
    disconnectNetwork,
    toggleNetworkEnabled,
  };
}
