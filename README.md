## Overview

This repository contains a **Rust-on-Rust RISC-V system**:

- **`vm`**: a Rust RISC-V **RV64IMAC** emulator that can run natively on your machine or compiled to **Wasm** for the browser.
- **`kernel`**: a bare‑metal **no_std** Rust program compiled to `riscv64imac-unknown-none-elf`.
- **`web`**: a Next.js UI that runs the VM in the browser and exposes a simple terminal‑like interface.

The demo kernel:

- Boots and prints: `Booting RISC-V kernel...`
- Polls a UART at `0x1000_0000` for input bytes.
- On every `Enter` (`\n` or `\r`), increments a counter and prints: `Hello, count N`.

## Prerequisites

- **Rust** (stable) with Cargo.
- The **RISC-V target**:

```bash
rustup target add riscv64imac-unknown-none-elf
```

- **Node.js + npm** (for the `web` app).
- `wasm-pack` (used by `build.sh`):

```bash
cargo install wasm-pack
```

## Building the RISC-V kernel

The kernel is configured via `kernel/.cargo/config.toml` to target `riscv64imac-unknown-none-elf` and disable compressed instruction generation (`target-feature=-c`).

Build the kernel:

```bash
cd kernel
cargo build --release
```

This produces:

- `target/riscv64imac-unknown-none-elf/release/kernel`

## Running the VM natively (CLI)

From the workspace root:

```bash
cargo run -p vm -- target/riscv64imac-unknown-none-elf/release/kernel
```

You should see:

- `Booting RISC-V kernel...`
- After that, the VM will sit in a loop polling the UART. When wired up to a UART implementation that reads from stdin, each `Enter` would trigger `Hello, count N` messages.

## Building the Web/Wasm demo

The `build.sh` script automates:

- Building the RISC-V kernel.
- Building the `vm` crate to Wasm via `wasm-pack`.
- Copying the artifacts into the Next.js app’s `public`/`src/pkg` folders.

From the workspace root:

```bash
./build.sh
```

What it does:

- `kernel`:
  - `cargo build --release`
  - Copies `target/riscv64imac-unknown-none-elf/release/kernel` to `web/public/kernel.bin`
- `vm`:
  - `npx wasm-pack build --target web --out-dir ../web/src/pkg`
  - Copies `web/src/pkg/vm_bg.wasm` to `web/public/vm_bg.wasm`

## Running the Web UI

After `./build.sh` completes:

```bash
cd web
npm install          # first time only
npm run dev
```

Then open `http://localhost:3000` in your browser.

You should see:

- A black terminal‑style UI labeled **“RISC-V VM”**.
- Status `[Running]` when the Wasm VM has started and the kernel is loaded.
- Initial text: `Booting RISC-V kernel...`

### Interacting with the VM

- Simply **type anywhere on the page**; keypresses are captured globally.
- Keys are sent to the VM’s UART:
  - Regular printable characters are sent as their byte value.
  - `Enter` sends `\n` (ASCII 10).
- The kernel logic:
  - Ignores `0` bytes (no input).
  - Buffers other bytes.
  - On `Enter` or `\r`:
    - Increments an internal counter.
    - Prints `Hello, count N` via the UART.
    - Clears the buffer.

If you previously saw crashes like:

```text
Crashed: Unimplemented compressed instruction (Q1): 0x8191
```

those are now handled by the improved VM CPU implementation, which supports more RV64I/RV64W opcodes and safely no‑ops unknown compressed encodings used by toolchains. Rebuild with `./build.sh` and refresh the browser to pick up the latest VM.

## Project Structure (high level)

- **`Cargo.toml`**: workspace root for `vm` and `kernel`.
- **`vm/`**:
  - `src/cpu.rs`: RV64IMAC CPU core (including some compressed instruction handling).
  - `src/bus.rs`, `src/dram.rs`, `src/uart.rs`: memory + UART model.
  - `src/loader.rs`: ELF / raw image loader.
  - `src/lib.rs`: Wasm bindings (`WasmVm`), used by the web app.
  - `src/main.rs`: CLI entrypoint.
- **`kernel/`**:
  - `src/main.rs`: `#[entry]` function and UART loop (`println!("Hello, count {}", count)`).
  - `src/uart.rs`: simple MMIO UART driver at `0x1000_0000`.
  - `memory.x`, `link.x`: memory layout and linker script.
- **`web/`**:
  - `src/app/page.tsx`: React terminal UI.
  - `src/hooks/useVM.ts`: loads `vm_bg.wasm`, starts the VM loop, forwards key input and renders UART output.

## Notes / Gotchas

- Always run `./build.sh` after changing the VM or kernel so the web demo picks up the new artifacts.
- The UART model is intentionally minimal:
  - Reads: return the next input byte if available, otherwise `0`.
  - Writes: enqueue bytes into an output buffer, which the web UI drains every frame.
- The CPU currently does **not** implement the full RISC-V spec (no MMU, no floating point, no atomics), but it is sufficient for this text‑based demo kernel.