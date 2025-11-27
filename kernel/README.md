# Custom RISC-V Kernel

A lightweight, bare-metal operating system kernel written in Rust for the RISC-V architecture. It serves as a demonstration of the VM's capabilities, featuring a command-line interface, memory management, and a TCP/IP networking stack.

## Features

- **Pure Rust**: Built with `#![no_std]` for bare-metal execution.
- **Networking**: Full TCP/IP stack via `smoltcp` driver for VirtIO-Net.
- **Memory Management**: Dynamic heap allocation using a linked-list allocator.
- **Interactive Shell**: Built-in UART console with command history and editing.
- **Device Drivers**:
  - VirtIO Network (Net)
  - UART Console
  - CLINT Timer

## Commands

The kernel boots into an interactive shell supporting the following commands:

| Command | Description |
|---------|-------------|
| `help` | Show available commands |
| `ip addr` | Display network interface configuration (IP/MAC/Gateway) |
| `ping <addr>` | Send ICMP Echo requests to an IP or hostname |
| `nslookup <host>` | Resolve a hostname to an IP address using DNS |
| `netstat` | Show network device status |
| `alloc <bytes>` | Allocate memory on the heap (debug) |
| `memstats` | Show heap usage statistics |
| `memtest` | Run memory allocation/deallocation stress tests |
| `clear` | Clear the screen |

## Building

To build the kernel, you need the RISC-V target installed:

```bash
rustup target add riscv64gc-unknown-none-elf
```

Build the kernel binary:

```bash
cargo build --release
```

The artifact will be located at `../target/riscv64gc-unknown-none-elf/release/kernel`.

## Running

You can run this kernel using the `riscv-vm` emulator:

```bash
cargo run -p riscv-vm --release -- --kernel target/riscv64gc-unknown-none-elf/release/kernel
```

