#!/bin/bash
cargo build -p kernel --target riscv64gc-unknown-none-elf --release
cargo build -p relay --release
cargo build -p riscv-vm --release
cargo run -p mkfs -- --output target/riscv64gc-unknown-none-elf/release/disk.img --dir mkfs/root --size 64

cd riscv-vm && yarn build && cd ..
