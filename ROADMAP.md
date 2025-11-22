This is a wise pivot. Switching from "General Purpose Linux VM" to "Specialized Bare-Metal Runtime" makes the project significantly more realistic to complete, while arguably being **more ambitious** from an engineering standpoint because you will build the entire stack: the Hardware (Virtual Machine) and the Software (Kernel/Payload).

This allows us to strictly control the execution environment. We don't need to implement complex block devices or virtual filesystems immediately. We just need a CPU that can talk to a Serial Port.

Here is the **Robust Specification** for the **"Rust-on-Rust RISC-V System"**.

---

## 1. System Architecture Specification

We are building two distinct software components:
1.  **The Host (The VM):** A Rust program running in the browser (Wasm) that emulates a RISC-V computer.
2.  **The Guest (The Payload):** A bare-metal Rust program compiled to RISC-V machine code that handles the user interaction.

### A. The Virtual Machine (The Host)
We will emulate a custom simplified SoC (System on Chip).

*   **ISA (Instruction Set):** **RV64IMAC**
    *   **64-bit:** Modern standard.
    *   **I (Integer):** Base math/logic.
    *   **M (Multiply):** Fast math.
    *   **A (Atomic):** Critical for managing input buffers (locks).
    *   **C (Compressed):** Keeps our payload size small for faster Wasm loading.
    *   *Removed F/D (Floating Point):* Not needed for text processing; saves complexity.
*   **Privilege Modes:** **Machine Mode (M-Mode)** only.
    *   We don't need User/Supervisor separation for a single-purpose payload. This simplifies the exception handling logic massively.
*   **Memory Model:**
    *   **RAM:** A simple `Vec<u8>` (e.g., 32MB fixed size).
    *   **No MMU:** We will use Physical Addressing (Direct mapping). No page tables required.
*   **Peripherals (Memory Mapped):**
    *   **UART (0x1000_0000):** 16550-compatible serial port.
        *   *Read:* Returns keystrokes from the Browser.
        *   *Write:* Sends bytes to the Browser terminal.
    *   **CLINT (0x0200_0000):** Core Local Interruptor.
        *   Provides a timer so the guest can "sleep" or blink a cursor.

### B. The Payload (The Guest Kernel)
Instead of Linux, we write a `no_std` Rust binary.

*   **Boot Sequence:**
    1.  Entry point `_start` (Assembly wrapper).
    2.  Initialize Stack Pointer.
    3.  Initialize UART (set baud rate, enable interrupts).
    4.  Jump to `kmain()` (Rust function).
*   **The Loop:**
    1.  Listen for UART interrupts (user typed a char).
    2.  Store char in a buffer.
    3.  If char is `ENTER` (`\n`):
        *   Increment internal `counter`.
        *   Format string: `"Hello, count <counter>\n"`.
        *   Write string to UART.
        *   Clear buffer.

---

## 2. Revised Roadmap

This is a concrete path to a working demo.

### Phase 1: The Scaffold & Binary Loader
**Goal:** Load a dummy binary file into a Rust vector and read bytes from it.
1.  Set up a Rust workspace with two crates: `vm` (Host) and `kernel` (Guest).
2.  Configure `kernel` to cross-compile to `riscv64imac-unknown-none-elf`.
3.  Implement the `Bus` struct in the VM:
    *   Map `0x8000_0000` to `DRAM`.
    *   Map `0x1000_0000` to `UART`.
4.  **Test:** The VM loads the Kernel binary and prints the first 4 bytes (the ELF header) to verify loading works.

### Phase 2: The CPU Core (Instruction Execution)
**Goal:** The CPU executes instructions from the Kernel.
1.  Implement the **Fetch** loop (Read instruction at PC).
2.  Implement **Decode** (Parse Opcode).
3.  Implement **Execute** for:
    *   `LUI`, `AUIPC` (Loading constants).
    *   `JAL`, `JALR` (Function calls).
    *   `ADDI`, `ADD`, `SUB` (Math).
    *   `LW`, `SW` (Stack memory access).
4.  **Test:** Write a Guest assembly program that adds 1+1. Verify the CPU register equals 2.

### Phase 3: UART & Input/Output
**Goal:** The VM can print "Hello" to your console.
1.  **VM Side:** Implement `store_byte` at address `0x1000_0000`. When written, print the char to standard stdout (or console.log).
2.  **Guest Side:** Write a Rust function `print_uart(s: &str)` that writes bytes to that address.
3.  **Test:** The Guest boots and prints "Booting..." to the console.

### Phase 4: The Logic (The Counter)
**Goal:** Implement the specific interaction logic.
1.  **VM Side:** Implement `read_byte` at `0x1000_0000`. It should return 0 if no input, or the char if user typed.
2.  **Guest Side:** Implement a polling loop:
    ```rust
    let mut count = 0;
    loop {
        if let Some(c) = uart.read() {
            if c == b'\n' {
                count += 1;
                println!("Hello, count {}", count);
            }
        }
    }
    ```

### Phase 5: Web Assembly Integration (Next.js)
**Goal:** Move from terminal to Browser.
1.  Compile `vm` to Wasm using `wasm-bindgen`.
2.  Expose a JS function `vm_step()` and `vm_input(char)`.
3.  **Next.js Frontend:**
    *   Create a Terminal UI (black box, green text).
    *   On KeyPress $\to$ Call `vm_input(key)`.
    *   Loop $\to$ Call `vm_step()`.
    *   On VM Output $\to$ Append to Terminal UI text.

---

## 3. Detailed File Structure

This structure separates the "Emulator" from the "Program" clearly.

```text
riscv-echo-project/
├── Cargo.toml              # Workspace root
├── vm/                     # THE HOST (Runs in Browser)
│   ├── Cargo.toml          # Dep: wasm-bindgen
│   └── src/
│       ├── lib.rs          # Wasm interface (start, step, add_input)
│       ├── cpu.rs          # Registers, PC, Step logic
│       ├── instructions.rs # Match opcodes -> functionality
│       ├── bus.rs          # Maps addresses to components
│       ├── dram.rs         # Vec<u8> memory
│       └── uart.rs         # Buffers for input/output
├── kernel/                 # THE GUEST (Runs inside VM)
│   ├── Cargo.toml          # Dep: riscv (crate), no_std
│   ├── .cargo/config.toml  # Linker script definition
│   ├── memory.x            # Defines RAM layout (start at 0x80000000)
│   └── src/
│       ├── main.rs         # The loop: read char -> print counter
│       ├── start.S         # Assembly entry point (setup stack)
│       └── uart.rs         # Low-level driver to write to 0x10000000
└── web/                    # THE UI (Next.js)
    ├── src/
    │   ├── app/page.tsx    # The Terminal UI
    │   └── hooks/useVM.ts  # Loads Wasm, manages loop
    └── public/
        └── kernel.bin      # The compiled kernel binary
```

---

## 4. Immediate Next Question

To generate the Phase 1 & 2 code, I need to know:

**Do you want to start with the VM (The Host) implementation first, or do you want to write the Guest Kernel (The logic) first to define what instructions we actually need to support?**

(Usually, building the VM first is better so you have a place to run your tests).