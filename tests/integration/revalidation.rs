use crate::common::{self, cache_status};

use actix_web::http::StatusCode;
use actix_web::test;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};
use wiremock::matchers::{any, header};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KIB: usize = 1024;

/// how long a `max-age=1` response takes to go stale, with a margin.
const PAST_ONE_SECOND: Duration = Duration::from_millis(1100);

fn get(uri: &str) -> test::TestRequest {
    test::TestRequest::get().uri(uri)
}

/// a 200 with `cache_control`, an `ETag` of `"v1"` and the body "old".
fn tagged(cache_control: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("cache-control", cache_control)
        .insert_header("etag", "\"v1\"")
        .set_body_string("old")
}

/// an origin that answers the first request with `first` and every later
/// request with `later`.
async fn origin_answering(first: ResponseTemplate, later: ResponseTemplate) -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(any())
        .respond_with(first)
        .up_to_n_times(1)
        .mount(&origin)
        .await;
    Mock::given(any()).respond_with(later).mount(&origin).await;
    origin
}

async fn origin_requests(origin: &MockServer) -> usize {
    origin.received_requests().await.unwrap().len()
}

/// the value of `name` on the origin's `n`th received request.
async fn received_header(origin: &MockServer, n: usize, name: &str) -> Option<String> {
    let received = origin.received_requests().await.unwrap();
    received[n]
        .headers
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
}

async fn health<S, B>(app: &S) -> serde_json::Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let resp = test::call_service(app, get("/health").to_request()).await;
    test::read_body_json(resp).await
}

fn age_of<B>(resp: &actix_web::dev::ServiceResponse<B>) -> u64 {
    resp.headers()
        .get("age")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// an origin that answers one connection with `response` and then stops
/// listening, so later requests are refused.
fn one_shot_origin(response: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        drop(listener);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
            line.clear();
        }
        let mut stream = stream;
        let _ = stream.write_all(response.as_bytes());
    });
    format!("http://{addr}")
}

/// a 200 with `cache_control`, an `ETag` of `"v1"` and the body "old", that
/// closes its connection.
fn one_shot_response(cache_control: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncache-control: {cache_control}\r\netag: \"v1\"\r\nconnection: close\r\ncontent-length: 3\r\n\r\nold"
    )
}

#[actix_web::test]
async fn not_modified_serves_the_stored_body_with_updated_headers() {
    let origin = origin_answering(
        tagged("max-age=1").insert_header("x-version", "1"),
        ResponseTemplate::new(304)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"v1\"")
            .insert_header("x-version", "2"),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let revalidated = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(revalidated.status(), StatusCode::OK);
    assert_eq!(cache_status(&revalidated), "REVALIDATED");
    assert_eq!(revalidated.headers().get("x-version").unwrap(), "2");
    assert_eq!(
        revalidated.headers().get("cache-control").unwrap(),
        "max-age=60"
    );
    assert_eq!(test::read_body(revalidated).await.as_ref(), b"old");
    assert_eq!(
        received_header(&origin, 1, "if-none-match")
            .await
            .as_deref(),
        Some("\"v1\"")
    );

    // the 304's max-age=60 made the entry fresh again
    let after = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&after), "HIT");
    assert_eq!(after.headers().get("x-version").unwrap(), "2");
    assert_eq!(test::read_body(after).await.as_ref(), b"old");
    assert_eq!(origin_requests(&origin).await, 2);

    let health = health(&app).await;
    assert_eq!(health["cache"]["revalidations"], 1);
}

#[actix_web::test]
async fn last_modified_is_revalidated_with_if_modified_since() {
    let last_modified = "Tue, 01 Sep 2026 10:00:00 GMT";
    let origin = MockServer::start().await;
    // `header` splits values on commas, which an HTTP date contains
    let if_modified_since = move |request: &wiremock::Request| {
        request
            .headers
            .get("if-modified-since")
            .map(|v| v.as_bytes())
            == Some(last_modified.as_bytes())
    };
    Mock::given(if_modified_since)
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=60"))
        .mount(&origin)
        .await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=1")
                .insert_header("last-modified", last_modified)
                .set_body_string("old"),
        )
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let revalidated = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(revalidated.status(), StatusCode::OK);
    assert_eq!(cache_status(&revalidated), "REVALIDATED");
    assert_eq!(test::read_body(revalidated).await.as_ref(), b"old");
    assert_eq!(received_header(&origin, 1, "if-none-match").await, None);
    let after = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&after), "HIT");
}

