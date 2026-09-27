use actix_web::body::SizedStream;
use actix_web::error::PayloadError;
use actix_web::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use actix_web::http::{Method, StatusCode};
use actix_web::{web, HttpRequest, HttpResponse};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request as HyperRequest, Uri};
use log::{debug, error, warn};
use std::convert::TryFrom;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use url::{Position, Url};

use crate::cache::{
    self, Fields, FlightKey, Lookup, PrimaryKey, RequestPolicy, StaleEntry, StoredResponse,
    PRECONDITION_HEADERS,
};
use crate::coalesce::{FlightGuard, Join, Waiter};
use crate::forwarded::{ClientInfo, CLIENT_FORWARDING_HEADERS, URL_OVERRIDE_HEADERS};
use crate::{AppState, CacheStats};

const CACHE_STATUS: &str = "x-shadowstep-cache";

type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// the body of a request to the origin.
pub(crate) type UpstreamBody = UnsyncBoxBody<Bytes, BodyError>;

type Flight = FlightGuard<FlightKey>;

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
    let ToOrigin { stale, flight } =
        match serve_from_cache(&req, &state, &cache_key, &request_policy).await {
            Before::Answer(response) => return response,
            Before::Origin(to_origin) => to_origin,
        };

    let Some((builder, target_uri)) = upstream_request(&req, &state, &client, path_and_query)
    else {
        state.cache_stats.miss();
        return error_response(StatusCode::INTERNAL_SERVER_ERROR);
    };
    // a client's own preconditions go to the origin unchanged, and then the
    // origin's answer is for the client, not for the stored response
    let revalidating = stale.as_ref().filter(|_| !request_policy.conditional);
    let builder = match revalidating {
        Some(entry) => with_validators(builder, &entry.response),
        None => builder,
    };
    let exchange = Exchange {
        revalidating: revalidating.is_some(),
        req,
        state,
        cache_key,
        request_policy,
        stale,
        flight,
    };

    let hyper_req = match origin_request(builder, payload).await {
        Ok(hyper_req) => hyper_req,
        Err(status) => {
            exchange.state.cache_stats.miss();
            return error_response(status);
        }
    };

    let upstream = tokio::time::timeout(
        exchange.state.upstream_timeout,
        exchange.state.http_client.request(hyper_req),
    )
    .await;

    match upstream {
        Ok(Ok(upstream_response)) => {
            debug!(
                "Received response from upstream: {:?}",
                upstream_response.status()
            );
            exchange.answered(upstream_response)
        }
        Ok(Err(e)) => {
            error!("Error forwarding request to upstream {}: {}", target_uri, e);
            exchange.failed(StatusCode::BAD_GATEWAY)
        }
        Err(_) => {
            error!(
                "Upstream {} sent no response headers within {:?}",
                target_uri, exchange.state.upstream_timeout
            );
            exchange.failed(StatusCode::GATEWAY_TIMEOUT)
        }
    }
}

/// what the cache did for a request before any origin request.
enum Before {
    /// the cache answers the request
    Answer(HttpResponse),
    /// the request goes to the origin
    Origin(ToOrigin),
}

/// what a request takes to the origin.
struct ToOrigin {
    /// the stored response to revalidate
    stale: Option<StaleEntry>,
    /// the flight this request leads, which other requests wait for
    flight: Option<Flight>,
}

/// the cache's answer to the request, if it has one. a request that may
/// coalesce and finds no response it can use as is joins the flight for
/// its key: the first such request leads it to the origin, and the others
/// wait for the leader to finish and then look again.
async fn serve_from_cache(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    cache_key: &PrimaryKey,
    request_policy: &RequestPolicy,
) -> Before {
    let to_origin = match from_cache(req, state, cache_key, request_policy, FreshUse::Hit).or(None)
    {
        Before::Origin(to_origin) => to_origin,
        answer => return answer,
    };
    let (flight, fresh_use) = match join_flight(req, state, cache_key, request_policy) {
        Join::Alone => return Before::Origin(to_origin),
        Join::Lead(flight) => (Some(flight), FreshUse::Hit),
        Join::Follow(waiter) => {
            wait_for_leader(req, state, waiter).await;
            (None, FreshUse::Coalesced)
        }
    };
    // a leader looks again because a flight for the key may have stored a
    // response and ended since the first lookup. a follower that still
    // finds nothing it may use goes to the origin alone, because the
    // leader's response was not storable, did not match this request's
    // `Vary` values or failed, and RFC 9111 section 3 lets a shared cache
    // pass on only what it stores.
    from_cache(req, state, cache_key, request_policy, fresh_use).or(flight)
}

