"use client";

import { useEffect, useRef, useState } from "react";
import { useVM, KernelType, VMStatus } from "../hooks/useVM";

// Power icon SVG
const PowerIcon = () => (
  <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
    <path d="M12 2v10" strokeLinecap="round" />
    <path d="M18.4 6.6a9 9 0 1 1-12.8 0" strokeLinecap="round" />
  </svg>
);

// CPU chip icon
const CpuIcon = () => (
  <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
    <rect x="4" y="4" width="16" height="16" rx="2" stroke="currentColor" strokeWidth="1.5" fill="none" />
    <rect x="8" y="8" width="8" height="8" rx="1" fill="currentColor" />
    <path d="M9 1v3M15 1v3M9 20v3M15 20v3M1 9h3M1 15h3M20 9h3M20 15h3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
  </svg>
);

// Memory chip icon
const MemoryIcon = () => (
  <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
    <rect x="2" y="6" width="20" height="12" rx="2" stroke="currentColor" strokeWidth="1.5" fill="none" />
    <rect x="5" y="9" width="3" height="6" rx="0.5" fill="currentColor" />
    <rect x="10" y="9" width="3" height="6" rx="0.5" fill="currentColor" />
    <rect x="15" y="9" width="3" height="6" rx="0.5" fill="currentColor" />
  </svg>
);

// Floppy disk icon
const FloppyIcon = () => (
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
    <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
    <polyline points="17 21 17 13 7 13 7 21" />
    <polyline points="7 3 7 8 15 8" />
  </svg>
);

// RISC-V Logo/Badge
const RiscVBadge = () => (
  <div className="flex items-center gap-2">
    <div className="w-8 h-8 rounded bg-gradient-to-br from-amber-600 to-amber-800 flex items-center justify-center shadow-lg">
      <span className="text-white font-bold text-xs">RV</span>
    </div>
    <div className="flex flex-col">
      <span className="text-[10px] text-gray-500 leading-none">RISC-V</span>
      <span className="text-[8px] text-gray-600 leading-none">64-bit</span>
    </div>
  </div>
);

function getStatusLed(status: VMStatus): { power: string; activity: string } {
  switch (status) {
    case "off":
      return { power: "led-off", activity: "led-off" };
    case "booting":
      return { power: "led-on-amber", activity: "led-on-amber" };
    case "running":
      return { power: "led-on-green", activity: "led-on-green" };
    case "error":
      return { power: "led-on-red", activity: "led-off" };
    default:
      return { power: "led-off", activity: "led-off" };
  }
}

