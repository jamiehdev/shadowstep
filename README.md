# shadowstep

A small reverse proxy written in Rust (actix-web 4, hyper 0.14, rustls 0.20). It serves local files from an asset directory and forwards every other request to one upstream origin.

## What it does

- `GET /assets/{path}` serves files from the asset directory. Paths that escape the directory, including through symlinks, return 404.
- Asset responses carry an `ETag`, `Cache-Control: public, max-age=86400` and `X-Shadowstep-Cache: HIT` or `MISS`. A request whose `If-None-Match` matches the ETag of a cached file gets `304 Not Modified`.
- Every other path is forwarded to the origin with its method, path, query and headers. Hop-by-hop headers are dropped, `Host` is set to the origin's host, and `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` are added. An unreachable origin returns `502 Bad Gateway`.
- `GET /health` returns `{"status":"ok","cache":{...}}` with hit, miss and item counts for the asset cache.
- Responses are compressed according to the request's `Accept-Encoding`.
- HTTPS is served when both a certificate and a key are given (see [TLS](#tls)).

## Not implemented

- Origin responses are not cached. Every proxied request goes to the origin.
- The asset cache is an in-memory map with no size limit, no TTL and no eviction. A file is read once and served from memory until the process restarts, so edits to a file on disk are not picked up.
- `--cache-ttl-seconds` and `--cache-size-mb` are parsed but not used.

## Build

Install Rust with [rustup](https://rustup.rs/), then:

```bash
git clone git@github.com:jamiehdev/shadowstep.git
cd shadowstep
cargo build --release --locked
```

## Run

```bash
./target/release/shadowstep \
  --origin-url http://localhost:3000 \
  --listen-addr 127.0.0.1:8080 \
  --asset-path ./assets
```

The same with environment variables:

```bash
ORIGIN_URL=http://localhost:3000 LISTEN_ADDR=127.0.0.1:8080 ASSET_PATH=./assets ./target/release/shadowstep
```

The default asset path is `/app/assets`, the path used in the Docker image. When running outside a container, pass `--asset-path`. The directory is created at start-up if it does not exist.

Logging uses `env_logger` at `info` by default. Set `RUST_LOG=debug` to log each proxied request.

## Configuration

Each option can be set with a flag or an environment variable. The flag wins if both are set.

| Flag | Environment variable | Default | Description |
|---|---|---|---|
| `--origin-url` | `ORIGIN_URL` | required | Upstream origin URL, for example `http://origin.internal:3000` |
| `--listen-addr` | `LISTEN_ADDR` | `0.0.0.0:8080` | Address for the plain HTTP listener |
| `--asset-path` | `ASSET_PATH` | `/app/assets` | Directory served under `/assets/` |
| `--tls-cert` | `TLS_CERT_PATH` | none | PEM certificate chain |
| `--tls-key` | `TLS_KEY_PATH` | none | PEM private key in PKCS#8 form |
| `--cache-ttl-seconds` | `CACHE_TTL_SECONDS` | `300` | Not used yet |
| `--cache-size-mb` | `CACHE_SIZE_MB` | `100` | Not used yet |

`cargo run -- --help` prints the same list.

## TLS

When both `--tls-cert` and `--tls-key` are set, shadowstep also listens for HTTPS on `0.0.0.0:8443`. That address is fixed and cannot be configured. The HTTP listener on `--listen-addr` keeps running. If only one of the two paths is set, HTTPS is off.

The key must be PKCS#8 (`-----BEGIN PRIVATE KEY-----`). Only the first key in the file is used. To convert a PKCS#1 or SEC1 key:

```bash
openssl pkcs8 -topk8 -nocrypt -in key.pem -out key.pkcs8.pem
```

A self-signed certificate for local testing (OpenSSL 3 writes PKCS#8 by default):

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=localhost \
  -keyout certs/key.pem -out certs/cert.pem
```

`certs/` is listed in `.gitignore` and `.dockerignore`, so keys there are never committed or sent to a Docker build.

## Docker

The image contains the binary and the contents of `assets/`. It does not contain certificates.

```bash
docker build -t shadowstep:local .
```

HTTP only:

```bash
docker run --rm -p 8080:8080 shadowstep:local --origin-url http://example.com
```

With HTTPS, mount the certificate directory read-only and pass the paths inside the container:

```bash
docker run --rm \
  -p 8080:8080 -p 8443:8443 \
  -v "$(pwd)/certs:/etc/shadowstep/certs:ro" \
  -e ORIGIN_URL=http://example.com \
  -e TLS_CERT_PATH=/etc/shadowstep/certs/cert.pem \
  -e TLS_KEY_PATH=/etc/shadowstep/certs/key.pem \
  shadowstep:local
```

The container runs as user `shadowstep` (uid 1000), so the mounted key file must be readable by that uid.

## Kubernetes

`k8s/` holds a Deployment and a LoadBalancer Service. Before applying them, set the image and `ORIGIN_URL` in `k8s/deployment.yaml` and create the TLS Secret the Deployment mounts at `/etc/tls`:

```bash
kubectl create secret tls shadowstep-tls --cert=certs/cert.pem --key=certs/key.pem
kubectl apply -f k8s/
```

The Service maps port 80 to 8080 and 443 to 8443. Readiness and liveness probes call `/health` on port 8080. To run without TLS, remove the `TLS_CERT_PATH` and `TLS_KEY_PATH` variables, the `tls` volume and mount, and the `https` ports.

## Tests

```bash
cargo test
```

The unit tests in `src/assets.rs` cover path traversal in the asset route. `tests/smoke.rs` starts the app against a [wiremock](https://crates.io/crates/wiremock) origin and checks `/health`, proxying and the 502 path. CI also runs:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Licence

[MIT](https://opensource.org/licenses/MIT)