/// the request's part in the flight for its key.
fn join_flight(
    req: &HttpRequest,
    state: &AppState,
    cache_key: &PrimaryKey,
    request_policy: &RequestPolicy,
) -> Join<FlightKey> {
    if !request_policy.may_coalesce() {
        return Join::Alone;
    }
    match state.cache.flight_key(cache_key, req.headers()) {
        Some(key) => state.flights.join(key, request_policy.may_lead()),
        None => Join::Alone,
    }
}

/// waits for the leader of a flight for no longer than the leader may wait
/// for the origin's response headers. a follower whose wait runs out goes
/// to the origin itself.
async fn wait_for_leader(req: &HttpRequest, state: &AppState, waiter: Waiter) {
    if tokio::time::timeout(state.upstream_timeout, waiter.wait())
        .await
        .is_err()
    {
        debug!("Stopped waiting for a concurrent request for {}", req.uri());
    }
}

/// how a fresh stored response came to answer a request.
#[derive(Clone, Copy)]
enum FreshUse {
    /// found on the request's own lookup
    Hit,
    /// stored by the flight this request waited for
    Coalesced,
}

impl FreshUse {
    /// counts the use and returns its `X-Shadowstep-Cache` value.
    fn count(self, stats: &CacheStats) -> &'static str {
        match self {
            FreshUse::Hit => {
                stats.hit();
                "HIT"
            }
            FreshUse::Coalesced => {
                stats.coalesced();
                "COALESCED"
            }
        }
    }
}

/// what the cache can do for a request before it goes to the origin.
enum Cached {
    /// the cache answers the request
    Answer(HttpResponse),
    /// the request goes to the origin to revalidate this stored response
    Stale(StaleEntry),
    /// the request goes to the origin
    Nothing,
}

impl Cached {
    /// the cache's answer, or the origin request with `flight`.
    fn or(self, flight: Option<Flight>) -> Before {
        let stale = match self {
            Cached::Answer(response) => return Before::Answer(response),
            Cached::Stale(entry) => Some(entry),
            Cached::Nothing => None,
        };
        Before::Origin(ToOrigin { stale, flight })
    }
}

/// the stored response for this request, if any, and whether it answers the
/// request. only GET requests use stale responses, because a revalidation
/// or a stored replacement needs a GET to the origin.
fn from_cache(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    cache_key: &PrimaryKey,
    request_policy: &RequestPolicy,
    fresh_use: FreshUse,
) -> Cached {
    if !request_policy.may_serve {
        return Cached::Nothing;
    }
    match state
        .cache
        .lookup(cache_key, req.headers(), request_policy.max_age)
    {
        Some(Lookup::Fresh(stored)) => {
            debug!("Cache hit for {} {}", req.method(), req.uri());
            let cache_status = fresh_use.count(&state.cache_stats);
            Cached::Answer(cached_response(&stored, cache_status))
        }
        Some(Lookup::Stale(entry)) if req.method() == Method::GET => {
            while_revalidating(req, state, cache_key, request_policy, entry)
        }
        _ => Cached::Nothing,
    }
}

/// the stale response and a background revalidation when the response's
/// stale-while-revalidate window allows it (RFC 5861 section 3), or else
/// the entry to revalidate before use.
fn while_revalidating(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    cache_key: &PrimaryKey,
    request_policy: &RequestPolicy,
    entry: StaleEntry,
) -> Cached {
    if !request_policy.may_serve_stale() || !entry.response.may_serve_while_revalidating() {
        return Cached::Stale(entry);
    }
    debug!("Serving stale {} while revalidating", req.uri());
    state.cache_stats.stale();
    let response = cached_response(&entry.response, "STALE");
    refresh_in_background(req, state, cache_key.clone(), entry);
    Cached::Answer(response)
}

