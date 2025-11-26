#!/bin/bash
cd vm
npx wasm-pack build --target web --out-dir ../web/src/pkg
cd ..

cp web/src/pkg/riscv_vm_bg.wasm web/public/riscv_vm_bg.wasm
