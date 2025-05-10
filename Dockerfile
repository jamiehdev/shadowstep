# Stage 1: Build
# Use the official Rust image as a builder
FROM rust:1.82-bullseye AS builder

# Set the working directory
WORKDIR /usr/src/shadowstep

# Clean cargo caches to ensure a completely fresh state
RUN rm -rf /usr/local/cargo/registry /usr/local/cargo/git ~/.cargo/registry ~/.cargo/git

# Install build dependencies (if any beyond Rust toolchain, e.g., for linking)
# RUN apt-get update && apt-get install -y some-lib-dev && rm -rf /var/lib/apt/lists/*

# Copy Cargo.toml and Cargo.lock
COPY Cargo.toml Cargo.lock ./

# Copy the source code
COPY src ./src

# Build the application (this will also build dependencies)
# We remove --locked here initially to ensure Cargo.lock can be regenerated if needed.
# If this succeeds, for subsequent production builds, --locked should be used.
RUN cargo update && cargo build --release

# Stage 2: Runtime
# Use a slim base image
FROM debian:bullseye-slim AS runtime

# Install runtime dependencies (e.g., CA certificates)
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Create a non-root user and group
RUN groupadd -r shadowstep && \
    useradd -r -g shadowstep -s /bin/false -d /app shadowstep

# Create directories and set permissions
RUN mkdir -p /app/assets && \
    chown -R shadowstep:shadowstep /app

COPY ./certs /app/certs
RUN chown -R shadowstep:shadowstep /app/certs

# Copy static assets for serving
COPY ./assets /app/assets
RUN chown -R shadowstep:shadowstep /app/assets

WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/shadowstep/target/release/shadowstep /usr/local/bin/shadowstep
RUN chmod +x /usr/local/bin/shadowstep && \
    chown shadowstep:shadowstep /usr/local/bin/shadowstep

# Expose the default ports (update if your defaults change)
EXPOSE 8080
# For TLS connections
EXPOSE 8443

# Switch to non-root user
USER shadowstep

# Set the entrypoint
ENTRYPOINT ["/usr/local/bin/shadowstep"]

# Default command (can be overridden)
# CMD ["--origin", "http://default-origin.example.com"] 