#[actix_web::test]
async fn full_response_to_revalidation_replaces_the_entry() {
    let origin = origin_answering(
        tagged("max-age=1"),
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"v2\"")
            .set_body_string("new"),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let replaced = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&replaced), "MISS");
    assert_eq!(test::read_body(replaced).await.as_ref(), b"new");
    assert_eq!(
        received_header(&origin, 1, "if-none-match")
            .await
            .as_deref(),
        Some("\"v1\"")
    );
    let after = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&after), "HIT");
    assert_eq!(test::read_body(after).await.as_ref(), b"new");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn client_conditional_passes_through_and_keeps_the_entry() {
    let origin = MockServer::start().await;
    Mock::given(header("if-none-match", "\"client\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"client\""))
        .mount(&origin)
        .await;
    Mock::given(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=60"))
        .mount(&origin)
        .await;
    Mock::given(any())
        .respond_with(tagged("max-age=1"))
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let conditional = get("/page").insert_header(("if-none-match", "\"client\""));
    let passed = test::call_service(&app, conditional.to_request()).await;

    assert_eq!(passed.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(cache_status(&passed), "MISS");
    assert_eq!(
        received_header(&origin, 1, "if-none-match")
            .await
            .as_deref(),
        Some("\"client\"")
    );

    // the client's 304 left the stored entry in place for revalidation
    let plain = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&plain), "REVALIDATED");
    assert_eq!(test::read_body(plain).await.as_ref(), b"old");
}

#[actix_web::test]
async fn stale_while_revalidate_serves_stale_and_refreshes_in_the_background() {
    let origin = origin_answering(
        tagged("max-age=1, stale-while-revalidate=10"),
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_string("new")
            .set_delay(Duration::from_millis(500)),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let started = Instant::now();
    let stale = test::call_service(&app, get("/page").to_request()).await;

    assert!(
        started.elapsed() < Duration::from_millis(400),
        "waited for the origin"
    );
    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(cache_status(&stale), "STALE");
    assert_eq!(test::read_body(stale).await.as_ref(), b"old");

    let refreshed = wait_for_hit(&app, "/page").await;
    assert_eq!(refreshed.as_ref(), b"new");
    assert_eq!(origin_requests(&origin).await, 2);
    assert_eq!(
        received_header(&origin, 1, "if-none-match")
            .await
            .as_deref(),
        Some("\"v1\"")
    );

    // polls before the refresh lands are stale serves too
    let health = health(&app).await;
    assert!(health["cache"]["stale"].as_u64().unwrap() >= 1);
    assert_eq!(health["cache"]["background_refreshes"], 1);
}

/// requests `path` until it is a hit, for up to three seconds, and returns
/// the hit's body.
async fn wait_for_hit<S, B>(app: &S, path: &str) -> bytes::Bytes
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        actix_web::rt::time::sleep(Duration::from_millis(100)).await;
        let resp = test::call_service(app, get(path).to_request()).await;
        let status = cache_status(&resp);
        let body = test::read_body(resp).await;
        if status == "HIT" {
            return body;
        }
        assert!(Instant::now() < deadline, "no hit, last status {status}");
    }
}

#[actix_web::test]
async fn concurrent_stale_requests_start_one_background_revalidation() {
    let origin = origin_answering(
        tagged("max-age=1, stale-while-revalidate=10"),
        ResponseTemplate::new(304)
            .insert_header("cache-control", "max-age=60")
            .set_delay(Duration::from_millis(500)),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let requests = (0..5).map(|_| test::call_service(&app, get("/page").to_request()));
    let responses = futures_util::future::join_all(requests).await;
    for resp in responses {
        assert_eq!(cache_status(&resp), "STALE");
        assert_eq!(test::read_body(resp).await.as_ref(), b"old");
    }

    assert_eq!(wait_for_hit(&app, "/page").await.as_ref(), b"old");
    assert_eq!(origin_requests(&origin).await, 2);
    let health = health(&app).await;
    assert!(health["cache"]["stale"].as_u64().unwrap() >= 5);
    assert_eq!(health["cache"]["background_refreshes"], 1);
    assert_eq!(health["cache"]["revalidations"], 1);
}

#[actix_web::test]
async fn stale_carries_the_right_age() {
    let origin = origin_answering(
        tagged("max-age=6, stale-while-revalidate=10").insert_header("age", "5"),
        ResponseTemplate::new(304).set_delay(Duration::from_secs(5)),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let stale = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&stale), "STALE");
    let age = age_of(&stale);
    assert!((6..=7).contains(&age), "age was {age}");
}

