use crate::common::{self, cache_status};

use actix_web::http::StatusCode;
use actix_web::test;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime};
use wiremock::matchers::{any, header};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// an origin that answers every request with `template`.
async fn origin_responding(template: ResponseTemplate) -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(any())
        .respond_with(template)
        .mount(&origin)
        .await;
    origin
}

/// a 200 with `cache_control` and a body naming it.
fn ok_with(cache_control: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("cache-control", cache_control)
        .set_body_string("cached body")
}

async fn origin_requests(origin: &MockServer) -> usize {
    origin.received_requests().await.unwrap().len()
}

/// sends two identical GETs built by `request` and returns their cache
/// statuses.
async fn twice(origin: &MockServer, request: impl Fn() -> test::TestRequest) -> (String, String) {
    let (app, _assets) = common::service(&origin.uri()).await;
    let first = test::call_service(&app, request().to_request()).await;
    let first_status = cache_status(&first);
    test::read_body(first).await;
    let second = test::call_service(&app, request().to_request()).await;
    let second_status = cache_status(&second);
    test::read_body(second).await;
    (first_status, second_status)
}

fn get(uri: &str) -> test::TestRequest {
    test::TestRequest::get().uri(uri)
}

/// asserts that the second of two GETs built by `request` reached the origin.
async fn assert_never_a_hit(template: ResponseTemplate, request: impl Fn() -> test::TestRequest) {
    let origin = origin_responding(template).await;
    let (first, second) = twice(&origin, request).await;
    assert_eq!((first.as_str(), second.as_str()), ("MISS", "MISS"));
    assert_eq!(origin_requests(&origin).await, 2);
}

/// asserts that the second of two GETs built by `request` came from the cache.
async fn assert_hit(template: ResponseTemplate, request: impl Fn() -> test::TestRequest) {
    let origin = origin_responding(template).await;
    let (first, second) = twice(&origin, request).await;
    assert_eq!((first.as_str(), second.as_str()), ("MISS", "HIT"));
    assert_eq!(origin_requests(&origin).await, 1);
}

#[actix_web::test]
async fn second_get_is_a_hit_and_origin_sees_one_request() {
    let origin = origin_responding(ok_with("max-age=60").insert_header("x-origin", "kept")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let first = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&first), "MISS");
    assert_eq!(test::read_body(first).await.as_ref(), b"cached body");

    let second = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(cache_status(&second), "HIT");
    assert_eq!(second.headers().get("x-origin").unwrap(), "kept");
    assert_eq!(second.headers().get("cache-control").unwrap(), "max-age=60");
    assert_eq!(test::read_body(second).await.as_ref(), b"cached body");

    assert_eq!(origin_requests(&origin).await, 1);
}

#[actix_web::test]
async fn hit_carries_age_including_the_origins_age() {
    let origin = origin_responding(ok_with("max-age=60").insert_header("age", "5")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let first = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(first.headers().get("age").unwrap(), "5");
    test::read_body(first).await;

    let second = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&second), "HIT");
    let age: u64 = second
        .headers()
        .get("age")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((5..=6).contains(&age), "age was {age}");
}

#[actix_web::test]
async fn hit_without_origin_age_has_age_zero() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let second = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&second), "HIT");
    assert_eq!(second.headers().get("age").unwrap(), "0");
}

#[actix_web::test]
async fn uncacheable_response_is_marked_miss() {
    let origin = origin_responding(ResponseTemplate::new(200)).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&resp), "MISS");
}

#[actix_web::test]
async fn response_without_explicit_freshness_is_not_cached() {
    assert_never_a_hit(ResponseTemplate::new(200).set_body_string("x"), || {
        get("/page")
    })
    .await;
}

#[actix_web::test]
async fn public_without_freshness_is_not_cached() {
    assert_never_a_hit(ok_with("public"), || get("/page")).await;
}

#[actix_web::test]
async fn no_store_response_is_never_a_hit() {
    assert_never_a_hit(ok_with("public, max-age=60, no-store"), || get("/page")).await;
}

