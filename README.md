# shadowstep🥷

a minimal and fairly quick edge CDN written in Rust.

## features

### implemented
* HTTP/1.1 reverse proxy
* in-memory caching with TTL and LRU eviction ([ttl hell](https://calpaterson.com/ttl-hell.html))
* TLS termination (HTTPS)

### planned
* content compression (gzip + maybe Brotli adventure)
* metrics endpoint (Prometheus)
* cache purge API (invalidation fun🫣)

## getting started

### prerequisites

*   rust: install from [rustup.rs](https://rustup.rs/)
*   git

### clone & build

```bash
git clone git@github.com:jamiehdev/shadowstep.git
cd shadowstep
cargo build --release
```

### running

shadowstep can be configured via command-line arguments or environment variables.

```bash
# example: run shadowstep, proxying to shadowstep.example.com, listening on port 8080
./target/release/shadowstep --origin http://shadowstep.example.com --listen 0.0.0.0:8080
```

or using environment variables:

```bash
ORIGIN_URL="http://shadowstep.example.com" LISTEN_ADDR="0.0.0.0:8080" ./target/release/shadowstep
```

## deployment

#### docker (arch linux base)

a `Dockerfile` is provided for building an arch linux-based image.

```bash
docker build -t shadowstep:local .
docker run -e ORIGIN_URL=http://shadowstep.example.com -p 8080:8080 shadowstep:local
```

#### other operating systems (Windows, macOS)

1.  **install rust**: follow instructions at [rustup.rs](https://rustup.rs/).
2.  **build from source**:
    ```bash
    git clone git@github.com:jamiehdev/shadowstep.git
    cd shadowstep
    cargo build --release
    ```
3.  **run**:
    *   **Windows (powershell)**:
        ```powershell
        $env:ORIGIN_URL="http://shadowstep.example.com"; $env:LISTEN_ADDR="0.0.0.0:8080"; .\target\release\shadowstep.exe
        ```
    *   **macOS/Linux (bash/zsh)**:
        ```bash
        ORIGIN_URL="http://shadowstep.example.com" LISTEN_ADDR="0.0.0.0:8080" ./target/release/shadowstep
        ```

#### kubernetes

basic kubernetes manifests are provided in the `k8s/` directory for local testing with tools like Minikube or Kind.

```bash
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml
```

## project structure

shadowstep/
├── assets/          # non-code assets (images, configs)
│   └── images/      # image assets
├── src/             # source code
├── k8s/             # kubernetes configs
├── Cargo.toml       # rust package config
└── README.md        # project documentation

## configuration

| CLI argument    | environment variable | default         | description                        |
|-----------------|----------------------|-----------------|------------------------------------|
| `--origin`      | `ORIGIN_URL`         | (required)      | upstream origin server URL         |
| `--listen`      | `LISTEN_ADDR`        | `0.0.0.0:8080`  | address and port to listen on      |
| `--cache-ttl`   | `CACHE_TTL_SECONDS`  | `300`           | cache time-to-live in seconds      |
| `--cache-size`  | `CACHE_SIZE_MB`      | `100`           | max cache size in megabytes        |
| `--tls-cert`    | `TLS_CERT_PATH`      | (none)          | path to TLS certificate (pem)      |
| `--tls-key`     | `TLS_KEY_PATH`       | (none)          | path to TLS private key (pem)      |