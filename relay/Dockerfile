# Multi-stage build for smaller final image
FROM rust:1.82-slim-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/relay

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock* ./

# Create a dummy main.rs to build dependencies
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs

# Build dependencies only (this layer will be cached)
RUN cargo build --release && rm -rf src

# Copy the actual source code
COPY src ./src

# Build the release binary (touch to ensure rebuild)
RUN touch src/main.rs && cargo build --release

# Runtime stage - minimal image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /usr/src/relay/target/release/relay /app/relay

# Expose ports:
# 4001/udp - QUIC transport
# 4002     - TCP transport
EXPOSE 4001/udp
EXPOSE 4002

# Default environment
ENV RUST_LOG=info

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD pgrep -x relay || exit 1

# Run the relay server
# --bind 0.0.0.0 ensures it listens on all interfaces in container
CMD ["/app/relay", "--bind", "0.0.0.0", "--port", "4001", "--tcp-port", "4002"]
