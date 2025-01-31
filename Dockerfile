# Build stage
FROM rust:1.84-slim-bookworm as builder

WORKDIR /usr/src/app
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy only the files needed for dependency resolution
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src/

# Copy the actual source code
COPY . .

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies including DNS utilities
RUN apt-get update && \
    apt-get install -y \
    ca-certificates \
    openresolv \
    libnss3-tools \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/sthp /app/sthp

# Create a non-root user
RUN useradd -m -u 1000 -U app && \
    chown -R app:app /app
USER app

EXPOSE 8080

ENTRYPOINT ["/app/sthp"]
