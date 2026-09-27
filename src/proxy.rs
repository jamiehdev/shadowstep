use actix_web::body::SizedStream;
use actix_web::{web, HttpRequest, HttpResponse};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use hyper::body::Body;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse, Uri};
use log::{debug, error, warn};
use std::convert::TryFrom;
use std::pin::Pin;
use std::task::{Context, Poll};
use url::{Position, Url};

use crate::cache::{self, Fields, PrimaryKey, RequestPolicy, StoredResponse};
use crate::forwarded::{ClientInfo, CLIENT_FORWARDING_HEADERS, URL_OVERRIDE_HEADERS};
use crate::AppState;

const CACHE_STATUS: &str = "x-shadowstep-cache";

pub async fn forward_to_upstream(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
) -> HttpResponse {
    let client = ClientInfo::from_request(&req);

    debug!(
        "Incoming proxy request: {:?} {} from {:?}",
        req.method(),
        req.uri(),
        client.ip
    );

    let path_and_query = req.uri().path_and_query().map_or("", |pq| pq.as_str());
    // the key takes scheme and host from `ClientInfo`, the source of the
    // X-Forwarded-Proto and X-Forwarded-Host that the origin receives
    let cache_key = PrimaryKey::new(client.scheme, &client.host, path_and_query);
    let request_policy = RequestPolicy::new(req.method(), req.headers());
    if let Some(hit) = serve_from_cache(&req, &state, &cache_key, &request_policy) {
        return hit;
    }
    state.cache_stats.miss();

    let Some((hyper_req_builder, target_uri)) =
        upstream_request(&req, &state, &client, path_and_query)
    else {
        return HttpResponse::InternalServerError().body("internal server error");
    };

    let body = match request_body(payload).await {
        Ok(body) => body,
        Err(e) => {
            warn!("Failed to read request body: {}", e);
            return HttpResponse::BadRequest().body("bad request");
        }
    };

    let hyper_req = match hyper_req_builder.body(body) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to build hyper request: {}", e);
            return HttpResponse::InternalServerError().body("internal server error");
        }
    };

    let upstream =
        tokio::time::timeout(state.upstream_timeout, state.http_client.request(hyper_req)).await;

    match upstream {
        Ok(Ok(upstream_response)) => {
            debug!(
                "Received response from upstream: {:?}",
                upstream_response.status()
            );
            let store = store_plan(&req, &state, cache_key, &request_policy, &upstream_response);
            client_response(upstream_response, store)
        }
        Ok(Err(e)) => {
            error!("Error forwarding request to upstream {}: {}", target_uri, e);
            HttpResponse::BadGateway().body("bad gateway")
        }
        Err(_) => {
            error!(
                "Upstream {} sent no response headers within {:?}",
                target_uri, state.upstream_timeout
            );
            HttpResponse::GatewayTimeout().body("gateway timeout")
        }
    }
}

/// the client's response from the cache, when a fresh stored response may
/// answer this request.
fn serve_from_cache(
    req: &HttpRequest,
    state: &AppState,
    cache_key: &PrimaryKey,
    request_policy: &RequestPolicy,
) -> Option<HttpResponse> {
    if !request_policy.may_serve {
        return None;
    }
    let stored = state
        .cache
        .lookup(cache_key, req.headers(), request_policy.max_age)?;
    debug!("Cache hit for {} {}", req.method(), req.uri());
    state.cache_stats.hit();
    Some(stored_response(&stored))
}

