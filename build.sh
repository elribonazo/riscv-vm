#!/bin/bash
cargo build -p kernel --target riscv64gc-unknown-none-elf --release
cargo build -p relay --release
cd riscv-vm && yarn build && cd ..

cp target/riscv64gc-unknown-none-elf/release/kernel web/public/images/custom/kernel
