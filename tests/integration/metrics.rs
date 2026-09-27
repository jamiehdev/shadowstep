use crate::common::{self, cache_status, metric, metric_sum, parse_exposition, Sample};

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::test;
use bytes::Bytes;
use http_body_util::Empty;
use std::time::{Duration, Instant};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

fn get(uri: &str) -> test::TestRequest {
    test::TestRequest::get().uri(uri)
}

/// sends `req` and reads the whole response, so that a streamed origin body
/// has ended before the next scrape.
async fn call<S, B>(app: &S, req: test::TestRequest) -> (StatusCode, String)
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let cache = cache_status(&resp);
    test::read_body(resp).await;
    (status, cache)
}

async fn scrape<S, B>(app: &S) -> Vec<Sample>
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let resp = test::call_service(app, get("/metrics").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    parse_exposition(std::str::from_utf8(&body).unwrap())
}

async fn health<S, B>(app: &S) -> serde_json::Value
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let resp = test::call_service(app, get("/health").to_request()).await;
    test::read_body_json(resp).await
}

fn requests(samples: &[Sample], route: &str, cache: &str) -> Option<f64> {
    metric(
        samples,
        "shadowstep_requests_total",
        &[("route", route), ("cache", cache)],
    )
}

fn responses(samples: &[Sample], route: &str, class: &str) -> Option<f64> {
    metric(
        samples,
        "shadowstep_responses_total",
        &[("route", route), ("status_class", class)],
    )
}

fn origin(samples: &[Sample], kind: &str, outcome: &str) -> Option<f64> {
    metric(
        samples,
        "shadowstep_origin_requests_total",
        &[("kind", kind), ("outcome", outcome)],
    )
}

fn cacheable(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("cache-control", "max-age=60")
        .set_body_string(body)
}

#[actix_web::test]
async fn metrics_has_the_prometheus_text_content_type() {
    let (app, _assets) = common::service(&common::unreachable_origin()).await;

    let resp = test::call_service(&app, get("/metrics").to_request()).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), CONTENT_TYPE);
    let body = test::read_body(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("# TYPE shadowstep_requests_total counter"),
        "{text}"
    );
}

#[actix_web::test]
async fn miss_then_hit_count_under_their_own_series() {
    let origin_server = MockServer::start().await;
    Mock::given(path("/page"))
        .respond_with(cacheable("page"))
        .expect(1)
        .mount(&origin_server)
        .await;
    let (app, _assets) = common::service(&origin_server.uri()).await;

    assert_eq!(call(&app, get("/page")).await.1, "MISS");
    assert_eq!(call(&app, get("/page")).await.1, "HIT");
    let samples = scrape(&app).await;

    assert_eq!(requests(&samples, "proxy", "MISS"), Some(1.0));
    assert_eq!(requests(&samples, "proxy", "HIT"), Some(1.0));
    assert_eq!(metric_sum(&samples, "shadowstep_requests_total", &[]), 2.0);
    assert_eq!(responses(&samples, "proxy", "2xx"), Some(2.0));
    assert_eq!(origin(&samples, "foreground", "2xx"), Some(1.0));
    let health = health(&app).await;
    assert_eq!(health["cache"]["hits"], 1);
    assert_eq!(health["cache"]["misses"], 1);
}

#[actix_web::test]
async fn post_counts_as_bypass() {
    let origin_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&origin_server)
        .await;
    let (app, _assets) = common::service(&origin_server.uri()).await;

    let (status, _) = call(&app, test::TestRequest::post().uri("/form")).await;
    assert_eq!(status, StatusCode::CREATED);
    let samples = scrape(&app).await;

    assert_eq!(requests(&samples, "proxy", "BYPASS"), Some(1.0));
    assert_eq!(requests(&samples, "proxy", "MISS"), Some(0.0));
    assert_eq!(responses(&samples, "proxy", "2xx"), Some(1.0));
    assert_eq!(origin(&samples, "foreground", "2xx"), Some(1.0));
    // `/health` has always counted a bypass as a miss
    assert_eq!(health(&app).await["cache"]["misses"], 1);
}

