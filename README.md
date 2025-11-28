# RISK-V

<div align="center">

![RISC-V](https://img.shields.io/badge/RISC--V-64--bit-blue)
![Rust](https://img.shields.io/badge/Rust-1.70+-orange)
![WASM](https://img.shields.io/badge/WASM-Supported-green)

**A modern, modular RISC-V virtualization ecosystem built in Rust.**

[Web Demo](https://linux.jribo.kiwi) • [Documentation](#documentation) • [Components](#components)

</div>
<div style="width:100%">
<img width="917" height="943" alt="Screenshot 2025-11-28 at 21 49 31" src="https://github.com/user-attachments/assets/9143c834-5f6d-453e-bf4b-065dc64fb854" style="margin:0 auto;" />
</div>

---

## Overview

**RISK-V** is a high-performance emulator and operating system environment designed to bring the RISC-V architecture to the browser and desktop. It features a complete RV64GC virtual machine, a custom bare-metal kernel, and a peer-to-peer networking relay, all written in Rust.

Whether you want to run Linux in your browser, learn about OS development, or experiment with networked virtual machines, RISK-V provides the tools you need.

## Components

This repository is organized as a workspace containing several loosely coupled components:

### 🖥️ [Virtual Machine (`riscv-vm`)](./riscv-vm/README.md)
The core emulator implementing the RISC-V 64-bit instruction set (RV64GC).
- **Features**: MMU, VirtIO, UART, PLIC, CLINT.
- **Targets**: WebAssembly (Browser) and Native (CLI).
- **Networking**: WebSocket, WebTransport, and TAP backends.

### 🐚 [Kernel (`kernel`)](./kernel/README.md)
A custom bare-metal operating system kernel written in Rust.
- **Features**: TCP/IP stack, Interactive CLI, Heap Allocator.
- **Purpose**: Demonstrates VM capabilities and provides a lightweight runtime environment.

### 📡 [Relay (`relay`)](./relay/README.md)
A P2P WebTransport relay server.
- **Features**: Enables browser-to-browser and browser-to-internet networking.
- **Role**: Acts as a NAT gateway and signaling server for VM instances.

## Quick Start

### 1. Build the Project

Ensure you have Rust and the RISC-V target installed:

```bash
rustup target add riscv64gc-unknown-none-elf
sh ./build.sh
```

### 2. Run the Kernel

Boot the custom kernel in the emulator:

```bash
cargo run -p riscv-vm --release -- --kernel target/riscv64gc-unknown-none-elf/release/kernel
```

### 3. Enable Networking

To enable networking, first start the relay server (or use a public one):

```bash
# Terminal 1: Start Relay
cargo run -p relay --release

# Terminal 2: Run VM with networking
cargo run -p riscv-vm --release -- \
  --kernel target/riscv64gc-unknown-none-elf/release/kernel \
  --net-webtransport https://127.0.0.1:4433 \
  --net-cert-hash <HASH_FROM_RELAY_OUTPUT>
```

## Architecture

The system emulates a standard RISC-V board with the following memory map:

| Address | Device | Description |
|---------|--------|-------------|
| `0x0010_0000` | Test | Test Finisher |
| `0x0200_0000` | CLINT | Core Local Interruptor |
| `0x0C00_0000` | PLIC | Platform Interrupt Controller |
| `0x1000_0000` | UART | Serial Console |
| `0x1000_1000` | VirtIO | Block Device (Disk) |
| `0x1000_2000` | VirtIO | Network Device |
| `0x8000_0000` | DRAM | Main Memory (512 MiB) |

## License

MIT License. Made with ❤️ and Rust.