/// the origin request for `req`, without its body, and its target URI.
/// `None` when the target URI is invalid.
fn upstream_request(
    req: &HttpRequest,
    state: &AppState,
    client: &ClientInfo,
    path_and_query: &str,
) -> Option<(hyper::http::request::Builder, Uri)> {
    // `Url` prints a bare host as `http://host/`, so trim the base path's
    // trailing slash before appending the request's own leading slash. slicing
    // at `AfterPath` also drops any query or fragment on the origin URL.
    let upstream_base = state.upstream_base_url[..Position::AfterPath].trim_end_matches('/');
    let target_url_str = format!("{}{}", upstream_base, path_and_query);

    let target_uri = Uri::try_from(&target_url_str)
        .map_err(|e| error!("Error constructing target URI '{}': {}", target_url_str, e))
        .ok()?;

    debug!("Forwarding request to: {}", target_uri);

    let mut builder = HyperRequest::builder()
        .method(req.method().clone())
        .uri(target_uri.clone());

    let options = connection_options(req.headers().get_all(header::CONNECTION));
    for (name, value) in req.headers().iter() {
        // the Host and forwarding headers are set below
        if name != header::HOST
            && !CLIENT_FORWARDING_HEADERS.contains(name)
            && !URL_OVERRIDE_HEADERS.contains(name)
            && is_end_to_end(name, &options)
        {
            builder = builder.header(name.clone(), value.clone());
        }
    }

    if let Some(host) = upstream_host(&state.upstream_base_url) {
        builder = builder.header(header::HOST, host);
    }
    if let Some(ip) = client.ip {
        builder = builder.header("X-Forwarded-For", ip.to_string());
    }
    builder = builder
        .header("X-Forwarded-Proto", client.scheme)
        .header("X-Forwarded-Host", client.host.as_str());

    Some((builder, target_uri))
}

/// the Host header for the origin: its host, and its port unless that is 80
/// or 443.
fn upstream_host(base: &Url) -> Option<String> {
    let host = base.host_str()?;
    Some(match base.port_or_known_default() {
        Some(80 | 443) | None => host.to_owned(),
        Some(port) => format!("{host}:{port}"),
    })
}

/// how to store the origin's response, if at all. a successful unsafe
/// request also removes the stored responses for its URI (RFC 9111 section
/// 4.4).
fn store_plan(
    req: &HttpRequest,
    state: &AppState,
    cache_key: PrimaryKey,
    request_policy: &RequestPolicy,
    upstream_response: &HyperResponse<Body>,
) -> Option<Storing> {
    let status = upstream_response.status();
    if !req.method().is_safe() && (status.is_success() || status.is_redirection()) {
        state.cache.invalidate(&cache_key);
    }
    let (storable, vary) = cache::storable(
        request_policy,
        req.headers(),
        status,
        upstream_response.headers(),
    )?;
    let cache = state.cache.clone();
    Some(Storing {
        limit: state.cache.max_entry(),
        finish: Box::new(move |status, headers, body| {
            cache.insert_response(cache_key, vary, &storable, status, headers, body)
        }),
    })
}

/// the request body for the origin. actix's `Payload` is `!Send` and hyper
/// needs a `Send` body, so a task on this worker's local executor copies the
/// payload into a hyper body channel as chunks arrive.
async fn request_body(mut payload: web::Payload) -> Result<Body, actix_web::error::PayloadError> {
    // an empty body stays `Body::empty()` so that hyper does not send
    // `Transfer-Encoding: chunked` on a GET
    let first = match payload.next().await {
        None => return Ok(Body::empty()),
        Some(chunk) => chunk?,
    };

    let (mut sender, body) = Body::channel();
    actix_web::rt::spawn(async move {
        if sender.send_data(first).await.is_err() {
            return;
        }
        while let Some(chunk) = payload.next().await {
            match chunk {
                Ok(chunk) => {
                    // the origin request has ended, so the rest is unwanted
                    if sender.send_data(chunk).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    warn!("Failed to read request body: {}", e);
                    sender.abort();
                    return;
                }
            }
        }
    });
    Ok(body)
}

/// how to store a cacheable response once its whole body has arrived.
struct Storing {
    /// the largest body to store
    limit: u64,
    finish: Box<dyn FnOnce(hyper::StatusCode, Fields, Bytes)>,
}

/// a body on its way into the cache.
struct BodyCopy {
    buffer: BytesMut,
    limit: u64,
    finish: Box<dyn FnOnce(Bytes)>,
}

