"use client";

import {
  useEffect,
  useRef,
} from "react";
import { useVM } from "../hooks/useVM";

export default function Home() {
  const { output, status, sendInput, cpuLoad, memUsage } = useVM();
  const endRef = useRef<HTMLDivElement>(null);

  // Auto scroll
  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [output]);

  // Global key handler so you can still type anywhere on the page
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;

      // Don't double-handle keys when an input/textarea/contenteditable has focus
      const target = e.target as HTMLElement | null;
      if (target) {
        const tag = target.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA" || target.isContentEditable) {
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
  }, [sendInput]);

  return (
    <div className="min-h-screen bg-black text-green-500 p-4 font-mono text-lg flex flex-col">
      <div className="mb-2 border-b border-green-700 pb-2 flex justify-between">
        <h1 className="font-bold">RISC-V VM</h1>
        <div className="flex gap-4 items-center">
          <span className="text-sm text-green-400">
            CPU: {cpuLoad.toFixed(0)}%
          </span>
          <span className="text-sm text-green-400">
            MEM: {(memUsage / (1024 * 1024)).toFixed(1)} MiB
          </span>
          <span
            className={status === "Running" ? "text-green-500" : "text-red-500"}
          >
            [{status}]
          </span>
        </div>
      </div>

      <div className="flex-grow whitespace-pre-wrap break-all focus:outline-none" tabIndex={0}>
        {output}
        <span className="animate-pulse">_</span>
        <div ref={endRef} />
      </div>
      
      <div className="mt-2 text-xs text-gray-500">
          Type anywhere to send input to the VM.
      </div>
    </div>
  );
}
