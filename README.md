# RISK-V — RISC-V Virtual Machine

<div align="center">

![RISC-V](https://img.shields.io/badge/RISC--V-64--bit-blue)
![Rust](https://img.shields.io/badge/Rust-1.70+-orange)
![WebAssembly](https://img.shields.io/badge/WebAssembly-Supported-green)
![License](https://img.shields.io/badge/License-MIT-yellow)

**A fully-featured RISC-V 64-bit virtual machine written in Rust that runs in your browser.**

[Features](#features) • [Quick Start](#quick-start) • [Architecture](#architecture) • [Building](#building-from-source)

</div>

---

## Overview

RISK-V is a RISC-V RV64GC virtual machine emulator built entirely in Rust. It can boot real operating systems like xv6 (a Unix-like teaching OS) or custom bare-metal kernels. The VM compiles to WebAssembly, allowing you to run RISC-V operating systems directly in your web browser with a retro-style CRT terminal interface.

### What Makes This Special?

- **Browser-Native**: Run a full RISC-V virtual machine in any modern browser via WebAssembly
- **Real OS Support**: Boot xv6 Linux or custom kernels — not just toy programs
- **Educational**: Learn about CPU architecture, operating systems, and low-level programming
- **Retro UI**: Beautiful CRT-style terminal that looks like a classic computer

---

## Features

### Virtual Machine
- **Full RV64GC ISA**: Implements the complete RISC-V 64-bit instruction set with:
  - **I** (Base Integer)
  - **M** (Multiplication/Division)
  - **A** (Atomics)
  - **F/D** (Single/Double Floating Point)
  - **C** (Compressed Instructions)
  - **Zicsr/Zifencei** (CSR and Fence extensions)
- **Memory Management Unit (MMU)**: Sv39 virtual memory with page table walking
- **Privilege Modes**: Machine (M), Supervisor (S), and User (U) modes
- **Interrupt Controller**: PLIC (Platform-Level Interrupt Controller) and CLINT (Core Local Interruptor)
- **UART**: 16550-compatible serial console for I/O
- **VirtIO**: Block and Network device support
- **Networking**: Multiple backends (libp2p, WebSocket, TAP) with NAT traversal

### Web Interface
- **Retro CRT Display**: Authentic scanline effect and phosphor glow
- **Kernel Selection**: Choose between custom kernel or xv6
- **Power Controls**: Boot, shutdown, and restart the VM
- **System Monitor**: Real-time CPU load and memory usage display
- **LED Indicators**: Power and activity status lights

### Custom Kernel
- **Bare-metal Rust**: Written in `no_std` Rust for the RISC-V target
- **Interactive CLI**: Built-in command shell with various commands
- **Heap Allocator**: Simple bump allocator for dynamic memory
- **UART Driver**: Direct hardware access for console I/O

---

## Quick Start

### Running in Browser

Visit the deployed web application or run locally:

```bash
# Clone the repository
git clone https://github.com/yourusername/risk-v.git
cd risk-v

# Install dependencies
cd web && yarn install && cd ..

# Build everything (kernel + WASM VM)
./build.sh

# Start development server
cd web && yarn dev
```

Open http://localhost:3000 in your browser, select a kernel, and press the power button!

### Running from Command Line

```bash
# Run with xv6 kernel and disk image
cargo run --release -- --kernel xv6/kernel --disk xv6/fs.img

# Run with custom kernel (no disk needed)
cargo run --release -- --kernel target/riscv64gc-unknown-none-elf/release/kernel
```

---

## Networking

The VM supports networking through VirtIO-net with multiple backend options. A libp2p-based relay service enables NAT traversal, peer-to-peer connectivity, and **external network access** (ping, DNS).

### Network Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│   VM Instance   │     │  libp2p Relay   │     │   VM Instance   │
│  ┌───────────┐  │     │ + NAT Gateway   │     │  ┌───────────┐  │
│  │  Kernel   │  │     │  • QUIC (4001)  │     │  │  Kernel   │  │
│  │ (smoltcp) │  │     │  • TCP  (4002)  │     │  │ (smoltcp) │  │
│  └─────┬─────┘  │     │  • WS   (8765)  │     │  └─────┬─────┘  │
│        │        │     │  • Gossipsub    │     │        │        │
│  ┌─────┴─────┐  │     │  • NAT Gateway  │     │  ┌─────┴─────┐  │
│  │ VirtIO-net│  │◄───►│  • Circuit Relay│◄───►│  │ VirtIO-net│  │
│  └───────────┘  │     │  • DCUtR        │     │  └───────────┘  │
└─────────────────┘     │  • Kademlia DHT │     └─────────────────┘
        │               └────────┬────────┘
        │ NAT                    │ NAT
        ▼                        ▼
┌──────────────────────────────────────────────────────────────────┐
│                        External Internet                          │
│               (ping 8.8.8.8, nslookup google.com)                │
└──────────────────────────────────────────────────────────────────┘
```

### Starting the Relay Service

The relay enables VM-to-VM networking and NAT traversal:

```bash
# Build the relay
cargo build --release -p relay

# Start the relay (default ports: QUIC 4001, TCP 4002)
./target/release/relay

# With custom ports
./target/release/relay --port 4001 --tcp-port 4002

# With external address announcement (for public servers)
./target/release/relay --external-addr "/ip4/YOUR_PUBLIC_IP/udp/4001/quic-v1"
```

The relay will output its **Peer ID** on startup — you'll need this to connect VMs:

```
Local peer ID: 12D3KooWSkQfZmVU4FJRErMfNgXvK7YWhR67te6weS8nVuNoE2Mf
Listening on QUIC: /ip4/0.0.0.0/udp/4001/quic-v1
Listening on TCP: /ip4/0.0.0.0/tcp/4002
```

### Connecting VMs to the Relay

#### Option 1: libp2p (Recommended)

Connect via QUIC for best performance and NAT traversal:

```bash
# Connect via QUIC (replace PEER_ID with the relay's peer ID)
./target/release/riscv-vm --kernel kernel \
    --net-libp2p "/ip4/127.0.0.1/udp/4001/quic-v1/p2p/12D3KooW..."

# Connect via TCP (fallback)
./target/release/riscv-vm --kernel kernel \
    --net-libp2p "/ip4/127.0.0.1/tcp/4002/p2p/12D3KooW..."

# Connect to remote relay
./target/release/riscv-vm --kernel kernel \
    --net-libp2p "/ip4/RELAY_IP/udp/4001/quic-v1/p2p/PEER_ID"
```

#### Option 2: WebSocket (Legacy/Browser)

For browser compatibility or simpler setups:

```bash
# Start the old WebSocket relay (if needed)
# ./target/release/relay-ws --port 8765

# Connect VM via WebSocket
./target/release/riscv-vm --kernel kernel \
    --net-ws "ws://localhost:8765"
```

#### Option 3: TAP Device (Linux only)

For direct host networking:

```bash
# Create TAP interface
sudo ip tuntap add dev tap0 mode tap
sudo ip addr add 10.0.2.1/24 dev tap0
sudo ip link set tap0 up

# Run VM with TAP
./target/release/riscv-vm --kernel kernel --net-tap tap0
```

#### Option 4: Dummy Backend (Testing)

For testing without actual networking:

```bash
./target/release/riscv-vm --kernel kernel --net-dummy
```

### Relay Features

| Feature | Description |
|---------|-------------|
| **QUIC Transport** | Fast, secure connections (UDP port 4001) |
| **TCP Transport** | Fallback for restricted networks (port 4002) |
| **WebSocket Bridge** | Browser compatibility (port 8765) |
| **NAT Gateway** | Routes external traffic (ICMP/ping, UDP/DNS) to real internet |
| **Circuit Relay v2** | NAT traversal for peers behind firewalls |
| **DCUtR** | Direct Connection Upgrade through Relay (hole punching) |
| **Gossipsub** | Pub/sub messaging for Ethernet frame broadcast |
| **Kademlia DHT** | Distributed peer discovery |
| **AutoNAT** | Automatic NAT detection |

### Supported Scenarios

| Scenario | Transport | How it Works |
|----------|-----------|--------------|
| **Server ↔ Server** | Direct QUIC | Peers connect directly when possible |
| **Behind NAT** | Circuit Relay + DCUtR | Initial relay, then hole punching |
| **Browser → Server** | TCP/WebSocket | Via relay bridge |
| **Browser ↔ Browser** | Circuit Relay | Full relay for browser-to-browser |

### Docker Deployment

Run the relay in Docker:

```bash
# Build the image
cd relay
docker build -t riscv-relay .

# Run the container
docker run -d \
    --name riscv-relay \
    -p 4001:4001/udp \
    -p 4002:4002 \
    riscv-relay

# With custom configuration
docker run -d \
    --name riscv-relay \
    -p 4001:4001/udp \
    -p 4002:4002 \
    -e RUST_LOG=info \
    riscv-relay \
    /app/relay --port 4001 --tcp-port 4002
```

### Kernel Network Configuration

The custom kernel uses these default network settings:

| Setting | Value |
|---------|-------|
| VM IP Address | `10.0.2.15` |
| Gateway | `10.0.2.2` |
| Subnet Mask | `255.255.255.0` |
| DNS Server | `8.8.8.8` |

Test networking from the kernel:

```bash
# Ping the gateway (internal, always works)
ping 10.0.2.2

# Ping external host (requires NAT gateway - relay or native backend)
ping 8.8.8.8

# DNS lookup (requires NAT gateway)
nslookup google.com

# Ping resolved hostname
ping google.com
```

---

## Architecture

### System Overview

```
┌─────────────────────────────────────────────────────────────┐
│                      Web Browser                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │                 React Frontend                       │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  │    │
│  │  │  CRT Screen │  │   Controls  │  │   Status    │  │    │
│  │  └─────────────┘  └─────────────┘  └─────────────┘  │    │
│  └─────────────────────────────────────────────────────┘    │
│                            │                                 │
│                            ▼                                 │
│  ┌─────────────────────────────────────────────────────┐    │
│  │              WebAssembly VM (Rust → WASM)            │    │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌────────┐  │    │
│  │  │   CPU   │  │   MMU   │  │   Bus   │  │ Devices│  │    │
│  │  │ RV64GC  │  │  Sv39   │  │         │  │        │  │    │
│  │  └─────────┘  └─────────┘  └─────────┘  └────────┘  │    │
│  └─────────────────────────────────────────────────────┘    │
│                            │                                 │
│                            ▼                                 │
│  ┌─────────────────────────────────────────────────────┐    │
│  │                   Guest Kernel                       │    │
│  │            (xv6 Linux or Custom Kernel)              │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
```

### Memory Map

| Address Range | Size | Device |
|--------------|------|--------|
| `0x0010_0000` | 4 KiB | Test Finisher (HTIF) |
| `0x0200_0000` | 64 KiB | CLINT (Timer/IPI) |
| `0x0C00_0000` | 64 MiB | PLIC (Interrupt Controller) |
| `0x1000_0000` | 256 B | UART (Serial Console) |
| `0x1000_1000` | 4 KiB | VirtIO Block Device |
| `0x1000_2000` | 4 KiB | VirtIO Network Device |
| `0x8000_0000` | 512 MiB | DRAM (Main Memory) |

### CPU Pipeline

The CPU executes instructions in a simple fetch-decode-execute cycle:

1. **Fetch**: Read instruction from memory at PC
2. **Decode**: Parse opcode, registers, and immediates
3. **Execute**: Perform ALU operation or memory access
4. **Writeback**: Store result to register file
5. **Interrupt Check**: Handle pending interrupts/exceptions

---

## Project Structure

```
risk-v/
├── riscv-vm/               # Virtual Machine (Rust)
│   ├── src/
│   │   ├── main.rs         # CLI entry point
│   │   ├── lib.rs          # WASM bindings
│   │   ├── cpu.rs          # RV64GC CPU implementation
│   │   ├── decoder.rs      # Instruction decoder
│   │   ├── csr.rs          # Control/Status Registers
│   │   ├── mmu.rs          # Memory Management Unit
│   │   ├── bus.rs          # System bus & address routing
│   │   ├── dram.rs         # DRAM memory
│   │   ├── uart.rs         # 16550 UART emulation
│   │   ├── clint.rs        # Core Local Interruptor
│   │   ├── plic.rs         # Platform Interrupt Controller
│   │   ├── virtio.rs       # VirtIO block & net devices
│   │   ├── net.rs          # Network backend trait
│   │   ├── net_tap.rs      # TAP network backend (Linux)
│   │   ├── net_ws.rs       # WebSocket network backend
│   │   ├── net_libp2p.rs   # libp2p network backend
│   │   └── emulator.rs     # High-level emulator wrapper
│   └── tests/              # Integration tests
│
├── relay/                  # libp2p Relay Service
│   ├── src/
│   │   └── main.rs         # Relay server implementation
│   ├── Cargo.toml          # Dependencies (libp2p, tokio)
│   └── Dockerfile          # Container deployment
│
├── kernel/                 # Custom RISC-V Kernel (Rust)
│   ├── src/
│   │   ├── main.rs         # Kernel entry & CLI
│   │   ├── uart.rs         # UART driver
│   │   ├── allocator.rs    # Heap allocator
│   │   ├── net.rs          # Network stack (smoltcp)
│   │   ├── virtio_net.rs   # VirtIO network driver
│   │   └── dns.rs          # DNS resolver
│   ├── memory.x            # Memory layout
│   ├── link.x              # Linker script
│   └── .cargo/config.toml  # Build target config
│
├── web/                    # Web Frontend (Next.js)
│   ├── src/
│   │   ├── app/
│   │   │   ├── page.tsx    # Main UI component
│   │   │   ├── layout.tsx  # App layout
│   │   │   └── globals.css # Styling
│   │   ├── hooks/
│   │   │   └── useVM.ts    # VM React hook
│   │   └── pkg/            # Generated WASM bindings
│   └── public/
│       ├── kernel          # xv6 kernel binary
│       ├── custom_kernel   # Custom kernel binary
│       ├── fs.img          # xv6 filesystem image
│       └── riscv_vm_bg.wasm
│
├── xv6/                    # xv6 kernel & disk image
│   ├── kernel              # Compiled xv6 kernel
│   ├── kernel.asm          # Disassembly for debugging
│   └── fs.img              # Filesystem image
│
├── build.sh                # Build script
└── Cargo.toml              # Workspace manifest
```

---

## Building from Source

### Prerequisites

- **Rust** (1.70+): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **RISC-V Target**: `rustup target add riscv64gc-unknown-none-elf`
- **wasm-pack**: `cargo install wasm-pack` or `npm install -g wasm-pack`
- **Node.js** (18+) and **Yarn**

### Build Everything

```bash
# Full build: kernel + VM + WASM + copy artifacts
./build.sh
```

### Build Individual Components

```bash
# Build the custom kernel
cd kernel
cargo build --target riscv64gc-unknown-none-elf --release

# Build the VM for native
cd vm
cargo build --release

# Build the VM for WebAssembly
cd vm
wasm-pack build --target web --out-dir ../web/src/pkg

# Run the web frontend
cd web
yarn dev
```

### Running Tests

```bash
# Run VM tests
cargo test

# Run specific test suite
cargo test --package riscv-vm
```

---

## Deployment

### GitHub Pages (Automatic)

This project is configured for automatic deployment to GitHub Pages on every push to the `main` branch.

#### Setup Instructions

1. **Enable GitHub Pages** in your repository settings:
   - Go to **Settings** → **Pages**
   - Under "Build and deployment", select **GitHub Actions** as the source

2. **Push to main branch**:
   ```bash
   git add .
   git commit -m "Your commit message"
   git push origin main
   ```

3. **Access your site** at: `https://yourusername.github.io/risk-v/`

#### What the CI/CD Pipeline Does

The GitHub Actions workflow (`.github/workflows/deploy.yml`) automatically:

1. ✅ Sets up Rust with the RISC-V target
2. ✅ Builds the custom kernel for `riscv64gc-unknown-none-elf`
3. ✅ Installs `wasm-pack` and builds the VM to WebAssembly
4. ✅ Copies all artifacts to the web directory
5. ✅ Builds the Next.js static site
6. ✅ Deploys to GitHub Pages

#### Manual Deployment

To manually trigger a deployment:

1. Go to **Actions** tab in your repository
2. Select "Deploy to GitHub Pages" workflow
3. Click **Run workflow** → **Run workflow**

### Custom Domain (Optional)

To use a custom domain:

1. Add a `CNAME` file in `web/public/` with your domain:
   ```
   vm.yourdomain.com
   ```

2. Configure DNS with your domain provider:
   - Add a CNAME record pointing to `yourusername.github.io`

3. Enable HTTPS in repository Settings → Pages

---

## How It Works

### Instruction Execution

The VM interprets RISC-V instructions one at a time. Here's a simplified flow:

```rust
// Fetch instruction
let inst = self.bus.read32(self.pc)?;

// Decode opcode
let opcode = inst & 0x7f;

match opcode {
    0b0110011 => self.execute_r_type(inst),  // ADD, SUB, etc.
    0b0010011 => self.execute_i_type(inst),  // ADDI, etc.
    0b0000011 => self.execute_load(inst),    // LB, LW, LD
    0b0100011 => self.execute_store(inst),   // SB, SW, SD
    0b1100011 => self.execute_branch(inst),  // BEQ, BNE, etc.
    // ... more opcodes
}
```

### Virtual Memory (Sv39)

The MMU translates virtual addresses to physical addresses using a 3-level page table:

```
Virtual Address (39 bits):
┌─────────┬─────────┬─────────┬──────────────┐
│ VPN[2]  │ VPN[1]  │ VPN[0]  │   Offset     │
│ 9 bits  │ 9 bits  │ 9 bits  │   12 bits    │
└─────────┴─────────┴─────────┴──────────────┘
     │         │         │
     ▼         ▼         ▼
  Level 2   Level 1   Level 0    Physical
  Page Tbl  Page Tbl  Page Tbl   Address
```

### Interrupt Handling

When an interrupt occurs:

1. Save current PC to `mepc` (or `sepc`)
2. Set exception cause in `mcause` (or `scause`)
3. Jump to trap handler at `mtvec` (or `stvec`)
4. Kernel handles the interrupt
5. `mret`/`sret` returns to interrupted code

---

## Custom Kernel Commands

When running the custom kernel, these commands are available:

| Command | Description |
|---------|-------------|
| `help` | Show available commands |
| `hello` | Increment and print counter |
| `count` | Show current counter value |
| `echo <text>` | Print text back |
| `clear` | Clear screen (print newlines) |
| `alloc <bytes>` | Allocate heap memory (for testing) |

---

## Debugging

### xv6 Disassembly

The `xv6/kernel.asm` file contains the disassembled kernel, useful for debugging:

```bash
# View specific function
grep -A 50 "^[0-9a-f]* <scheduler>:" xv6/kernel.asm
```

### Enable VM Logging

```bash
# Run with trace logging
RUST_LOG=trace cargo run --release -- --kernel xv6/kernel --disk xv6/fs.img
RUST_LOG=trace cargo run --release -- --kernel target/riscv64gc-unknown-none-elf/release/kernel
```

### Common Issues

**Kernel won't boot**: Check that memory layout in `memory.x` matches VM's DRAM (128 MiB starting at `0x8000_0000`).

**No UART output**: Ensure UART address (`0x1000_0000`) matches between kernel and VM.

**Page fault**: Virtual memory translation failed. Check page table setup in kernel.

---

## Contributing

Contributions are welcome! Areas of interest:

- [ ] Implement more RISC-V extensions (V for vectors, B for bit manipulation)
- [x] ~~Add networking (VirtIO-net)~~ — **Done!** libp2p relay with QUIC, NAT traversal
- [ ] Improve performance with JIT compilation
- [ ] Add GDB stub for debugging
- [ ] Port more operating systems
- [ ] WebRTC transport for browser-to-browser networking

---

## References

- [RISC-V Specifications](https://riscv.org/technical/specifications/)
- [xv6 Book](https://pdos.csail.mit.edu/6.828/2023/xv6/book-riscv-rev3.pdf)
- [RISC-V Privileged Architecture](https://github.com/riscv/riscv-isa-manual)
- [VirtIO Specification](https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.html)

---

## License

MIT License — feel free to use this for learning, teaching, or building upon.

---

<div align="center">
Made with ❤️ and Rust
</div>