#[actix_web::test]
async fn origin_502_and_timeout_count_by_outcome() {
    let origin_server = MockServer::start().await;
    Mock::given(path("/bad"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&origin_server)
        .await;
    Mock::given(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&origin_server)
        .await;
    let (app, _assets) =
        common::service_with(&origin_server.uri(), |c| c.upstream_timeout_seconds = 1).await;

    assert_eq!(call(&app, get("/bad")).await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        call(&app, get("/slow")).await.0,
        StatusCode::GATEWAY_TIMEOUT
    );
    let samples = scrape(&app).await;

    assert_eq!(origin(&samples, "foreground", "5xx"), Some(1.0));
    assert_eq!(origin(&samples, "foreground", "timeout"), Some(1.0));
    assert_eq!(
        metric_sum(&samples, "shadowstep_origin_requests_total", &[]),
        2.0
    );
    assert_eq!(responses(&samples, "proxy", "5xx"), Some(2.0));
    assert_eq!(requests(&samples, "proxy", "MISS"), Some(2.0));
}

#[actix_web::test]
async fn unreachable_origin_counts_as_error() {
    let (app, _assets) = common::service(&common::unreachable_origin()).await;

    assert_eq!(call(&app, get("/page")).await.0, StatusCode::BAD_GATEWAY);
    let samples = scrape(&app).await;

    assert_eq!(origin(&samples, "foreground", "error"), Some(1.0));
    assert_eq!(
        metric_sum(&samples, "shadowstep_origin_requests_total", &[]),
        1.0
    );
    assert_eq!(responses(&samples, "proxy", "5xx"), Some(1.0));
}

#[actix_web::test]
async fn histogram_counts_each_origin_response() {
    let origin_server = MockServer::start().await;
    Mock::given(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_server)
        .await;
    Mock::given(path("/gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&origin_server)
        .await;
    Mock::given(path("/bad"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&origin_server)
        .await;
    let (app, _assets) = common::service(&origin_server.uri()).await;

    for uri in ["/ok", "/gone", "/bad", "/ok"] {
        call(&app, get(uri)).await;
    }
    let samples = scrape(&app).await;

    let received = origin_server.received_requests().await.unwrap().len() as f64;
    assert_eq!(received, 4.0);
    let count = metric(&samples, "shadowstep_origin_response_seconds_count", &[]);
    assert_eq!(count, Some(received));
    let infinite = metric(
        &samples,
        "shadowstep_origin_response_seconds_bucket",
        &[("le", "+Inf")],
    );
    assert_eq!(infinite, Some(received));
    assert_eq!(
        metric_sum(&samples, "shadowstep_origin_requests_total", &[]),
        received
    );
    let sum = metric(&samples, "shadowstep_origin_response_seconds_sum", &[]).unwrap();
    assert!(sum > 0.0, "{sum}");
}

#[actix_web::test]
async fn cache_gauges_match_health() {
    let origin_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(cacheable("a stored body"))
        .mount(&origin_server)
        .await;
    let (app, assets) = common::service(&origin_server.uri()).await;
    std::fs::write(assets.path().join("app.css"), "body{}").unwrap();

    call(&app, get("/one")).await;
    call(&app, get("/two")).await;
    call(&app, get("/assets/app.css")).await;
    let samples = scrape(&app).await;
    let health = health(&app).await;

    let items = health["cache"]["items"].as_f64().unwrap();
    let bytes = health["cache"]["bytes"].as_f64().unwrap();
    assert_eq!(items, 3.0);
    assert!(bytes > 0.0);
    assert_eq!(
        metric(&samples, "shadowstep_cache_entries", &[]),
        Some(items)
    );
    assert_eq!(metric(&samples, "shadowstep_cache_bytes", &[]), Some(bytes));
}

#[actix_web::test]
async fn assets_count_under_their_own_route() {
    let (app, assets) = common::service(&common::unreachable_origin()).await;
    std::fs::write(assets.path().join("app.css"), "body{}").unwrap();

    assert_eq!(call(&app, get("/assets/app.css")).await.1, "MISS");
    assert_eq!(call(&app, get("/assets/app.css")).await.1, "HIT");
    assert_eq!(
        call(&app, get("/assets/missing.css")).await.0,
        StatusCode::NOT_FOUND
    );
    let samples = scrape(&app).await;

    assert_eq!(requests(&samples, "asset", "MISS"), Some(1.0));
    assert_eq!(requests(&samples, "asset", "HIT"), Some(1.0));
    assert_eq!(requests(&samples, "asset", "BYPASS"), Some(1.0));
    assert_eq!(responses(&samples, "asset", "2xx"), Some(2.0));
    assert_eq!(responses(&samples, "asset", "4xx"), Some(1.0));
    assert_eq!(
        metric_sum(&samples, "shadowstep_requests_total", &[("route", "proxy")]),
        0.0
    );
    // `/health` has never counted an asset that was not found
    let health = health(&app).await;
    assert_eq!(health["cache"]["hits"], 1);
    assert_eq!(health["cache"]["misses"], 1);
}

#[actix_web::test]
async fn labels_never_hold_paths() {
    let origin_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(cacheable("page"))
        .mount(&origin_server)
        .await;
    let (app, assets) = common::service(&origin_server.uri()).await;
    std::fs::write(assets.path().join("app.css"), "body{}").unwrap();

    for uri in ["/secret-page?token=abc", "/other", "/assets/app.css"] {
        call(&app, get(uri)).await;
    }
    call(&app, test::TestRequest::post().uri("/secret-form")).await;
    let samples = scrape(&app).await;

    assert!(!samples.is_empty());
    let allowed = ["route", "cache", "status_class", "kind", "outcome", "le"];
    for sample in &samples {
        for (key, value) in &sample.labels {
            assert!(allowed.contains(&key.as_str()), "{sample:?}");
            assert!(
                !value.contains('/') && !value.contains("secret") && !value.contains("app"),
                "{sample:?}"
            );
        }
    }
}

#[actix_web::test]
async fn metrics_and_health_are_not_proxied_or_counted() {
    let origin_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("origin metrics"))
        .expect(0)
        .mount(&origin_server)
        .await;
    let (app, _assets) = common::service(&origin_server.uri()).await;

    health(&app).await;
    scrape(&app).await;
    let samples = scrape(&app).await;

    assert_eq!(metric_sum(&samples, "shadowstep_requests_total", &[]), 0.0);
    assert_eq!(metric_sum(&samples, "shadowstep_responses_total", &[]), 0.0);
    origin_server.verify().await;
}

#[actix_web::test]
async fn background_revalidation_counts_as_background() {
    let origin_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=1, stale-while-revalidate=10")
                .insert_header("etag", "\"v1\"")
                .set_body_string("old"),
        )
        .up_to_n_times(1)
        .mount(&origin_server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=60"))
        .mount(&origin_server)
        .await;
    let (app, _assets) = common::service(&origin_server.uri()).await;

    call(&app, get("/page")).await;
    // max-age has a granularity of one second
    actix_web::rt::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(call(&app, get("/page")).await.1, "STALE");
    let deadline = Instant::now() + Duration::from_secs(5);
    let samples = loop {
        let samples = scrape(&app).await;
        if metric(&samples, "shadowstep_cache_revalidations_total", &[]) == Some(1.0) {
            break samples;
        }
        assert!(Instant::now() < deadline, "the background 304 never landed");
        actix_web::rt::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(origin(&samples, "foreground", "2xx"), Some(1.0));
    assert_eq!(origin(&samples, "background", "3xx"), Some(1.0));
    assert_eq!(
        metric(&samples, "shadowstep_cache_background_refreshes_total", &[]),
        Some(1.0)
    );
    assert_eq!(requests(&samples, "proxy", "STALE"), Some(1.0));
    assert_eq!(
        metric(&samples, "shadowstep_origin_response_seconds_count", &[]),
        Some(2.0)
    );
    let health = health(&app).await;
    assert_eq!(health["cache"]["revalidations"], 1);
    assert_eq!(health["cache"]["background_refreshes"], 1);
    assert_eq!(health["cache"]["stale"], 1);
}

/// a GET of `url` over a socket: the status, `Content-Type` and body.
async fn socket_get(url: &str) -> (u16, String, String) {
    let client = common::client::<Empty<Bytes>>();
    let resp = client.get(url.parse().unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = common::body_bytes(resp.into_body()).await;
    (
        status,
        content_type,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[actix_web::test]
async fn metrics_addr_moves_metrics_to_its_own_listener() {
    let origin_server = MockServer::start().await;
    Mock::given(path("/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string("origin metrics"))
        .expect(1)
        .mount(&origin_server)
        .await;
    Mock::given(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("page"))
        .expect(0)
        .mount(&origin_server)
        .await;
    let running = common::spawn_with_metrics_listener(&origin_server.uri());

    let (status, content_type, body) = socket_get(&running.metrics_url("/metrics")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, CONTENT_TYPE);
    assert!(
        body.contains("# TYPE shadowstep_requests_total counter"),
        "{body}"
    );

    // the proxy listener forwards `/metrics` to the origin like any path
    let (status, _, body) = socket_get(&running.proxy.url("/metrics")).await;
    assert_eq!(status, 200);
    assert_eq!(body, "origin metrics");

    // the metrics listener serves nothing else and proxies nothing
    let (status, _, _) = socket_get(&running.metrics_url("/page")).await;
    assert_eq!(status, 404);
    let (status, _, _) = socket_get(&running.metrics_url("/health")).await;
    assert_eq!(status, 404);

    let (_, _, body) = socket_get(&running.metrics_url("/metrics")).await;
    let samples = parse_exposition(&body);
    assert_eq!(requests(&samples, "proxy", "MISS"), Some(1.0));
    assert_eq!(metric_sum(&samples, "shadowstep_requests_total", &[]), 1.0);
    running.stop().await;
    origin_server.verify().await;
}

#[actix_web::test]
async fn metrics_is_served_on_the_proxy_listener_by_default() {
    let origin_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("origin metrics"))
        .expect(0)
        .mount(&origin_server)
        .await;
    let proxy = common::spawn(&origin_server.uri());

    let (status, content_type, body) = socket_get(&proxy.url("/metrics")).await;

    assert_eq!(status, 200);
    assert_eq!(content_type, CONTENT_TYPE);
    assert!(body.contains("shadowstep_cache_entries 0"), "{body}");
    proxy.stop().await;
    origin_server.verify().await;
}
