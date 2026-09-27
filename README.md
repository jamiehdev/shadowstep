# shadowstep

A small caching reverse proxy written in Rust (actix-web 4, hyper 0.14, rustls 0.20). It serves local files from an asset directory, forwards every other request to one upstream origin, and keeps cacheable origin responses in memory.

## What it does

- `GET /assets/{path}` serves files from the asset directory. Paths that escape the directory, including through symlinks, return 404.
- Asset responses carry an `ETag` and `Cache-Control: public, max-age=86400`. A request whose `If-None-Match` equals the file's ETag gets `304 Not Modified`.
- Every other path is forwarded to the origin with its method, path, query, headers and body. See [Proxying](#proxying).
- Cacheable origin responses are stored and served from memory. See [Caching](#caching).
- `GET /health` returns `{"status":"ok","cache":{...}}` with `hits`, `misses`, `hit_ratio`, `items` and `bytes` for the cache.
- Responses are compressed according to the request's `Accept-Encoding`.
- HTTPS is served when both a certificate and a key are given. See [TLS](#tls).

## Proxying

The request path and query are appended to the path of `ORIGIN_URL`, so `--origin-url http://origin.internal/app` sends `/page?x=1` to `http://origin.internal/app/page?x=1`. A query or fragment on `ORIGIN_URL` is dropped.

The request body streams to the origin and the response body streams back to the client. `Host` is set to the origin's host, with its port unless that is 80 or 443.

Hop-by-hop headers are removed in both directions: `Connection`, `Keep-Alive`, `Proxy-Connection`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`, `Proxy-Authenticate`, `Proxy-Authorization`, and every header named in `Connection`.

shadowstep expects to face clients directly. It removes these client-sent headers and never reads them:

- `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Forwarded-Port`, `X-Forwarded-Server` and `X-Real-IP`
- `X-Forwarded-Prefix`, `X-Forwarded-Uri`, `X-Forwarded-Scheme`, `X-Host`, `X-Original-Host` and `Front-End-Https`, which some origin frameworks use to build links and redirects
- `X-Original-URL` and `X-Rewrite-URL`, which some origin frameworks use to override the request path

The origin then receives one of each of these headers:

- `X-Forwarded-For`: the IP address of the TCP peer
- `X-Forwarded-Proto`: `https` on the HTTPS listener, `http` on the plain one
- `X-Forwarded-Host`: the authority of an absolute-form or HTTP/2 request target, else the request's `Host`

There is no trusted-proxy setting. Behind another load balancer or proxy, `X-Forwarded-For` holds that proxy's address, and any forwarding headers that proxy adds are removed.

`--upstream-timeout-seconds` (default 30) limits how long shadowstep waits for the origin's response headers, including the time to send the request body. The limit does not cover the response body.

Error responses have short generic bodies:

- `502 Bad Gateway`: the origin could not be reached or the connection failed
- `504 Gateway Timeout`: no response headers within the upstream timeout
- `400 Bad Request`: reading the request body failed before any of it was sent to the origin
- `500 Internal Server Error`: the upstream URL or request could not be built

## Caching

Origin responses are cached in memory under the rules of RFC 9111 for a shared cache.

A response to `GET` is stored when all of these hold:

- the status is 200, 203, 204, 300, 301, 308, 404, 405, 410, 414 or 501
- it has an explicit freshness lifetime from `Cache-Control: s-maxage`, then `max-age`, then `Expires`
- `Cache-Control` has none of `no-store`, `private` or `no-cache`
- it has no `Set-Cookie` and no `Vary: *`
- if the request had `Authorization` or `Cookie`, the response has `Cache-Control: public`

The cache key is the scheme, host, path and query, plus the request's values for each header the response names in `Vary`. The host comes from the same place as `X-Forwarded-Host`.

For each request:

- A `HEAD` request is answered from a stored `GET` response.
- Request `Cache-Control: no-cache` skips the stored copy, goes to the origin and stores the new response.
- Request `Cache-Control: no-store` is neither served from the cache nor stored.
- Request `Cache-Control: max-age=N` skips stored copies older than `N` seconds.
- A request with `X-HTTP-Method-Override`, `X-HTTP-Method` or `X-Method-Override` is forwarded with that header, but is neither served from the cache nor stored, because the origin may treat it as another method.
- A request with an unsafe method, such as `POST`, `PUT` or `DELETE`, that gets a 2xx or 3xx response removes the stored response for its URL.

Responses served from the cache carry `Age`. Proxied and asset responses carry `X-Shadowstep-Cache: HIT` or `MISS`.

`--cache-ttl-seconds` (default 300) caps how long any entry is kept, whatever the origin's freshness lifetime. `--cache-size-mb` (default 100) bounds origin responses and assets together, measured in bytes. The largest single entry is 8 MiB or the cache size, whichever is smaller. Larger bodies stream to the client without being stored. Setting either option to 0 turns caching off.

Assets share the same cache. A stored asset is read from disk again when the file's size or modified time changes.

`/health` counts hits and misses for proxied requests and assets together. `items` and `bytes` describe the whole cache.

Known limits:

- There is no revalidation. shadowstep never sends conditional requests to the origin, so a stale entry is dropped and fetched again in full.
- There is no request coalescing. Concurrent misses for the same URL all go to the origin.
- The host is part of the key, so a client that sends many different `Host` values can create many entries. The byte bound on the cache still applies.
- Each process has its own cache. Replicas do not share entries or invalidations.

## Install

Each release tag `vX.Y.Z` publishes a multi-platform image for `linux/amd64` and `linux/arm64` to `ghcr.io/jamiehdev/shadowstep`, tagged `X.Y.Z`, `X.Y` and `latest`. A pre-release tag such as `v2.1.0-rc.1` publishes only `2.1.0-rc.1`.

```bash
docker pull ghcr.io/jamiehdev/shadowstep:2.0.0
```

The image carries SLSA provenance and an SBOM. To check that an image was built by this repository's release workflow:

```bash
gh attestation verify oci://ghcr.io/jamiehdev/shadowstep:2.0.0 --owner jamiehdev
```

Releases do not include prebuilt binaries. To run outside a container, build from source as shown in [Build](#build).

See [Docker](#docker) for running the image.

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

Logging uses `env_logger` at `info` by default, which logs one line per request. Set `RUST_LOG=debug` to also log forwarding and cache decisions.

## Configuration

Each option can be set with a flag or an environment variable. The flag wins if both are set.

| Flag | Environment variable | Default | Description |
|---|---|---|---|
| `--origin-url` | `ORIGIN_URL` | required | Upstream origin URL, for example `http://origin.internal:3000` |
| `--listen-addr` | `LISTEN_ADDR` | `0.0.0.0:8080` | Address for the plain HTTP listener |
| `--asset-path` | `ASSET_PATH` | `/app/assets` | Directory served under `/assets/` |
| `--cache-ttl-seconds` | `CACHE_TTL_SECONDS` | `300` | Longest time any cache entry is kept; 0 turns caching off |
| `--cache-size-mb` | `CACHE_SIZE_MB` | `100` | Cache size in MiB for origin responses and assets together; 0 turns caching off |
| `--tls-cert` | `TLS_CERT_PATH` | none | PEM certificate chain |
| `--tls-key` | `TLS_KEY_PATH` | none | PEM private key in PKCS#8 form |
| `--tls-listen-addr` | `TLS_LISTEN_ADDR` | `0.0.0.0:8443` | Address for the HTTPS listener, used only when both TLS paths are set |
| `--upstream-timeout-seconds` | `UPSTREAM_TIMEOUT_SECONDS` | `30` | Seconds to wait for the origin's response headers before answering 504 |

`cargo run -- --help` prints the same list.

## TLS

When both `--tls-cert` and `--tls-key` are set, shadowstep also listens for HTTPS on `--tls-listen-addr`. The HTTP listener on `--listen-addr` keeps running. If only one of the two paths is set, HTTPS is off.

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

The image exposes 8080 and 8443. If you change `LISTEN_ADDR` or `TLS_LISTEN_ADDR`, publish the matching ports.

The container runs as user `shadowstep` (uid 1000), so the mounted key file must be readable by that uid.

## Kubernetes

`k8s/` holds a Deployment and a LoadBalancer Service. Before applying them, set the image and `ORIGIN_URL` in `k8s/deployment.yaml` and create the TLS Secret the Deployment mounts at `/etc/tls`:

```bash
kubectl create secret tls shadowstep-tls --cert=certs/cert.pem --key=certs/key.pem
kubectl apply -f k8s/
```

The Service maps port 80 to 8080 and 443 to 8443. Readiness and liveness probes call `/health` on port 8080. To run without TLS, remove the `TLS_CERT_PATH`, `TLS_KEY_PATH` and `TLS_LISTEN_ADDR` variables, the `tls` volume and mount, and the `https` ports.

`CACHE_SIZE_MB` is set to 100 against a 256Mi memory limit. Change the two together.

The Deployment runs two replicas, and each has its own cache. `X-Forwarded-For` holds whatever source address reaches the pod. With the Service's default `externalTrafficPolicy: Cluster`, that is often a node address rather than the client's.

## Tests

```bash
cargo test
```

Unit tests in `src/assets.rs` cover asset path traversal, `src/cache.rs` covers `Cache-Control` parsing, and `src/forwarded.rs` covers reading the client's address, scheme and host from the connection. The integration tests in `tests/integration/` build one test crate and run the app against a [wiremock](https://crates.io/crates/wiremock) origin or a local TCP origin:

- `smoke.rs`: `/health`, proxying, upstream paths and the 502 path
- `proxy.rs`: request and response bodies, the upstream timeout, hop-by-hop and URL override headers
- `tls.rs`: the HTTPS listener and `--tls-listen-addr`
- `forwarded.rs`: removal and replacement of forwarding headers
- `cache.rs`: origin response caching

CI also runs:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Licence

[MIT](LICENSE)
