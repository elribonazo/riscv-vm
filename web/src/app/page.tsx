"use client";

import { useEffect, useRef, useState } from "react";
import { useVM, KernelType, VMStatus, NetworkStatus } from "../hooks/useVM";

// 3D Computer SVG Component
const Computer3D = ({ 
  children, 
  isOn, 
  powerLed, 
  actLed, 
  netLed,
  onPowerClick 
}: { 
  children: React.ReactNode;
  isOn: boolean;
  powerLed: string;
  actLed: string;
  netLed: string;
  onPowerClick: () => void;
}) => {
  return (
    <div className="computer-3d-container">
      {/* Main monitor body */}
      <div className="monitor-3d">
        {/* Top face (creates depth) */}
        <div className="monitor-top-face" />
        
        {/* Left face (creates depth) */}
        <div className="monitor-left-face" />
        
        {/* Front face with screen */}
        <div className="monitor-front-face">
          {/* Bezel */}
          <div className="monitor-bezel-3d">
            {/* Screen area */}
            <div className="screen-container-3d">
              {children}
            </div>
          </div>
          
          {/* Bottom bezel with LEDs and brand */}
          <div className="monitor-bottom-strip">
            <div className="led-cluster">
              <div className={`led-3d ${powerLed}`} title="Power" />
              <div className={`led-3d ${actLed}`} title="Activity" />
              <div className={`led-3d ${netLed}`} title="Network" />
            </div>
            <span className="brand-emboss">RISK-V</span>
            <button 
              onClick={onPowerClick}
              className={`power-btn-3d ${isOn ? 'on' : ''}`}
              title={isOn ? "Shutdown" : "Power On"}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
                <path d="M12 2v10" strokeLinecap="round" />
                <path d="M18.4 6.6a9 9 0 1 1-12.8 0" strokeLinecap="round" />
              </svg>
            </button>
          </div>
        </div>
      </div>
      
      {/* Stand/Neck */}
      <div className="monitor-stand">
        <div className="stand-neck" />
        <div className="stand-base">
          <div className="stand-base-top" />
        </div>
      </div>
    </div>
  );
};

// Minimal network indicator
const NetworkBadge = ({ status, enabled, onClick }: { 
  status: NetworkStatus; 
  enabled: boolean;
  onClick: () => void;
}) => {
  const getStatusInfo = () => {
    if (status === 'connected') return { color: '#22c55e', label: 'Connected' };
    if (status === 'connecting') return { color: '#eab308', label: 'Connecting...' };
    if (status === 'error') return { color: '#ef4444', label: 'Error' };
    if (enabled) return { color: '#3b82f6', label: 'Ready' };
    return { color: '#6b7280', label: 'Offline' };
  };
  
  const { color, label } = getStatusInfo();
  
  return (
    <button 
      onClick={onClick}
      className="network-badge"
      title="Network settings"
    >
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={color} strokeWidth="2">
        <circle cx="12" cy="12" r="10" />
        <path d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" />
      </svg>
      <span style={{ color }}>{label}</span>
    </button>
  );
};

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

