#!/bin/bash
cargo build -p kernel --target riscv64gc-unknown-none-elf --release
cargo build -p relay --release
cargo build -p riscv-vm --release
cd riscv-vm && yarn build && cd ..