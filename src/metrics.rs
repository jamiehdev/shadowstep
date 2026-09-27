use actix_web::http::StatusCode;
use actix_web::HttpResponse;
use prometheus::core::Collector;
use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use std::time::Duration;

use crate::cache::Store;

/// the `Content-Type` of the Prometheus text exposition format 0.0.4.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// the `X-Shadowstep-Cache` values, plus `BYPASS` for a request that could
/// neither be answered from the cache nor stored in it.
pub const HIT: &str = "HIT";
pub const MISS: &str = "MISS";
pub const REVALIDATED: &str = "REVALIDATED";
pub const STALE: &str = "STALE";
pub const COALESCED: &str = "COALESCED";
pub const BYPASS: &str = "BYPASS";

const CACHE_LABELS: [&str; 6] = [HIT, MISS, REVALIDATED, STALE, COALESCED, BYPASS];
const ASSET_CACHE_LABELS: [&str; 3] = [HIT, MISS, BYPASS];
const STATUS_CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

/// upper bounds of the origin response time buckets, in seconds. 30 s is
/// the default upstream timeout.
const ORIGIN_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// the handler a request reached. paths are left out of labels because
/// each one would be a new series.
#[derive(Clone, Copy)]
pub enum Route {
    Proxy,
    Asset,
}

impl Route {
    fn label(self) -> &'static str {
        match self {
            Route::Proxy => "proxy",
            Route::Asset => "asset",
        }
    }
}

/// whether an origin request answers a client or revalidates a stale
/// response in the background.
#[derive(Clone, Copy)]
pub enum OriginKind {
    Foreground,
    Background,
}

impl OriginKind {
    fn label(self) -> &'static str {
        match self {
            OriginKind::Foreground => "foreground",
            OriginKind::Background => "background",
        }
    }
}

/// why an origin request ended without response headers.
#[derive(Clone, Copy)]
pub enum OriginFailure {
    /// the connection or the request failed
    Error,
    /// no response headers within the upstream timeout
    Timeout,
}

/// the counts `/health` reports, read from the same counters as `/metrics`.
pub struct CacheCounts {
    pub hits: u64,
    pub misses: u64,
    pub revalidations: u64,
    pub stale: u64,
    pub coalesced: u64,
    pub background_refreshes: u64,
}

/// the proxy's Prometheus metrics, in a registry of their own so that each
/// `AppState` counts only its own requests.
pub struct Metrics {
    registry: Registry,
    requests: IntCounterVec,
    responses: IntCounterVec,
    origin_requests: IntCounterVec,
    origin_response_seconds: Histogram,
    origin_body_idle_timeouts: IntCounterVec,
    revalidations: IntCounter,
    background_refreshes: IntCounter,
    cache_bytes: IntGauge,
    cache_entries: IntGauge,
}

/// registers `metric` in `registry`. every name, label name and bucket
/// list here is a constant, so an error means a programming mistake, which
/// any test that builds an `AppState` catches.
fn register<M: Collector + Clone + 'static>(
    registry: &Registry,
    metric: prometheus::Result<M>,
) -> M {
    let metric = metric.expect("invalid metric definition");
    registry
        .register(Box::new(metric.clone()))
        .expect("duplicate metric name");
    metric
}

fn counter_vec(name: &str, help: &str, labels: &[&str]) -> prometheus::Result<IntCounterVec> {
    IntCounterVec::new(Opts::new(name, help), labels)
}

const REQUESTS_HELP: &str = "Proxied and asset requests, by route and X-Shadowstep-Cache value, \
    or BYPASS for requests the cache could neither answer nor store.";
const RESPONSES_HELP: &str = "Responses to proxied and asset requests, by route and status class.";
const ORIGIN_REQUESTS_HELP: &str = "Requests to the origin, by kind and by the status class of \
    the response, or error or timeout when no response headers arrived.";
const ORIGIN_RESPONSE_SECONDS_HELP: &str =
    "Time from sending an origin request to receiving its response headers.";
const BODY_IDLE_HELP: &str =
    "Origin response bodies given up after the origin sent nothing for the upstream timeout.";
const REVALIDATIONS_HELP: &str = "304 responses from the origin that freshened a stored \
    response, in the foreground or the background.";
const BACKGROUND_REFRESHES_HELP: &str =
    "Background revalidations started under stale-while-revalidate.";