#[actix_web::test]
async fn private_response_is_never_a_hit() {
    assert_never_a_hit(ok_with("private, max-age=60"), || get("/page")).await;
}

#[actix_web::test]
async fn qualified_private_response_is_never_a_hit() {
    assert_never_a_hit(ok_with("max-age=60, private=\"x-user\""), || get("/page")).await;
}

#[actix_web::test]
async fn no_cache_response_is_never_a_hit() {
    assert_never_a_hit(ok_with("max-age=60, no-cache"), || get("/page")).await;
}

#[actix_web::test]
async fn set_cookie_response_is_never_a_hit() {
    assert_never_a_hit(
        ok_with("max-age=60").insert_header("set-cookie", "session=abc"),
        || get("/page"),
    )
    .await;
}

#[actix_web::test]
async fn public_set_cookie_response_is_never_a_hit() {
    assert_never_a_hit(
        ok_with("public, max-age=60").insert_header("set-cookie", "session=abc"),
        || get("/page"),
    )
    .await;
}

#[actix_web::test]
async fn response_to_authorization_request_is_never_a_hit() {
    assert_never_a_hit(ok_with("max-age=60"), || {
        get("/page").insert_header(("authorization", "Bearer test"))
    })
    .await;
}

#[actix_web::test]
async fn s_maxage_response_to_authorization_request_is_never_a_hit() {
    assert_never_a_hit(ok_with("s-maxage=60, must-revalidate"), || {
        get("/page").insert_header(("authorization", "Bearer test"))
    })
    .await;
}

#[actix_web::test]
async fn response_to_cookie_request_is_never_a_hit() {
    assert_never_a_hit(ok_with("max-age=60"), || {
        get("/page").insert_header(("cookie", "session=abc"))
    })
    .await;
}

#[actix_web::test]
async fn public_response_to_authorization_request_is_a_hit() {
    assert_hit(ok_with("public, max-age=60"), || {
        get("/page").insert_header(("authorization", "Bearer test"))
    })
    .await;
}

#[actix_web::test]
async fn public_response_to_cookie_request_is_a_hit() {
    assert_hit(ok_with("public, max-age=60"), || {
        get("/page").insert_header(("cookie", "session=abc"))
    })
    .await;
}

#[actix_web::test]
async fn vary_accept_encoding_keys_separately() {
    let origin = MockServer::start().await;
    for encoding in ["gzip", "br"] {
        Mock::given(header("accept-encoding", encoding))
            .respond_with(
                ok_with("max-age=60")
                    .insert_header("vary", "Accept-Encoding")
                    .insert_header("x-variant", encoding),
            )
            .mount(&origin)
            .await;
    }
    let (app, _assets) = common::service(&origin.uri()).await;
    let request = |encoding: &str| {
        get("/page")
            .insert_header(("accept-encoding", encoding))
            .to_request()
    };

    let mut seen = Vec::new();
    for encoding in ["gzip", "br", "gzip", "br"] {
        let resp = test::call_service(&app, request(encoding)).await;
        let variant = resp
            .headers()
            .get("x-variant")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(variant, encoding, "served the wrong variant");
        seen.push(cache_status(&resp));
        test::read_body(resp).await;
    }

    assert_eq!(seen, ["MISS", "MISS", "HIT", "HIT"]);
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn vary_star_is_not_cached() {
    assert_never_a_hit(ok_with("max-age=60").insert_header("vary", "*"), || {
        get("/page")
    })
    .await;
}

#[actix_web::test]
async fn max_age_expires() {
    let origin = origin_responding(ok_with("max-age=1")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let fresh = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&fresh), "HIT");
    test::read_body(fresh).await;

    actix_web::rt::time::sleep(Duration::from_millis(1100)).await;
    let stale = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&stale), "MISS");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn s_maxage_beats_max_age() {
    assert_hit(ok_with("max-age=0, s-maxage=60"), || get("/page")).await;
}

#[actix_web::test]
async fn zero_s_maxage_beats_max_age() {
    assert_never_a_hit(ok_with("max-age=60, s-maxage=0"), || get("/page")).await;
}

