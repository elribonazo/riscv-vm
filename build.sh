#!/bin/bash

set -e

cargo build -p kernel --target riscv64gc-unknown-none-elf --release
cargo build -p relay --release
cargo build -p riscv-vm --release

cargo build -p mkfs --release --target wasm32-unknown-unknown --no-default-features
cargo run -p mkfs -- --output target/riscv64gc-unknown-none-elf/release/fs.img --dir mkfs/root --size 2

cd riscv-vm && yarn build && cd ..