#[actix_web::test]
async fn stale_if_error_serves_stale_on_origin_errors() {
    for status in [500, 502, 503, 504] {
        let origin = origin_answering(
            tagged("max-age=1, stale-if-error=10"),
            ResponseTemplate::new(status),
        )
        .await;
        let (app, _assets) = common::service(&origin.uri()).await;

        test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
        actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
        let stale = test::call_service(&app, get("/page").to_request()).await;

        assert_eq!(stale.status(), StatusCode::OK, "origin {status}");
        assert_eq!(cache_status(&stale), "STALE", "origin {status}");
        assert_eq!(test::read_body(stale).await.as_ref(), b"old");
    }
}

#[actix_web::test]
async fn stale_if_error_serves_stale_on_timeout() {
    let origin = origin_answering(
        tagged("max-age=1, stale-if-error=10"),
        ResponseTemplate::new(200).set_delay(Duration::from_secs(3)),
    )
    .await;
    let (app, _assets) =
        common::service_with(&origin.uri(), |c| c.upstream_timeout_seconds = 1).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let stale = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(cache_status(&stale), "STALE");
    assert_eq!(test::read_body(stale).await.as_ref(), b"old");
}

#[actix_web::test]
async fn stale_if_error_serves_stale_on_connection_refused() {
    let origin = one_shot_origin(one_shot_response("max-age=1, stale-if-error=10"));
    let (app, _assets) = common::service(&origin).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let stale = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(cache_status(&stale), "STALE");
    assert_eq!(test::read_body(stale).await.as_ref(), b"old");
}

#[actix_web::test]
async fn origin_error_goes_through_outside_the_stale_if_error_window() {
    let origin = origin_answering(
        tagged("max-age=1, stale-if-error=1"),
        ResponseTemplate::new(503),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(Duration::from_millis(2200)).await;
    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cache_status(&resp), "MISS");
}

#[actix_web::test]
async fn must_revalidate_is_never_served_stale() {
    for directive in ["must-revalidate", "proxy-revalidate"] {
        let cache_control =
            format!("max-age=1, {directive}, stale-while-revalidate=10, stale-if-error=10");
        let origin = origin_answering(tagged(&cache_control), ResponseTemplate::new(503)).await;
        let (app, _assets) = common::service(&origin.uri()).await;

        test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
        actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
        let resp = test::call_service(&app, get("/page").to_request()).await;

        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{directive}"
        );
        assert_eq!(cache_status(&resp), "MISS", "{directive}");
        assert_eq!(origin_requests(&origin).await, 2, "{directive}");
    }
}

#[actix_web::test]
async fn must_revalidate_gets_504_when_the_origin_is_unreachable() {
    for directive in ["must-revalidate", "proxy-revalidate"] {
        let cache_control = format!("max-age=1, {directive}, stale-if-error=10");
        let origin = one_shot_origin(one_shot_response(&cache_control));
        let (app, _assets) = common::service(&origin).await;

        test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
        actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
        let resp = test::call_service(&app, get("/page").to_request()).await;

        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT, "{directive}");
    }
}

#[actix_web::test]
async fn entry_without_validator_or_stale_window_is_gone_after_expiry() {
    let origin = origin_answering(
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=1")
            .set_body_string("old"),
        ResponseTemplate::new(200).set_body_string("new"),
    )
    .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    assert_eq!(health(&app).await["cache"]["items"], 1);
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;

    assert_eq!(health(&app).await["cache"]["items"], 0);
    let resp = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&resp), "MISS");
    assert_eq!(test::read_body(resp).await.as_ref(), b"new");
}

#[actix_web::test]
async fn stale_entries_kept_for_revalidation_stay_within_the_byte_bound() {
    let origin = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=1, stale-while-revalidate=60")
                .insert_header("etag", "\"v1\"")
                .set_body_bytes(vec![b'x'; 300 * KIB]),
        )
        .mount(&origin)
        .await;
    let (app, _assets) = common::service_with(&origin.uri(), |c| c.cache_size_mb = 1).await;

    for path in ["/a", "/b", "/c", "/d", "/e"] {
        test::read_body(test::call_service(&app, get(path).to_request()).await).await;
    }
    actix_web::rt::time::sleep(PAST_ONE_SECOND).await;
    let health = health(&app).await;

    let items = health["cache"]["items"].as_u64().unwrap();
    assert!((1..=3).contains(&items), "{items} items");
    assert!(health["cache"]["bytes"].as_u64().unwrap() <= 1024 * KIB as u64);
}
