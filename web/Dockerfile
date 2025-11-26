FROM rust:1.83-slim-bookworm AS builder

# Install dependencies
RUN apt-get update && apt-get install -y \
    curl \
    pkg-config \
    libssl-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Install wasm-pack
RUN curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

# Add RISC-V target
RUN rustup target add riscv64gc-unknown-none-elf

WORKDIR /app

# Copy entire project
COPY . .

# Build kernel and VM (WASM)
RUN chmod +x build.sh && ./build.sh

# --- Node.js Build Stage ---
FROM node:20-slim AS web-builder

WORKDIR /app

# Copy web directory
COPY web/package.json web/yarn.lock ./
COPY web .

# Copy WASM artifacts from builder stage
COPY --from=builder /app/web/src/pkg ./src/pkg
COPY --from=builder /app/web/public/images/custom/kernel ./public/images/custom/kernel
COPY --from=builder /app/web/public/riscv_vm_bg.wasm ./public/riscv_vm_bg.wasm

# Install dependencies and build
RUN yarn install --immutable
RUN yarn build

# --- Final Stage ---
FROM node:20-slim AS runner

WORKDIR /app

ENV NODE_ENV=production

# Copy standalone build
COPY --from=web-builder /app/.next/standalone ./
COPY --from=web-builder /app/.next/static ./.next/static
COPY --from=web-builder /app/public ./public

EXPOSE 3000

ENV PORT=3000
ENV HOSTNAME="0.0.0.0"

CMD ["node", "server.js"]