/// revalidates `entry` in a task on this worker, unless another task is
/// already revalidating it. the origin request is built as for a miss, with
/// the stored validators in place of the client's preconditions.
fn refresh_in_background(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    cache_key: PrimaryKey,
    entry: StaleEntry,
) {
    let Some(guard) = state.cache.start_refresh(&entry) else {
        return;
    };
    let client = ClientInfo::from_request(req);
    let path_and_query = req.uri().path_and_query().map_or("", |pq| pq.as_str());
    let Some((mut builder, target_uri)) = upstream_request(req, state, &client, path_and_query)
    else {
        return;
    };
    if let Some(headers) = builder.headers_mut() {
        for name in &PRECONDITION_HEADERS {
            headers.remove(name.as_str());
        }
    }
    let Ok(hyper_req) = with_validators(builder, &entry.response).body(empty_body()) else {
        return;
    };
    state.cache_stats.background_refresh();
    let (req, state) = (req.clone(), state.clone());
    actix_web::rt::spawn(async move {
        let _guard = guard;
        let upstream =
            tokio::time::timeout(state.upstream_timeout, state.http_client.request(hyper_req))
                .await;
        match upstream {
            Ok(Ok(response)) => refresh(&req, &state, cache_key, &entry, response).await,
            Ok(Err(e)) => warn!("Background revalidation of {} failed: {}", target_uri, e),
            Err(_) => warn!("Background revalidation of {} timed out", target_uri),
        }
    });
}

/// updates `entry` from the origin's answer to a background revalidation: a
/// 304 freshens it, and a storable response replaces it. anything else
/// leaves it for its stale windows.
async fn refresh(
    req: &HttpRequest,
    state: &AppState,
    cache_key: PrimaryKey,
    entry: &StaleEntry,
    response: hyper::Response<Incoming>,
) {
    let (parts, body) = response.into_parts();
    let head = Head::from_upstream(&parts);
    let request_policy = RequestPolicy::new(req.method(), req.headers());
    if head.status == StatusCode::NOT_MODIFIED {
        state.cache.freshen(
            entry,
            &request_policy,
            req.headers(),
            &stored_fields(forwardable_fields(&head)),
            head.map.get(header::AGE),
        );
        state.cache_stats.revalidation();
        return;
    }
    let Some(store) = store_plan(req, state, cache_key, &request_policy, &head) else {
        return;
    };
    let limit = usize::try_from(store.limit).unwrap_or(usize::MAX);
    match Limited::new(body, limit).collect().await {
        Ok(collected) => (store.finish)(
            head.status,
            stored_fields(forwardable_fields(&head)),
            collected.to_bytes(),
        ),
        Err(e) => debug!("Background revalidation body not stored: {}", e),
    }
}

