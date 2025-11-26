FROM rust:latest

WORKDIR /usr/src/relay

COPY . .

# Build the release version
RUN cargo build --release

EXPOSE 8765

# Run the release binary
CMD ["cargo", "run", "--release"]

