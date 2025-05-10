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

#### HTTPS example (origin over HTTP is normal)
```bash
# run with TLS termination enabled (origin can be HTTP)
./target/release/shadowstep \
  --origin http://shadowstep.example.com \
  --listen 0.0.0.0:8080 \
  --tls-cert ./certs/cert.pem \
  --tls-key ./certs/key.pem
```

or using environment variables:

```bash
ORIGIN_URL="http://shadowstep.example.com" LISTEN_ADDR="0.0.0.0:8080" ./target/release/shadowstep
```

## deployment

### Docker

a `Dockerfile` is provided for building a Docker image. make sure you have your `certs/` directory (with cert.pem and key.pem) in your project root before building.

```bash
docker build -t shadowstep:local .
```

```bash
docker run -d \
  --name shadowstep_cdn \
  -e RUST_LOG=info \
  -p 8080:8080 \
  -p 8443:8443 \
  -v $(pwd)/assets:/app/assets \
  shadowstep:local \
  --origin-url http://example.com \
  --tls-cert /app/certs/cert.pem \
  --tls-key /app/certs/key.pem
```

The options explained:
- `-d`: Run container in detached mode (background)
- `--name shadowstep_cdn`: Name the container for easy reference
- `-e RUST_LOG=info`: Set logging level (use `debug` for more verbose output)
- `-p 8080:8080 -p 8443:8443`: Map container ports to host ports
- `-v $(pwd)/assets:/app/assets`: Mount local assets directory to container
- `--origin-url`: Set the upstream origin server (use a real server or example.com for testing)
- `--tls-cert` and `--tls-key`: Paths to TLS certificate and key files inside the container

#### other operating systems (Windows, macOS)

1.  **install rust**: follow instructions at [rustup.rs](https://rustup.rs/).
2.  **build from source**:
    ```bash
    git clone git@github.com:jamiehdev/shadowstep.git
    cd shadowstep
    cargo build --release
    ```
3.  **run**:
    *   **Windows (PowerShell)**:
        ```powershell
        $env:ORIGIN_URL="http://shadowstep.example.com"; $env:LISTEN_ADDR="0.0.0.0:8080"; .\target\release\shadowstep.exe
        ```
    *   **macOS/Linux (bash/zsh)**:
        ```bash
        ORIGIN_URL="http://shadowstep.example.com" LISTEN_ADDR="0.0.0.0:8080" ./target/release/shadowstep
        ```

#### kubernetes

basic Kubernetes manifests are provided in the `k8s/` directory for local testing with tools like Minikube or Kind.

```bash
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml
```

## project structure

```
shadowstep/
├── assets/          # non-code assets (images, configs)
│   └── images/      # image assets
├── src/             # source code
├── k8s/             # Kubernetes configs
├── Cargo.toml       # rust package config
└── README.md        # project documentation
```

## configuration

| CLI argument    | environment variable | default         | description                        |
|-----------------|----------------------|-----------------|------------------------------------|
| `--origin`      | `ORIGIN_URL`         | (required)      | upstream origin server URL         |
| `--listen`      | `LISTEN_ADDR`        | `0.0.0.0:8080`  | address and port to listen on      |
| `--cache-ttl`   | `CACHE_TTL_SECONDS`  | `300`           | cache time-to-live in seconds      |
| `--cache-size`  | `CACHE_SIZE_MB`      | `100`           | max cache size in megabytes        |
| `--tls-cert`    | `TLS_CERT_PATH`      | (none)          | path to TLS certificate (pem)      |
| `--tls-key`     | `TLS_KEY_PATH`       | (none)          | path to TLS private key (pem)      |

## testing

below are the tests run to verify both HTTP and HTTPS endpoints:

### HTTP test - first request (cache miss)

```bash
# HTTP test - first request (cache miss)
curl -v http://localhost:8080/assets/test.txt
```

![HTTP test showing cache miss](https://i.imgur.com/GPTlpOS.png)

in this first request, you can see the `x-shadowstep-cache: MISS` header in the response, indicating the content was fetched from the origin.

### HTTP test - second request (cache hit)

```bash
# HTTP test - second request (cache hit)
curl -v http://localhost:8080/assets/test.txt
```

![HTTP test showing cache hit](https://i.imgur.com/cUmSQk5.png)

the second request shows `x-shadowstep-cache: HIT` in the response headers, confirming the file is now being served from cache.

### HTTPS test

```bash
# HTTPS test (insecure for self-signed cert)
curl -kv https://localhost:8443/assets/test.txt
```

for HTTPS tests, the output is similar but shows HTTP/2 protocol being used:

```
* SSL connection using TLSv1.3 / TLS_AES_256_GCM_SHA384
* ALPN: server accepted h2
* Connected to localhost (::1) port 8443
* using HTTP/2
< HTTP/2 200
< content-length: 12
< cache-control: public, max-age=86400
< etag: "9cfedc1214908e0b6a357b17e96244b0"
< x-shadowstep-cache: HIT
< content-type: text/plain
< date: Sat, 10 May 2025 18:53:37 GMT
<
hello HTTPS
```

### health endpoint test

```bash
# check cache statistics
curl http://localhost:8080/health
```

```
{"cache":{"hit_ratio":0.75,"hits":3,"items":1,"misses":1},"status":"ok"}
```

the health endpoint displays cache statistics, showing the ratio of hits to total requests, confirming the cache is working as expected.

## license

[MIT](https://opensource.org/licenses/MIT).