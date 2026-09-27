# shadowstep

A small caching reverse proxy written in Rust (actix-web 4, hyper 1, rustls 0.23). It serves local files from an asset directory, forwards every other request to one upstream origin, and keeps cacheable origin responses in memory.

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

`--upstream-timeout-seconds` (default 30) limits how long shadowstep waits for the origin's response headers, including the time to send the request body. It also limits how long the origin may send nothing while the proxy waits for the next part of the response body. The body may take longer in total, as long as data keeps arriving. When the origin stays silent for longer, shadowstep logs a warning and closes the client's connection before the body is complete: a response with `Content-Length` ends short, and a chunked response gets no last chunk. The proxy does not store that response.

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

Responses served from the cache carry `Age`. Proxied and asset responses carry `X-Shadowstep-Cache`:

- `HIT`: a fresh stored response.
- `MISS`: the origin's response.
- `REVALIDATED`: the origin answered a conditional request with `304 Not Modified`, and the client got the stored body with the 304's header fields.
- `STALE`: a stale stored response, served under `stale-while-revalidate` or `stale-if-error`.
- `COALESCED`: a response that a concurrent request for the same key stored while this request waited for it. See [Request coalescing](#request-coalescing).

`--cache-ttl-seconds` (default 300) caps the freshness lifetime of any entry, whatever the origin sent. `--cache-size-mb` (default 100) bounds origin responses and assets together, measured in bytes. The largest single entry is 8 MiB or the cache size, whichever is smaller. Larger bodies stream to the client without being stored. Setting either option to 0 turns caching off.

### Revalidation and stale responses

A stored response that is stale stays in the cache for a grace period so that it can be revalidated or served stale. The grace period is `--cache-ttl-seconds` if the response has an `ETag` or `Last-Modified`, and otherwise the larger of its `stale-while-revalidate` and `stale-if-error` windows, capped at `--cache-ttl-seconds`. Those windows do not count for a response that must be revalidated, as described below. A response with neither a validator nor a stale window is dropped when it goes stale. Entries in their grace period count towards `--cache-size-mb`.

A `GET` that finds a stale response sends the origin `If-None-Match` from the stored `ETag` and `If-Modified-Since` from the stored `Last-Modified`. A request whose `Cache-Control: max-age` is shorter than a fresh stored response's age revalidates that response the same way.

- On `304 Not Modified`, the 304's header fields replace the stored fields of the same name, the freshness lifetime starts again from the updated fields, and the client gets `200` with the stored body.
- On any other response, the proxy forwards it and stores it under the usual rules.
- If the client sent its own `If-None-Match`, `If-Modified-Since`, `If-Match`, `If-Unmodified-Since` or `If-Range`, the proxy forwards those unchanged and adds none of its own. The origin's answer, including a `304`, goes to the client as `MISS`, and a 304 leaves the stored response as it was.

`Cache-Control: stale-while-revalidate=N` lets the proxy serve a response for `N` seconds after it goes stale while it revalidates in the background. At most one background revalidation runs for each stored response at a time. It uses the same header fields, forwarding headers and `--upstream-timeout-seconds` as any request to the origin, with the stored validators in place of the client's conditional headers. If the origin sends nothing for that long during the body of its answer, the proxy stores nothing, and a later request can start another background revalidation.

`Cache-Control: stale-if-error=N` lets the proxy serve a response for `N` seconds after it goes stale when the origin answers 500, 502, 503 or 504, refuses the connection or times out.

A response with `must-revalidate`, `proxy-revalidate` or `s-maxage` is never served stale, whatever its stale windows. If the origin cannot be reached to revalidate it, the client gets `504 Gateway Timeout`. A request with `Cache-Control: max-age` never gets a stale response. `HEAD` requests are answered only from fresh responses.

### Request coalescing

When several requests miss on the same key at once, the first one, the leader, goes to the origin and the others wait for it. The key is the scheme, host, path and query, plus the request's values for the `Vary` field names of the latest stored response for that URL, if the cache has one. Requests for different known variants therefore do not wait for each other. Requests that find a stale response they must revalidate coalesce the same way, so the origin gets one conditional request.

- Only `GET` and `HEAD` requests that may be served from the cache wait. A request with `Authorization`, `Cookie`, a conditional header, request `Cache-Control: no-cache` or `no-store`, or a method-override header goes to the origin as before.
- Only a `GET` leads. A `HEAD` request with no `GET` in flight goes to the origin.
- The leader's response streams to its client as usual. The waiting requests look up the cache once the response is stored, or once the proxy knows it will not be stored: it is not storable, its body passes the entry size limit or fails, the leader's client disconnects, or the origin fails, times out or stops sending the body.
- A waiting request gets the stored response with `X-Shadowstep-Cache: COALESCED`. If the cache has nothing it may use, for example because the response was `private` or had `Vary` values that differ from the waiting request's, the waiting request goes to the origin on its own. One client's uncacheable response never goes to another client.
- A request waits for at most `--upstream-timeout-seconds`, then goes to the origin on its own.

Assets share the same cache. A stored asset is read from disk again when the file's size or modified time changes.

`/health` counts responses for proxied requests and assets together. Each response counts once:

- `hits`: fresh stored responses.
- `misses`: responses from the origin, including errors.
- `revalidations`: 304s that freshened a stored response, in the foreground or the background.
- `stale`: stale responses served under `stale-while-revalidate` or `stale-if-error`.
- `coalesced`: responses stored by a concurrent request that this request waited for.

`background_refreshes` counts background revalidations started. `hit_ratio` is the share of responses whose body came from the cache: hits, revalidations, stale and coalesced responses. `items` and `bytes` describe the whole cache.

Known limits:

- A response with `Cache-Control: no-cache` is not stored, although RFC 9111 allows storing it and revalidating it on every use.
- A stored response is never used to answer a client's conditional request with a 304. A fresh hit always gets the full response.
- Requests with `Cookie` do not coalesce. If browsers send a cookie with every request to the site, only cookieless clients coalesce.
- The host is part of the key, so a client that sends many different `Host` values can create many entries. The byte bound on the cache still applies.
- Each process has its own cache. Replicas do not share entries or invalidations.

## Install

Each release tag `vX.Y.Z` publishes a multi-platform image for `linux/amd64` and `linux/arm64` to `ghcr.io/jamiehdev/shadowstep`, tagged `X.Y.Z`, `X.Y` and `latest`. A pre-release tag such as `v2.1.0-rc.1` publishes only `2.1.0-rc.1`.

```bash
docker pull ghcr.io/jamiehdev/shadowstep:2.1.0
```

The image carries SLSA provenance and an SBOM. To check that an image was built by this repository's release workflow:

```bash
gh attestation verify oci://ghcr.io/jamiehdev/shadowstep:2.1.0 --owner jamiehdev
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
| `--cache-ttl-seconds` | `CACHE_TTL_SECONDS` | `300` | Longest freshness lifetime of any cache entry, and longest time a stale one is kept; 0 turns caching off |
| `--cache-size-mb` | `CACHE_SIZE_MB` | `100` | Cache size in MiB for origin responses and assets together; 0 turns caching off |
| `--tls-cert` | `TLS_CERT_PATH` | none | PEM certificate chain |
| `--tls-key` | `TLS_KEY_PATH` | none | PEM private key in PKCS#8 form |
| `--tls-listen-addr` | `TLS_LISTEN_ADDR` | `0.0.0.0:8443` | Address for the HTTPS listener, used only when both TLS paths are set |
| `--upstream-timeout-seconds` | `UPSTREAM_TIMEOUT_SECONDS` | `30` | Seconds to wait for the origin's response headers before answering 504, and the longest the origin may send nothing during a response body |

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

The container runs as user `shadowstep` (uid 1000), so the mounted key file must be readable by that uid. `openssl req` writes the key with mode 0600, owned by you. On a Linux host where your uid is not 1000, the container exits with `Permission denied`. Add `--user "$(id -u)"` to `docker run` to read the key as your own uid. Docker Desktop on macOS shows bind-mounted files as owned by the container's user, so the 0600 key works there without changes.

## Kubernetes

`k8s/` holds a Deployment and a LoadBalancer Service. The Deployment runs `ghcr.io/jamiehdev/shadowstep:2.1.0`. Before applying them, set `ORIGIN_URL` in `k8s/deployment.yaml` and create the TLS Secret the Deployment mounts at `/etc/tls`:

```bash
kubectl create secret tls shadowstep-tls --cert=certs/cert.pem --key=certs/key.pem
kubectl apply -f k8s/
```

To run a local build in a [kind](https://kind.sigs.k8s.io/) cluster instead, load the image into the cluster and point the Deployment at it after applying:

```bash
kind load docker-image shadowstep:local
kubectl set image deployment/shadowstep shadowstep=shadowstep:local
```

A tag other than `latest` gets the default `imagePullPolicy: IfNotPresent`, so the nodes use the loaded image.

The Service maps port 80 to 8080 and 443 to 8443. Readiness and liveness probes call `/health` on port 8080. To run without TLS, remove the `TLS_CERT_PATH`, `TLS_KEY_PATH` and `TLS_LISTEN_ADDR` variables, the `tls` volume and mount, and the `https` ports.

`CACHE_SIZE_MB` is set to 100 against a 256Mi memory limit. Change the two together.

The Deployment runs two replicas, and each has its own cache. `X-Forwarded-For` holds whatever source address reaches the pod. With the Service's default `externalTrafficPolicy: Cluster`, that is often a node address rather than the client's.

## Tests

```bash
cargo test
```

Unit tests in `src/assets.rs` cover asset path traversal, `src/cache.rs` covers `Cache-Control` parsing and header updates from a 304, and `src/forwarded.rs` covers reading the client's address, scheme and host from the connection. The integration tests in `tests/integration/` build one test crate and run the app against a [wiremock](https://crates.io/crates/wiremock) origin or a local TCP origin:

- `smoke.rs`: `/health`, proxying, upstream paths and the 502 path
- `proxy.rs`: request and response bodies, the upstream timeout, hop-by-hop and URL override headers
- `tls.rs`: the HTTPS listener and `--tls-listen-addr`
- `forwarded.rs`: removal and replacement of forwarding headers
- `cache.rs`: origin response caching
- `revalidation.rs`: conditional requests, `stale-while-revalidate`, `stale-if-error` and `must-revalidate`

CI also runs:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

A second CI job builds the Docker image without pushing it, then checks that the container runs as uid 1000 and answers `/health`.

## Licence

[MIT](LICENSE)