#[actix_web::test]
async fn cache_ttl_seconds_caps_freshness() {
    let origin = origin_responding(ok_with("max-age=3600")).await;
    let (app, _assets) = common::service_with(&origin.uri(), |c| c.cache_ttl_seconds = 1).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let fresh = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&fresh), "HIT");
    test::read_body(fresh).await;

    actix_web::rt::time::sleep(Duration::from_millis(1100)).await;
    let capped = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&capped), "MISS");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn origin_age_uses_up_freshness() {
    assert_never_a_hit(ok_with("max-age=60").insert_header("age", "60"), || {
        get("/page")
    })
    .await;
}

#[actix_web::test]
async fn future_expires_is_a_hit() {
    let now = SystemTime::now();
    let template = ResponseTemplate::new(200)
        .insert_header("date", httpdate::fmt_http_date(now).as_str())
        .insert_header(
            "expires",
            httpdate::fmt_http_date(now + Duration::from_secs(60)).as_str(),
        );
    assert_hit(template, || get("/page")).await;
}

#[actix_web::test]
async fn past_expires_is_not_cached() {
    let now = SystemTime::now();
    let template = ResponseTemplate::new(200)
        .insert_header("date", httpdate::fmt_http_date(now).as_str())
        .insert_header(
            "expires",
            httpdate::fmt_http_date(now - Duration::from_secs(60)).as_str(),
        );
    assert_never_a_hit(template, || get("/page")).await;
}

#[actix_web::test]
async fn invalid_expires_is_not_cached() {
    assert_never_a_hit(
        ResponseTemplate::new(200).insert_header("expires", "0"),
        || get("/page"),
    )
    .await;
}

#[actix_web::test]
async fn max_age_beats_expires() {
    let now = SystemTime::now();
    let template = ok_with("max-age=0").insert_header(
        "expires",
        httpdate::fmt_http_date(now + Duration::from_secs(60)).as_str(),
    );
    assert_never_a_hit(template, || get("/page")).await;
}

#[actix_web::test]
async fn statuses_cacheable_by_default_are_hits() {
    for status in [200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501] {
        let template = ResponseTemplate::new(status).insert_header("cache-control", "max-age=60");
        let origin = origin_responding(template).await;
        let (first, second) = twice(&origin, || get("/page")).await;
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("MISS", "HIT"),
            "status {status}"
        );
        assert_eq!(origin_requests(&origin).await, 1, "status {status}");
    }
}

#[actix_web::test]
async fn other_statuses_are_not_cached() {
    for status in [201, 202, 206, 302, 307, 400, 403, 500, 502, 503] {
        let template = ResponseTemplate::new(status).insert_header("cache-control", "max-age=60");
        let origin = origin_responding(template).await;
        let (first, second) = twice(&origin, || get("/page")).await;
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("MISS", "MISS"),
            "status {status}"
        );
    }
}

#[actix_web::test]
async fn post_is_never_cached() {
    assert_never_a_hit(ok_with("public, max-age=60"), || {
        test::TestRequest::post()
            .uri("/page")
            .set_payload("name=test")
    })
    .await;
}

#[actix_web::test]
async fn post_invalidates_the_stored_get() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let post = test::TestRequest::post()
        .uri("/page")
        .set_payload("name=test");
    test::read_body(test::call_service(&app, post.to_request()).await).await;
    let after = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&after), "MISS");
    assert_eq!(origin_requests(&origin).await, 3);
}

#[actix_web::test]
async fn head_is_answered_from_a_stored_get() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let head = test::call_service(
        &app,
        test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri("/page")
            .to_request(),
    )
    .await;

    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(cache_status(&head), "HIT");
    let received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method.as_str(), "GET");
}