function getNetworkLed(netStatus: NetworkStatus, vmStatus: VMStatus, netEnabled: boolean): string {
  if (vmStatus === 'off' && netEnabled) return "led-on-blue";
  switch (netStatus) {
    case "connected": return "led-on-green";
    case "connecting": return "led-on-amber";
    case "error": return "led-on-red";
    default: return "led-off";
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
    shutdownVM,
    networkStatus,
    networkEnabled,
    relayUrl,
    certHash,
    updateRelayUrl,
    updateCertHash,
    connectNetwork,
    disconnectNetwork,
    toggleNetworkEnabled,
  } = useVM();
  
  const endRef = useRef<HTMLDivElement>(null);
  const [selectedKernel, setSelectedKernel] = useState<KernelType>("custom");
  const [showNetworkPanel, setShowNetworkPanel] = useState(false);
  const [localRelayUrl, setLocalRelayUrl] = useState(relayUrl);
  const [localCertHash, setLocalCertHash] = useState(certHash);

  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [output]);

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

  const handleNetworkToggle = () => {
    if (status === 'off') {
      updateRelayUrl(localRelayUrl);
      updateCertHash(localCertHash);
      toggleNetworkEnabled(!networkEnabled);
    } else if (networkStatus === 'connected') {
      disconnectNetwork();
    } else if (networkStatus === 'disconnected' && status === 'running') {
      updateRelayUrl(localRelayUrl);
      updateCertHash(localCertHash);
      connectNetwork(localRelayUrl, localCertHash);
    }
  };

  const leds = getStatusLed(status);
  const netLed = getNetworkLed(networkStatus, status, networkEnabled);
  const isOn = status === "running" || status === "booting";

  return (
    <div className="scene">
      {/* Ambient glow effects */}
      <div className="ambient-glow" />
      
      <div className="content-wrapper">
        <Computer3D 
          isOn={isOn}
          powerLed={leds.power}
          actLed={leds.activity}
          netLed={netLed}
          onPowerClick={handlePowerClick}
        >
          {/* CRT Screen */}
          <div className="crt-screen-3d">
            <div className="screen-content-3d" tabIndex={0}>
              {status === "off" && (
                <div className="screen-off-state">
                  <div className="boot-prompt">
                    <span className="boot-icon">⏻</span>
                    <span>Press power to boot</span>
                  </div>
                </div>
              )}
              {status === "booting" && (
                <div className="screen-boot-state">
                  <div className="boot-animation">
                    <div className="boot-spinner" />
                    <span>Booting {selectedKernel === "custom" ? "Custom Kernel" : "xv6"}...</span>
                  </div>
                </div>
              )}
              {status === "error" && (
                <div className="screen-error-state">
                  <span className="error-icon">⚠</span>
                  <span className="error-title">SYSTEM ERROR</span>
                  <span className="error-msg">{errorMessage}</span>
                  <span className="error-hint">Press power to restart</span>
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
        </Computer3D>

        {/* Control Panel (below computer) */}
        <div className="control-panel">
          {/* Kernel selector */}
          <div className="control-group">
            <label className="control-label">BOOT</label>
            <select
              value={selectedKernel}
              onChange={(e) => setSelectedKernel(e.target.value as KernelType)}
              disabled={isOn}
              className="kernel-select-3d"
            >
              <option value="custom_kernel">Custom Kernel</option>
              <option value="kernel">xv6 Linux</option>
            </select>
          </div>

          {/* Stats */}
          <div className="stats-group">
            <div className="stat-item">
              <span className="stat-label">CPU</span>
              <div className="stat-bar">
                <div className="stat-fill cpu" style={{ width: `${cpuLoad}%` }} />
              </div>
              <span className="stat-value">{cpuLoad.toFixed(0)}%</span>
            </div>
            <div className="stat-item">
              <span className="stat-label">MEM</span>
              <div className="stat-bar">
                <div className="stat-fill mem" style={{ width: `${Math.min(100, (memUsage / (128 * 1024 * 1024)) * 100)}%` }} />
              </div>
              <span className="stat-value">{(memUsage / (1024 * 1024)).toFixed(0)}M</span>
            </div>
          </div>

          {/* Network */}
          <div className="control-group">
            <NetworkBadge 
              status={networkStatus} 
              enabled={networkEnabled}
              onClick={() => setShowNetworkPanel(!showNetworkPanel)}
            />
          </div>
        </div>

        {/* Network Panel (expandable) */}
        {showNetworkPanel && (
          <div className="network-panel">
            <div className="network-panel-header">
              <span className="network-panel-title">Network Configuration</span>
              <button onClick={() => setShowNetworkPanel(false)} className="close-btn">×</button>
            </div>
            <div className="network-panel-body">
              <div className="network-row">
                <label>Relay URL</label>
                <input
                  type="text"
                  value={localRelayUrl}
                  onChange={(e) => setLocalRelayUrl(e.target.value)}
                  disabled={isOn}
                  placeholder="https://localhost:4433"
                  className="network-input"
                />
              </div>
              <div className="network-row">
                <label>Cert Hash</label>
                <input
                  type="text"
                  value={localCertHash}
                  onChange={(e) => setLocalCertHash(e.target.value)}
                  disabled={isOn}
                  placeholder="(optional, for self-signed certs)"
                  className="network-input"
                />
              </div>
              <div className="network-row">
                <span className="network-status-text">
                  {status === 'off' 
                    ? (networkEnabled ? '✓ Will connect on boot' : '○ Disabled')
                    : networkStatus === 'connected' ? '● Connected' : '○ Disconnected'
                  }
                </span>
                <button
                  onClick={handleNetworkToggle}
                  disabled={networkStatus === 'connecting'}
                  className={`network-toggle-btn ${networkEnabled || networkStatus === 'connected' ? 'active' : ''}`}
                >
                  {status === 'off' 
                    ? (networkEnabled ? 'Disable' : 'Enable')
                    : (networkStatus === 'connected' ? 'Disconnect' : 'Connect')}
                </button>
              </div>
            </div>
          </div>
        )}

        {/* Footer info */}
        <div className="footer-info">
          <span>RISC-V 64-bit</span>
          <span className="sep">•</span>
          <span>128 MiB</span>
          <span className="sep">•</span>
          <span>{currentKernel === "kernel" ? "xv6" : currentKernel === "custom" ? "Custom" : "Ready"}</span>
          {networkStatus === 'connected' && (
            <>
              <span className="sep">•</span>
              <span className="net-active">Online</span>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