const CACHE_BYTES_HELP: &str = "Bytes held in the cache, for origin responses and assets.";
const CACHE_ENTRIES_HELP: &str = "Entries in the cache, for origin responses and assets.";

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let r = &registry;
        let histogram = HistogramOpts::new(
            "shadowstep_origin_response_seconds",
            ORIGIN_RESPONSE_SECONDS_HELP,
        )
        .buckets(ORIGIN_BUCKETS.to_vec());
        let metrics = Metrics {
            requests: register(
                r,
                counter_vec(
                    "shadowstep_requests_total",
                    REQUESTS_HELP,
                    &["route", "cache"],
                ),
            ),
            responses: register(
                r,
                counter_vec(
                    "shadowstep_responses_total",
                    RESPONSES_HELP,
                    &["route", "status_class"],
                ),
            ),
            origin_requests: register(
                r,
                counter_vec(
                    "shadowstep_origin_requests_total",
                    ORIGIN_REQUESTS_HELP,
                    &["kind", "outcome"],
                ),
            ),
            origin_response_seconds: register(r, Histogram::with_opts(histogram)),
            origin_body_idle_timeouts: register(
                r,
                counter_vec(
                    "shadowstep_origin_body_idle_timeouts_total",
                    BODY_IDLE_HELP,
                    &["kind"],
                ),
            ),
            revalidations: register(
                r,
                IntCounter::new("shadowstep_cache_revalidations_total", REVALIDATIONS_HELP),
            ),
            background_refreshes: register(
                r,
                IntCounter::new(
                    "shadowstep_cache_background_refreshes_total",
                    BACKGROUND_REFRESHES_HELP,
                ),
            ),
            cache_bytes: register(r, IntGauge::new("shadowstep_cache_bytes", CACHE_BYTES_HELP)),
            cache_entries: register(
                r,
                IntGauge::new("shadowstep_cache_entries", CACHE_ENTRIES_HELP),
            ),
            registry,
        };
        metrics.initialise_series();
        metrics
    }

    /// creates each expected series at 0, so that `rate()` sees the first
    /// increment of a series.
    fn initialise_series(&self) {
        for cache in CACHE_LABELS {
            self.requests.with_label_values(&["proxy", cache]);
        }
        for cache in ASSET_CACHE_LABELS {
            self.requests.with_label_values(&["asset", cache]);
        }
        for kind in [OriginKind::Foreground, OriginKind::Background] {
            for outcome in STATUS_CLASSES.iter().chain(&["error", "timeout"]) {
                self.origin_requests
                    .with_label_values(&[kind.label(), outcome]);
            }
            self.origin_body_idle_timeouts
                .with_label_values(&[kind.label()]);
        }
        for route in [Route::Proxy, Route::Asset] {
            for class in STATUS_CLASSES {
                self.responses.with_label_values(&[route.label(), class]);
            }
        }
    }

    /// counts a response to a proxied or asset request.
    pub fn observe(&self, route: Route, cache: &str, status: StatusCode) {
        self.requests
            .with_label_values(&[route.label(), cache])
            .inc();
        self.responses
            .with_label_values(&[route.label(), status_class(status.as_u16())])
            .inc();
    }

    /// counts an origin request whose response headers arrived after
    /// `elapsed`.
    pub fn origin_responded(&self, kind: OriginKind, status: u16, elapsed: Duration) {
        self.origin_requests
            .with_label_values(&[kind.label(), status_class(status)])
            .inc();
        self.origin_response_seconds.observe(elapsed.as_secs_f64());
    }

    /// counts an origin request that ended without response headers.
    pub fn origin_failed(&self, kind: OriginKind, failure: OriginFailure) {
        let outcome = match failure {
            OriginFailure::Error => "error",
            OriginFailure::Timeout => "timeout",
        };
        self.origin_requests
            .with_label_values(&[kind.label(), outcome])
            .inc();
    }

    /// the counter of origin bodies of `kind` given up for going idle.
    pub fn body_idle_timeouts(&self, kind: OriginKind) -> IntCounter {
        self.origin_body_idle_timeouts
            .with_label_values(&[kind.label()])
    }

    /// a 304 that freshened a stored response, in the foreground or the
    /// background
    pub fn revalidation(&self) {
        self.revalidations.inc();
    }

    pub fn background_refresh(&self) {
        self.background_refreshes.inc();
    }

    fn requests(&self, route: Route, cache: &str) -> u64 {
        self.requests
            .with_label_values(&[route.label(), cache])
            .get()
    }

    fn both_routes(&self, cache: &str) -> u64 {
        self.requests(Route::Proxy, cache) + self.requests(Route::Asset, cache)
    }

    /// the counts for `/health`. its misses are the responses that came
    /// from the origin, bypass or not, and asset misses. an asset request
    /// that found no file counts as a bypass here and not in `/health`.
    pub fn cache_counts(&self) -> CacheCounts {
        CacheCounts {
            hits: self.both_routes(HIT),
            misses: self.both_routes(MISS) + self.requests(Route::Proxy, BYPASS),
            revalidations: self.revalidations.get(),
            stale: self.both_routes(STALE),
            coalesced: self.both_routes(COALESCED),
            background_refreshes: self.background_refreshes.get(),
        }
    }

    /// the text exposition, with the cache gauges read from `store` now.
    pub fn encode(&self, store: &Store) -> Vec<u8> {
        self.cache_bytes.set(gauge_value(store.weighted_size()));
        self.cache_entries.set(gauge_value(store.entry_count()));
        let mut buffer = Vec::new();
        if let Err(e) = TextEncoder::new().encode(&self.registry.gather(), &mut buffer) {
            log::error!("Failed to encode metrics: {}", e);
        }
        buffer
    }
}

fn gauge_value(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `2xx` for 200 to 299 and so on. an origin may send a status up to 999,
/// which counts as `other`.
fn status_class(status: u16) -> &'static str {
    match status / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

/// the response's `X-Shadowstep-Cache` value, if it is one the proxy sets.
pub fn cache_label(response: &HttpResponse) -> Option<&'static str> {
    let value = response.headers().get("x-shadowstep-cache")?;
    CACHE_LABELS
        .into_iter()
        .find(|label| value.as_bytes() == label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_cover_each_hundred() {
        assert_eq!(status_class(100), "1xx");
        assert_eq!(status_class(299), "2xx");
        assert_eq!(status_class(304), "3xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(599), "5xx");
        assert_eq!(status_class(600), "other");
    }
}
