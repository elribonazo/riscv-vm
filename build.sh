#!/bin/bash

cargo build -p kernel --target riscv64gc-unknown-none-elf --release
cargo build -p relay --release
npx wasm-pack build  --target web --out-dir ../web/src/pkg riscv-vm



cp target/riscv64gc-unknown-none-elf/release/kernel web/public/images/custom/kernel
cp web/src/pkg/riscv_vm_bg.wasm web/public/riscv_vm_bg.wasm
