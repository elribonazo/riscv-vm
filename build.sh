#!/bin/bash

cd kernel
cargo build --release
cd .. 

cd vm
npx wasm-pack build --target web --out-dir ../web/src/pkg
cd ..

cp target/riscv64imac-unknown-none-elf/release/kernel web/public/kernel.bin
cp web/src/pkg/vm_bg.wasm web/public/vm_bg.wasm