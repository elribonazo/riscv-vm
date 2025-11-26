FROM node:20-slim AS web-builder

ENV NODE_ENV=production

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


# Copy entire project
RUN git clone https://github.com/elribonazo/riscv-vm.git .
# Build kernel and VM (WASM)
RUN chmod +x build.sh && ./build.sh

WORKDIR /app/web

# Enable corepack for Yarn 4
RUN corepack enable

# Install dependencies and build
RUN yarn install --immutable
RUN yarn build


EXPOSE 3000

ENV PORT=3000
ENV HOSTNAME="0.0.0.0"

CMD ["yarn", "start"]