/// turns the origin's response into the client's, streaming the body. the
/// response carries the origin's length when it sent one. with `store`, a
/// body no longer than its limit is also copied into the cache as it streams.
fn client_response(upstream: HyperResponse<Body>, store: Option<Storing>) -> HttpResponse {
    let (parts, body) = upstream.into_parts();
    let mut builder = HttpResponse::build(parts.status);

    let options = connection_options(parts.headers.get_all(header::CONNECTION));
    let mut stored_headers = Vec::new();
    for (name, value) in parts.headers.iter() {
        // actix writes Content-Length from the body size, so a copied header
        // would go stale when the Compress middleware re-encodes the body
        if name != header::CONTENT_LENGTH && name != CACHE_STATUS && is_end_to_end(name, &options) {
            builder.append_header((name.clone(), value.clone()));
            if name != header::AGE {
                stored_headers.push((name.clone(), value.clone()));
            }
        }
    }
    builder.insert_header((CACHE_STATUS, "MISS"));

    let body = body.map(|chunk| {
        chunk.map_err(|e| {
            error!("Error reading upstream response body: {}", e);
            e
        })
    });

    let length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let status = parts.status;
    let copy = store
        .filter(|store| length.is_none_or(|length| length <= store.limit))
        .map(|store| BodyCopy {
            buffer: BytesMut::new(),
            limit: store.limit,
            finish: Box::new(move |body| (store.finish)(status, stored_headers, body)),
        });
    let body = Tee::new(body, length, copy);

    match length {
        Some(length) => builder.body(SizedStream::new(length, body)),
        None => builder.streaming(body),
    }
}

/// the client's response from a stored one (RFC 9111 section 4). actix
/// leaves the body out when it answers a HEAD request.
fn stored_response(stored: &StoredResponse) -> HttpResponse {
    let mut builder = HttpResponse::build(stored.status);
    for (name, value) in &stored.headers {
        builder.append_header((name.clone(), value.clone()));
    }
    builder
        .insert_header((header::AGE, stored.age().as_secs().to_string()))
        .insert_header((CACHE_STATUS, "HIT"))
        .body(stored.body.clone())
}

/// passes a body stream through while copying it into a buffer. once the
/// body has ended within the limit, the buffer goes to the finish callback.
/// a body that passes the limit or fails is not kept.
struct Tee<S> {
    inner: S,
    length: Option<u64>,
    copy: Option<BodyCopy>,
}

impl<S> Tee<S> {
    fn new(inner: S, length: Option<u64>, copy: Option<BodyCopy>) -> Self {
        let mut tee = Tee {
            inner,
            length,
            copy,
        };
        // actix never polls a sized body of length 0
        if length == Some(0) {
            tee.finish();
        }
        tee
    }

    fn finish(&mut self) {
        if let Some(copy) = self.copy.take() {
            if self
                .length
                .is_none_or(|length| length == copy.buffer.len() as u64)
            {
                (copy.finish)(copy.buffer.freeze());
            }
        }
    }
}

impl<S> Stream for Tee<S>
where
    S: Stream<Item = Result<Bytes, hyper::Error>> + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let item = std::task::ready!(this.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(chunk)) => {
                if let Some(BodyCopy { buffer, limit, .. }) = &mut this.copy {
                    if (buffer.len() + chunk.len()) as u64 > *limit {
                        debug!("Response body passed the cache entry limit");
                        this.copy = None;
                    } else {
                        buffer.extend_from_slice(chunk);
                        // actix stops polling a sized body at its length
                        if this.length == Some(buffer.len() as u64) {
                            this.finish();
                        }
                    }
                }
            }
            Some(Err(_)) => this.copy = None,
            None => this.finish(),
        }
        Poll::Ready(item)
    }
}

/// the lowercased header names that a `Connection` header lists, which are
/// hop-by-hop for this message (RFC 9110 section 7.6.1).
fn connection_options<'a>(values: impl IntoIterator<Item = &'a HeaderValue>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// whether a proxy forwards `name`. RFC 9110 section 7.6.1 names
/// `Connection`, `Keep-Alive`, `Proxy-Connection`, `TE`, `Transfer-Encoding`
/// and `Upgrade`. the proxy auth headers apply to one hop (section 11.7).
/// `Trailer` goes too, because the body stream drops trailer fields.
fn is_end_to_end(name: &HeaderName, connection_options: &[String]) -> bool {
    let name = name.as_str();
    let hop_by_hop = matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    );
    !hop_by_hop && !connection_options.iter().any(|option| option == name)
}