#[actix_web::test]
async fn head_response_is_not_stored_for_get() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let head = test::TestRequest::default()
        .method(actix_web::http::Method::HEAD)
        .uri("/page");
    test::read_body(test::call_service(&app, head.to_request()).await).await;
    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&resp), "MISS");
    assert_eq!(test::read_body(resp).await.as_ref(), b"cached body");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn request_no_cache_goes_to_origin_and_refreshes_the_entry() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let bypass = test::call_service(
        &app,
        get("/page")
            .insert_header(("cache-control", "no-cache"))
            .to_request(),
    )
    .await;
    assert_eq!(cache_status(&bypass), "MISS");
    test::read_body(bypass).await;
    let after = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&after), "HIT");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn request_max_age_zero_goes_to_origin() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    let resp = test::call_service(
        &app,
        get("/page")
            .insert_header(("cache-control", "max-age=0"))
            .to_request(),
    )
    .await;

    assert_eq!(cache_status(&resp), "MISS");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn request_no_store_is_neither_served_nor_stored() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;
    let no_store = || {
        get("/page")
            .insert_header(("cache-control", "no-store"))
            .to_request()
    };

    test::read_body(test::call_service(&app, no_store()).await).await;
    let plain = test::call_service(&app, get("/page").to_request()).await;
    assert_eq!(cache_status(&plain), "MISS");
    test::read_body(plain).await;
    let again = test::call_service(&app, no_store()).await;

    assert_eq!(cache_status(&again), "MISS");
    assert_eq!(origin_requests(&origin).await, 3);
}

#[actix_web::test]
async fn different_hosts_are_different_keys() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;
    let from = |host: &str| get("/page").insert_header(("host", host)).to_request();

    let mut seen = Vec::new();
    for host in ["a.example", "b.example", "A.example"] {
        let resp = test::call_service(&app, from(host)).await;
        seen.push(cache_status(&resp));
        test::read_body(resp).await;
    }

    assert_eq!(seen, ["MISS", "MISS", "HIT"]);
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn different_queries_are_different_keys() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let mut seen = Vec::new();
    for uri in ["/page?a=1", "/page?a=2", "/page", "/page?a=1"] {
        let resp = test::call_service(&app, get(uri).to_request()).await;
        seen.push(cache_status(&resp));
        test::read_body(resp).await;
    }

    assert_eq!(seen, ["MISS", "MISS", "MISS", "HIT"]);
    assert_eq!(origin_requests(&origin).await, 3);
}

/// answers with a cacheable body that lists the `X-Forwarded-Host` values
/// the origin received, as an origin that builds links from them would.
struct EchoForwardedHost;

impl wiremock::Respond for EchoForwardedHost {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let hosts: Vec<_> = request
            .headers
            .get_all("x-forwarded-host")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        ok_with("max-age=60").set_body_string(hosts.join(","))
    }
}

#[actix_web::test]
async fn spoofed_forwarding_headers_do_not_poison_the_cache() {
    let origin = MockServer::start().await;
    Mock::given(any())
        .respond_with(EchoForwardedHost)
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let spoofed = get("/page")
        .insert_header(("host", "real.example"))
        .insert_header(("forwarded", "host=real.example"))
        .insert_header(("x-forwarded-host", "evil.example"));
    test::read_body(test::call_service(&app, spoofed.to_request()).await).await;
    let normal = get("/page").insert_header(("host", "real.example"));
    let body = test::read_body(test::call_service(&app, normal.to_request()).await).await;

    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("evil.example"), "served {body:?}");
    assert_eq!(body, "real.example");
}

#[actix_web::test]
async fn forwarding_headers_do_not_change_the_cache_key() {
    for spoofed in [
        ("x-forwarded-host", "other.example"),
        ("x-forwarded-proto", "https"),
        ("forwarded", "host=other.example;proto=https"),
    ] {
        let origin = origin_responding(ok_with("max-age=60")).await;
        let (app, _assets) = common::service(&origin.uri()).await;

        let plain = get("/page").insert_header(("host", "real.example"));
        test::read_body(test::call_service(&app, plain.to_request()).await).await;
        let with_header = get("/page")
            .insert_header(("host", "real.example"))
            .insert_header(spoofed);
        let resp = test::call_service(&app, with_header.to_request()).await;

        assert_eq!(cache_status(&resp), "HIT", "{spoofed:?} changed the key");
        assert_eq!(origin_requests(&origin).await, 1, "{spoofed:?}");
    }
}

