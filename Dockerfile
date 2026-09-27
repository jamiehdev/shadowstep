# rust 1.85 is the minimum: the dev-dependencies wiremock and hyper-util use
# edition 2024, and cargo parses every manifest in Cargo.lock
FROM rust:1.98-bookworm AS builder

WORKDIR /usr/src/shadowstep

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# uid and gid 1000 match runAsUser and fsGroup in k8s/deployment.yaml
RUN groupadd -r -g 1000 shadowstep && \
    useradd -r -u 1000 -g shadowstep -s /bin/false -d /app shadowstep

# certificates are not copied into the image: mount them at run time and pass
# --tls-cert and --tls-key (or TLS_CERT_PATH and TLS_KEY_PATH)
COPY --chown=shadowstep:shadowstep ./assets /app/assets

WORKDIR /app

COPY --from=builder /usr/src/shadowstep/target/release/shadowstep /usr/local/bin/shadowstep

EXPOSE 8080
# the HTTPS listener binds TLS_LISTEN_ADDR (default 0.0.0.0:8443) when both
# TLS paths are set
EXPOSE 8443

USER 1000:1000

ENTRYPOINT ["/usr/local/bin/shadowstep"]
