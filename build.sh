#!/bin/bash

cd kernel
cargo build --target riscv64gc-unknown-none-elf --release


cd ../vm
npx wasm-pack build --target web --out-dir ../web/src/pkg
cd ..

cp target/riscv64gc-unknown-none-elf/release/kernel web/public/custom_kernel
cp web/src/pkg/riscv_vm_bg.wasm web/public/riscv_vm_bg.wasm