#[actix_web::test]
async fn oversized_body_streams_through_uncached() {
    let body: Vec<u8> = (0..9 * MIB).map(|i| (i % 251) as u8).collect();
    let origin = origin_responding(
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_bytes(body.clone()),
    )
    .await;
    let server = common::spawn(&origin.uri());
    let client = hyper::Client::new();

    for _ in 0..2 {
        let resp = client
            .get(server.url("/large").parse().unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get("x-shadowstep-cache").unwrap(), "MISS");
        assert_eq!(resp.headers().get("content-length").unwrap(), "9437184");
        let received = hyper::body::to_bytes(resp.into_body()).await.unwrap();
        assert!(received == body, "client received a different body");
    }

    assert_eq!(origin_requests(&origin).await, 2);
    drop(client);
    server.stop().await;
}

/// an origin that answers each connection with a chunked `body` of unknown
/// length and closes it. the counter holds the number of requests served.
fn chunked_origin(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                line.clear();
            }
            let mut stream = stream;
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n",
            );
            for chunk in body.chunks(64 * KIB) {
                let _ = write!(stream, "{:x}\r\n", chunk.len());
                let _ = stream.write_all(chunk);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{addr}"), served)
}

#[actix_web::test]
async fn chunked_body_over_the_cap_streams_through_uncached() {
    // a 1 MiB cache caps entries at 1 MiB
    let body: Vec<u8> = (0..2 * MIB).map(|i| (i % 251) as u8).collect();
    let (origin_url, served) = chunked_origin(body.clone());
    let (app, _assets) = common::service_with(&origin_url, |c| c.cache_size_mb = 1).await;

    for _ in 0..2 {
        let resp = test::call_service(&app, get("/large").to_request()).await;
        assert_eq!(cache_status(&resp), "MISS");
        let received = test::read_body(resp).await;
        assert!(received == body, "client received a different body");
    }

    assert_eq!(served.load(Ordering::SeqCst), 2);
}

#[actix_web::test]
async fn chunked_body_under_the_cap_is_cached() {
    let body: Vec<u8> = (0..100 * KIB).map(|i| (i % 251) as u8).collect();
    let (origin_url, served) = chunked_origin(body.clone());
    let (app, _assets) = common::service(&origin_url).await;

    test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&resp), "HIT");
    assert!(test::read_body(resp).await == body, "cached body differs");
    assert_eq!(served.load(Ordering::SeqCst), 1);
}

#[actix_web::test]
async fn byte_bound_evicts() {
    let origin = origin_responding(
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_bytes(vec![b'x'; 300 * KIB]),
    )
    .await;
    let (app, _assets) = common::service_with(&origin.uri(), |c| c.cache_size_mb = 1).await;
    let paths = ["/a", "/b", "/c", "/d", "/e"];

    let mut hits = 0;
    for _round in 0..2 {
        for path in paths {
            let resp = test::call_service(&app, get(path).to_request()).await;
            if cache_status(&resp) == "HIT" {
                hits += 1;
            }
            test::read_body(resp).await;
        }
    }
    let health = test::call_service(&app, get("/health").to_request()).await;
    let health: serde_json::Value = test::read_body_json(health).await;

    // five 300 KiB bodies cannot all fit in 1 MiB, so at most three hit
    assert!(hits >= 1, "nothing was cached");
    assert!(hits <= 3, "{hits} hits");
    assert!(health["cache"]["bytes"].as_u64().unwrap() <= MIB as u64);
}

/// an origin that sends the head of a cacheable response and a first chunk,
/// then holds the rest of the body until the returned sender is used. it
/// accepts one connection.
fn cacheable_origin_holding_its_body() -> (String, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
            line.clear();
        }
        let mut stream = stream;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n")
            .unwrap();
        stream.flush().unwrap();
        let _ = released.recv();
        let _ = stream.write_all(b"4\r\nlast\r\n0\r\n\r\n");
    });
    (format!("http://{addr}"), release)
}

