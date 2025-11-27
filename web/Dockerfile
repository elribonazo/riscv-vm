FROM node:20-slim AS web-builder

ENV NODE_ENV=production
ARG NEXT_PUBLIC_RELAY_URL=https://localhost:4433
ARG NEXT_PUBLIC_RELAY_CERT_HASH=
ENV NEXT_PUBLIC_RELAY_URL=${NEXT_PUBLIC_RELAY_URL}
ENV NEXT_PUBLIC_RELAY_CERT_HASH=${NEXT_PUBLIC_RELAY_CERT_HASH}

# Install dependencies
RUN apt-get update && apt-get install -y \
    curl \
    pkg-config \
    libssl-dev \
    build-essential \
    git \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install wasm-pack
RUN curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

# Add RISC-V target
RUN rustup target add riscv64gc-unknown-none-elf

WORKDIR /app


COPY ../ .
# Build kernel and VM (WASM)
RUN chmod +x build.sh
RUN ./build.sh

WORKDIR /app/web

# Enable corepack for Yarn 4
RUN corepack enable

# Install dependencies and build
RUN yarn install --immutable


EXPOSE 3000
ENV PORT=3000
ENV HOSTNAME="0.0.0.0"

CMD ["yarn", "start"]