/// statuses for which stale-if-error serves a stale response in place of
/// the origin's (RFC 5861 section 4).
const ERROR_STATUSES: [StatusCode; 4] = [
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// one request on its way to the origin, and the stored response it may
/// revalidate.
struct Exchange {
    req: HttpRequest,
    state: web::Data<AppState>,
    cache_key: PrimaryKey,
    request_policy: RequestPolicy,
    stale: Option<StaleEntry>,
    /// whether the origin request carries the stale response's validators
    revalidating: bool,
    /// the flight this request leads. it ends when the exchange does, or
    /// once the response is stored or known not to be
    flight: Option<Flight>,
}

impl Exchange {
    /// the client's response once the origin has answered.
    fn answered(self, response: hyper::Response<Incoming>) -> HttpResponse {
        let (parts, body) = response.into_parts();
        let head = Head::from_upstream(&parts);
        if let Some(stale) = &self.stale {
            if self.revalidating && head.status == StatusCode::NOT_MODIFIED {
                return self.revalidated(stale, &head);
            }
            if ERROR_STATUSES.contains(&head.status) && self.may_serve_on_error(stale) {
                return self.serve_stale(stale);
            }
        }
        self.state.cache_stats.miss();
        let flight = self.flight;
        let store = store_plan(
            &self.req,
            &self.state,
            self.cache_key,
            &self.request_policy,
            &head,
        );
        client_response(head, body, store.map(|store| store.holding(flight)))
    }

    /// the client's response when the origin could not be reached or sent no
    /// response headers in time. a response that must be revalidated gets a
    /// 504 (RFC 9111 section 5.2.2.2).
    fn failed(self, status: StatusCode) -> HttpResponse {
        let mut status = status;
        if let Some(stale) = &self.stale {
            if self.may_serve_on_error(stale) {
                return self.serve_stale(stale);
            }
            if stale.response.must_revalidate() {
                status = StatusCode::GATEWAY_TIMEOUT;
            }
        }
        self.state.cache_stats.miss();
        error_response(status)
    }

    fn may_serve_on_error(&self, stale: &StaleEntry) -> bool {
        self.request_policy.may_serve_stale() && stale.response.may_serve_on_error()
    }

    fn serve_stale(&self, stale: &StaleEntry) -> HttpResponse {
        debug!(
            "Serving stale {} in place of an origin error",
            self.req.uri()
        );
        self.state.cache_stats.stale();
        cached_response(&stale.response, "STALE")
    }

    /// the stored body with the fields of the origin's 304, which also
    /// freshens the stored response.
    fn revalidated(&self, stale: &StaleEntry, head: &Head) -> HttpResponse {
        let fresh = self.state.cache.freshen(
            stale,
            &self.request_policy,
            self.req.headers(),
            &stored_fields(forwardable_fields(head)),
            head.map.get(header::AGE),
        );
        self.state.cache_stats.revalidation();
        cached_response(&fresh, "REVALIDATED")
    }
}

/// `builder` with the conditional fields from `stored`'s validators (RFC
/// 9111 section 4.3.1).
fn with_validators(
    mut builder: hyper::http::request::Builder,
    stored: &StoredResponse,
) -> hyper::http::request::Builder {
    for (name, value) in stored.validators() {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder
}

/// the origin request from `builder` and the client's body, or the status
/// of the client's error response.
async fn origin_request(
    builder: hyper::http::request::Builder,
    payload: web::Payload,
) -> Result<HyperRequest<UpstreamBody>, StatusCode> {
    let body = request_body(payload).await.map_err(|e| {
        warn!("Failed to read request body: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    builder.body(body).map_err(|e| {
        error!("Failed to build hyper request: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// a plain error response, such as "bad gateway" for a 502.
fn error_response(status: StatusCode) -> HttpResponse {
    let reason = status.canonical_reason().unwrap_or_default();
    HttpResponse::build(status).body(reason.to_ascii_lowercase())
}

fn empty_body() -> UpstreamBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
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

    // actix-web 4 uses `http` 0.2 and hyper 1 uses `http` 1, so the method
    // and fields cross as strings and bytes
    let mut builder = HyperRequest::builder()
        .method(req.method().as_str())
        .uri(target_uri.clone());

    let options = connection_options(req.headers().get_all(header::CONNECTION));
    for (name, value) in req.headers().iter() {
        // the Host and forwarding headers are set below
        if name != header::HOST
            && !CLIENT_FORWARDING_HEADERS.contains(name)
            && !URL_OVERRIDE_HEADERS.contains(name)
            && is_end_to_end(name, &options)
        {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
    }

    if let Some(host) = upstream_host(&state.upstream_base_url) {
        builder = builder.header(hyper::header::HOST, host);
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
    head: &Head,
) -> Option<Storing> {
    let status = head.status;
    if !req.method().is_safe() && (status.is_success() || status.is_redirection()) {
        state.cache.invalidate(&cache_key);
    }
    let (storable, vary) = cache::storable(request_policy, req.headers(), status, &head.map)?;
    let cache = state.cache.clone();
    Some(Storing {
        limit: state.cache.max_entry(),
        finish: Box::new(move |status, headers, body| {
            cache.insert_response(cache_key, vary, &storable, status, headers, body);
        }),
    })
}

/// the request body for the origin. actix's `Payload` is `!Send` and hyper
/// needs a `Send` body, so a task on this worker's local executor copies the
/// payload into a channel that the body reads, as chunks arrive.
async fn request_body(mut payload: web::Payload) -> Result<UpstreamBody, PayloadError> {
    // an empty body stays empty so that hyper does not send
    // `Transfer-Encoding: chunked` on a GET
    let first = match payload.next().await {
        None => return Ok(empty_body()),
        Some(chunk) => chunk?,
    };

    let (sender, mut receiver) = mpsc::channel(1);
    actix_web::rt::spawn(pump_request_body(first, payload, sender));
    let frames = futures_util::stream::poll_fn(move |cx| receiver.poll_recv(cx));
    Ok(StreamBody::new(frames).boxed_unsync())
}

/// sends `first` and the rest of `payload` to the origin request body. a
/// payload error goes to the body too, so that hyper fails the request
/// rather than end a truncated body.
async fn pump_request_body(
    first: Bytes,
    mut payload: web::Payload,
    sender: mpsc::Sender<Result<Frame<Bytes>, BodyError>>,
) {
    if sender.send(Ok(Frame::data(first))).await.is_err() {
        return;
    }
    while let Some(chunk) = payload.next().await {
        let frame = chunk.map(Frame::data).map_err(|e| {
            warn!("Failed to read request body: {}", e);
            BodyError::from(e)
        });
        let failed = frame.is_err();
        // a closed channel means the origin request has ended, so the rest
        // is unwanted
        if sender.send(frame).await.is_err() || failed {
            return;
        }
    }
}

/// the origin response's status and fields as actix types.
struct Head {
    status: StatusCode,
    /// the fields in the order the origin sent them
    fields: Fields,
    map: HeaderMap,
}

impl Head {
    fn from_upstream(parts: &hyper::http::response::Parts) -> Self {
        // both `http` versions accept the status codes 100 to 999
        let status = StatusCode::from_u16(parts.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        // both `http` versions validate names and values by the same rules,
        // so no field is dropped here
        let fields: Fields = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                Some((
                    HeaderName::from_bytes(name.as_str().as_bytes()).ok()?,
                    HeaderValue::from_bytes(value.as_bytes()).ok()?,
                ))
            })
            .collect();
        let mut map = HeaderMap::with_capacity(fields.len());
        for (name, value) in &fields {
            map.append(name.clone(), value.clone());
        }
        Head {
            status,
            fields,
            map,
        }
    }
}

/// how to store a cacheable response once its whole body has arrived.
struct Storing {
    /// the largest body to store
    limit: u64,
    finish: Box<dyn FnOnce(StatusCode, Fields, Bytes)>,
}

impl Storing {
    /// keeps `flight` until the response is stored, or until the copy of
    /// its body is given up because it passed the limit, failed or was
    /// dropped with the client's response, so that followers look up the
    /// cache only once the outcome is known.
    fn holding(self, flight: Option<Flight>) -> Self {
        let Storing { limit, finish } = self;
        Storing {
            limit,
            finish: Box::new(move |status, headers, body| {
                finish(status, headers, body);
                drop(flight);
            }),
        }
    }
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
fn client_response(head: Head, body: Incoming, store: Option<Storing>) -> HttpResponse {
    let mut builder = HttpResponse::build(head.status);

    let fields = forwardable_fields(&head);
    for (name, value) in &fields {
        builder.append_header((name.clone(), value.clone()));
    }
    builder.insert_header((CACHE_STATUS, "MISS"));
    let stored_headers = stored_fields(fields);

    // the data stream drops trailer frames
    let body = body.into_data_stream().map(|chunk| {
        chunk.map_err(|e| {
            error!("Error reading upstream response body: {}", e);
            e
        })
    });

    let length = head
        .map
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let status = head.status;
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

/// the origin response's fields that go to the client.
fn forwardable_fields(head: &Head) -> Fields {
    let options = connection_options(head.map.get_all(header::CONNECTION));
    head.fields
        .iter()
        // actix writes Content-Length from the body size, so a copied header
        // would go stale when the Compress middleware re-encodes the body
        .filter(|(name, _)| {
            name != header::CONTENT_LENGTH && name != CACHE_STATUS && is_end_to_end(name, &options)
        })
        .cloned()
        .collect()
}

/// forwardable fields without `Age`, which the store works out on each use.
fn stored_fields(mut fields: Fields) -> Fields {
    fields.retain(|(name, _)| name != header::AGE);
    fields
}

/// the client's response from a stored one (RFC 9111 section 4), marked
/// with `cache_status`. actix leaves the body out when it answers a HEAD
/// request.
fn cached_response(stored: &StoredResponse, cache_status: &'static str) -> HttpResponse {
    let mut builder = HttpResponse::build(stored.status);
    for (name, value) in &stored.headers {
        builder.append_header((name.clone(), value.clone()));
    }
    builder
        .insert_header((header::AGE, stored.age().as_secs().to_string()))
        .insert_header((CACHE_STATUS, cache_status))
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