#[actix_web::test]
async fn cacheable_response_streams_and_is_then_served_from_cache() {
    let (origin_url, release) = cacheable_origin_holding_its_body();
    let (app, _assets) = common::service(&origin_url).await;

    let first = tokio::time::timeout(
        Duration::from_secs(2),
        test::call_service(&app, get("/stream").to_request()),
    )
    .await
    .expect("the proxy waited for the whole origin body before responding");
    assert_eq!(cache_status(&first), "MISS");
    release.send(()).unwrap();
    assert_eq!(test::read_body(first).await.as_ref(), b"firstlast");

    // the origin accepts one connection, so only a hit can succeed
    let second = test::call_service(&app, get("/stream").to_request()).await;
    assert_eq!(cache_status(&second), "HIT");
    assert_eq!(test::read_body(second).await.as_ref(), b"firstlast");
}

#[actix_web::test]
async fn health_counts_origin_hits_and_misses() {
    let origin = origin_responding(ok_with("max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    for _ in 0..3 {
        test::read_body(test::call_service(&app, get("/page").to_request()).await).await;
    }
    let health = test::call_service(&app, get("/health").to_request()).await;
    let health: serde_json::Value = test::read_body_json(health).await;

    assert_eq!(health["cache"]["hits"], 2);
    assert_eq!(health["cache"]["misses"], 1);
    assert_eq!(health["cache"]["items"], 1);
}

#[actix_web::test]
async fn edited_asset_is_served_fresh() {
    let origin = common::unreachable_origin();
    let (app, assets) = common::service(&origin).await;
    let file = assets.path().join("app.css");
    std::fs::write(&file, "body{color:red}").unwrap();

    let first = test::call_service(&app, get("/assets/app.css").to_request()).await;
    assert_eq!(cache_status(&first), "MISS");
    assert_eq!(test::read_body(first).await.as_ref(), b"body{color:red}");
    let second = test::call_service(&app, get("/assets/app.css").to_request()).await;
    assert_eq!(cache_status(&second), "HIT");
    test::read_body(second).await;

    // same length, so only the modified time shows the edit
    std::fs::write(&file, "body{color:tan}").unwrap();
    let modified = SystemTime::now() + Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let edited = test::call_service(&app, get("/assets/app.css").to_request()).await;

    assert_eq!(cache_status(&edited), "MISS");
    assert_eq!(test::read_body(edited).await.as_ref(), b"body{color:tan}");
}

#[actix_web::test]
async fn asset_response_has_no_debug_header() {
    let origin = common::unreachable_origin();
    let (app, assets) = common::service(&origin).await;
    std::fs::write(assets.path().join("app.css"), "body{}").unwrap();

    for expected in ["MISS", "HIT"] {
        let resp = test::call_service(&app, get("/assets/app.css").to_request()).await;
        assert_eq!(cache_status(&resp), expected);
        assert!(resp.headers().get("x-debug").is_none());
    }
}

#[actix_web::test]
async fn post_response_is_not_served_to_get() {
    let origin = origin_responding(ok_with("public, max-age=60")).await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let post = test::TestRequest::post()
        .uri("/page")
        .set_payload("name=test");
    test::read_body(test::call_service(&app, post.to_request()).await).await;
    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&resp), "MISS");
    assert_eq!(origin_requests(&origin).await, 2);
}

#[actix_web::test]
async fn head_response_without_length_is_not_stored_for_get() {
    let (origin_url, served) = chunked_origin(b"full body".to_vec());
    let (app, _assets) = common::service(&origin_url).await;

    let head = test::TestRequest::default()
        .method(actix_web::http::Method::HEAD)
        .uri("/page");
    test::read_body(test::call_service(&app, head.to_request()).await).await;
    let resp = test::call_service(&app, get("/page").to_request()).await;

    assert_eq!(cache_status(&resp), "MISS");
    assert_eq!(test::read_body(resp).await.as_ref(), b"full body");
    assert_eq!(served.load(Ordering::SeqCst), 2);
}