export default function Home() {
  const { 
    output, 
    status, 
    errorMessage,
    sendInput, 
    cpuLoad, 
    memUsage,
    currentKernel,
    startVM,
    shutdownVM
  } = useVM();
  
  const endRef = useRef<HTMLDivElement>(null);
  const [selectedKernel, setSelectedKernel] = useState<KernelType>("custom_kernel");

  // Auto scroll
  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [output]);

  // Global key handler
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (status !== "running") return;

      const target = e.target as HTMLElement | null;
      if (target) {
        const tag = target.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable) {
          return;
        }
      }

      if (e.key.length === 1 || e.key === "Enter" || e.key === "Backspace") {
        e.preventDefault();
        sendInput(e.key);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [sendInput, status]);

  const handlePowerClick = () => {
    if (status === "off" || status === "error") {
      startVM(selectedKernel);
    } else {
      shutdownVM();
    }
  };

  const leds = getStatusLed(status);
  const isOn = status === "running" || status === "booting";

  return (
    <div className="min-h-screen flex items-center justify-center p-4 md:p-8">
      {/* Computer Assembly */}
      <div className="flex flex-col items-center gap-4">
        
        {/* Monitor */}
        <div className="computer-case rounded-3xl p-6 md:p-8">
          {/* Brand name */}
          <div className="flex justify-between items-center mb-4 px-2">
            <span className="brand-text text-sm md:text-base">RISK-V</span>
            <RiscVBadge />
          </div>

          {/* Monitor bezel */}
          <div className="monitor-bezel">
            {/* CRT Screen */}
            <div className="crt-screen w-[320px] h-[240px] md:w-[600px] md:h-[400px] lg:w-[720px] lg:h-[480px]">
              {/* Screen content */}
              <div 
                className="screen-content h-full p-4 overflow-y-auto whitespace-pre-wrap break-all focus:outline-none"
                tabIndex={0}
              >
                {status === "off" && (
                  <div className="h-full flex items-center justify-center text-gray-600">
                    <div className="text-center">
                      <div className="text-2xl mb-2">⏻</div>
                      <div className="text-sm">Press power to boot</div>
                    </div>
                  </div>
                )}
                {status === "booting" && (
                  <div className="h-full flex items-center justify-center">
                    <div className="text-center">
                      <div className="text-xl mb-2">Booting...</div>
                      <div className="text-sm text-[var(--screen-green-dim)]">
                        Loading {selectedKernel === "custom_kernel" ? "Custom Kernel" : "xv6 Linux"}
                      </div>
                    </div>
                  </div>
                )}
                {status === "error" && (
                  <div className="h-full flex items-center justify-center text-red-500">
                    <div className="text-center">
                      <div className="text-xl mb-2">SYSTEM ERROR</div>
                      <div className="text-sm">{errorMessage}</div>
                      <div className="text-xs mt-4 text-gray-500">Press power to restart</div>
                    </div>
                  </div>
                )}
                {status === "running" && (
                  <>
                    {output}
                    <span className="cursor-blink">█</span>
                    <div ref={endRef} />
                  </>
                )}
              </div>
            </div>
          </div>

          {/* Monitor controls */}
          <div className="flex items-center justify-between mt-4 px-2">
            {/* Left side - Status LEDs */}
            <div className="flex items-center gap-4">
              <div className="flex items-center gap-2">
                <div className={`led ${leds.power}`} />
                <span className="label-text">PWR</span>
              </div>
              <div className="flex items-center gap-2">
                <div className={`led ${leds.activity}`} />
                <span className="label-text">ACT</span>
              </div>
            </div>

            {/* Center - Model info */}
            <div className="hidden md:flex flex-col items-center">
              <span className="label-text">MODEL VM-64</span>
            </div>

            {/* Right side - Performance stats */}
            <div className="flex items-center gap-4">
              <div className="flex items-center gap-1 text-xs text-gray-600">
                <CpuIcon />
                <span className="font-mono">{cpuLoad.toFixed(0)}%</span>
              </div>
              <div className="flex items-center gap-1 text-xs text-gray-600">
                <MemoryIcon />
                <span className="font-mono">{(memUsage / (1024 * 1024)).toFixed(0)}M</span>
              </div>
            </div>
          </div>
        </div>

        {/* Base Unit */}
        <div className="computer-case rounded-2xl p-4 md:p-6 w-full max-w-[320px] md:max-w-[600px] lg:max-w-[720px]">
          <div className="flex items-center justify-between">
            {/* Left - Floppy drive / Kernel selector */}
            <div className="flex items-center gap-3">
              <div className="floppy-slot w-32 md:w-40 h-8 flex items-center px-2">
                <select
                  value={selectedKernel}
                  onChange={(e) => setSelectedKernel(e.target.value as KernelType)}
                  disabled={isOn}
                  className="kernel-select w-full h-6 text-xs disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  <option value="custom_kernel">Custom Kernel</option>
                  <option value="kernel">xv6 Linux</option>
                </select>
              </div>
              <div className="hidden md:flex items-center gap-1 text-gray-500">
                <FloppyIcon />
                <span className="label-text">BOOT</span>
              </div>
            </div>

            {/* Center - Vent grille */}
            <div className="hidden lg:block vent-grille w-32 h-8" />

            {/* Right - Power button */}
            <div className="flex items-center gap-3">
              <span className="label-text hidden md:inline">POWER</span>
              <button
                onClick={handlePowerClick}
                className={`power-button ${isOn ? "on" : ""}`}
                title={isOn ? "Shut down" : "Power on"}
              >
                <span className={isOn ? "text-green-400" : "text-gray-400"}>
                  <PowerIcon />
                </span>
              </button>
            </div>
          </div>

          {/* Bottom label */}
          <div className="flex justify-center mt-3">
            <div className="flex items-center gap-2 text-[10px] text-gray-500">
              <span>RISC-V 64-BIT VIRTUAL MACHINE</span>
              <span>•</span>
              <span>128 MiB RAM</span>
              <span>•</span>
              <span>{currentKernel === "kernel" ? "xv6" : currentKernel === "custom_kernel" ? "Custom" : "No OS"}</span>
            </div>
          </div>
        </div>

        {/* Instructions */}
        <div className="text-center text-gray-500 text-xs mt-2 max-w-md">
          {status === "running" ? (
            <span>Type anywhere to send input to the VM. Press power button to shut down.</span>
          ) : status === "off" ? (
            <span>Select a kernel and press the power button to boot the virtual machine.</span>
          ) : status === "booting" ? (
            <span>System is starting up...</span>
          ) : (
            <span>An error occurred. Press power to restart.</span>
          )}
        </div>
      </div>
    </div>
  );
}
