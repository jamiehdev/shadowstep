use crate::common;

use actix_web::http::StatusCode;
use actix_web::test;
use hyper::body::Bytes;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MIB: usize = 1024 * 1024;

/// a body of `len` bytes that is not one repeated byte, so a reordered or
/// truncated copy does not compare equal.
fn patterned_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn origin_accepting(verb: &str, route: &str) -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    origin
}

async fn only_request(origin: &MockServer) -> wiremock::Request {
    let mut received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    received.remove(0)
}

#[actix_web::test]
async fn post_body_reaches_origin() {
    let origin = origin_accepting("POST", "/submit").await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/submit")
            .insert_header(("content-type", "application/x-www-form-urlencoded"))
            .set_payload("name=Test+User&message=test")
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    assert_eq!(received.body, b"name=Test+User&message=test");
}

#[actix_web::test]
async fn put_body_reaches_origin() {
    let origin = origin_accepting("PUT", "/items/1").await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/items/1")
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"name":"test"}"#)
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    assert_eq!(received.body, br#"{"name":"test"}"#);
}

#[actix_web::test]
async fn large_post_body_reaches_origin_with_its_length() {
    let origin = origin_accepting("POST", "/upload").await;
    let server = common::spawn(&origin.uri());
    let client = hyper::Client::new();
    let body = patterned_body(MIB);

    let resp = client
        .request(
            hyper::Request::post(server.url("/upload"))
                .body(hyper::Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    assert_eq!(received.headers.get("content-length").unwrap(), "1048576");
    assert!(received.body == body, "origin received a different body");
    drop(client);
    server.stop().await;
}

#[actix_web::test]
async fn chunked_post_body_reaches_origin() {
    let origin = origin_accepting("POST", "/upload").await;
    let server = common::spawn(&origin.uri());
    let client = hyper::Client::new();
    let body = patterned_body(MIB);
    let (mut sender, request_body) = hyper::Body::channel();
    let chunks: Vec<Bytes> = body.chunks(64 * 1024).map(Bytes::copy_from_slice).collect();
    actix_web::rt::spawn(async move {
        for chunk in chunks {
            sender.send_data(chunk).await.unwrap();
        }
    });

    let resp = client
        .request(
            hyper::Request::post(server.url("/upload"))
                .body(request_body)
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    assert!(received.body == body, "origin received a different body");
    drop(client);
    server.stop().await;
}

#[actix_web::test]
async fn slow_origin_is_gateway_timeout() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&origin)
        .await;
    let (app, _assets) =
        common::service_with(&origin.uri(), |c| c.upstream_timeout_seconds = 1).await;

    let started = Instant::now();
    let resp = test::call_service(&app, test::TestRequest::get().uri("/slow").to_request()).await;

    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

#[actix_web::test]
async fn hop_by_hop_request_headers_do_not_reach_origin() {
    let origin = origin_accepting("GET", "/page").await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/page")
            .insert_header(("connection", "keep-alive, X-Foo"))
            .insert_header(("x-foo", "1"))
            .insert_header(("keep-alive", "timeout=5"))
            .insert_header(("proxy-connection", "keep-alive"))
            .insert_header(("te", "trailers"))
            .insert_header(("trailer", "x-checksum"))
            .insert_header(("upgrade", "websocket"))
            .insert_header(("proxy-authorization", "Basic dGVzdDp0ZXN0"))
            .insert_header(("x-end-to-end", "kept"))
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    for name in [
        "connection",
        "x-foo",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "transfer-encoding",
    ] {
        assert!(
            received.headers.get(name).is_none(),
            "{name} reached the origin"
        );
    }
    assert_eq!(received.headers.get("x-end-to-end").unwrap(), "kept");
}

#[actix_web::test]
async fn hop_by_hop_response_headers_do_not_reach_client() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("connection", "X-Bar")
                .insert_header("x-bar", "1")
                .insert_header("keep-alive", "timeout=5")
                .insert_header("proxy-connection", "keep-alive")
                .insert_header("proxy-authenticate", "Basic")
                .insert_header("trailer", "x-checksum")
                .insert_header("upgrade", "h2c")
                .insert_header("x-end-to-end", "kept")
                .set_body_string("body"),
        )
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(&app, test::TestRequest::get().uri("/page").to_request()).await;

    assert_eq!(resp.status(), StatusCode::OK);
    for name in [
        "connection",
        "x-bar",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "trailer",
        "upgrade",
    ] {
        assert!(
            resp.headers().get(name).is_none(),
            "{name} reached the client"
        );
    }
    assert_eq!(resp.headers().get("x-end-to-end").unwrap(), "kept");
    assert_eq!(test::read_body(resp).await.as_ref(), b"body");
}

#[actix_web::test]
async fn bad_gateway_body_does_not_reveal_the_origin() {
    let origin_url = common::unreachable_origin();
    let origin_addr = origin_url.trim_start_matches("http://").to_owned();
    let (app, _assets) = common::service(&origin_url).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/anything").to_request()).await;

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = test::read_body(resp).await;
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains(&origin_addr), "body was {body:?}");
    assert!(
        !body.to_lowercase().contains("connect"),
        "body was {body:?}"
    );
}

#[actix_web::test]
async fn large_response_arrives_intact() {
    let body = patterned_body(4 * MIB);
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&origin)
        .await;
    let server = common::spawn(&origin.uri());
    let client = hyper::Client::new();

    let resp = client
        .get(server.url("/large").parse().unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-length").unwrap(), "4194304");
    let received = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    assert!(received == body, "client received a different body");
    drop(client);
    server.stop().await;
}

/// an origin that sends the response head and a first chunk, then holds the
/// rest of the body until the returned sender is used or dropped.
fn origin_holding_its_body() -> (String, mpsc::Sender<()>) {
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
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n")
            .unwrap();
        stream.flush().unwrap();
        let _ = released.recv();
        let _ = stream.write_all(b"4\r\nlast\r\n0\r\n\r\n");
    });
    (format!("http://{addr}"), release)
}

#[actix_web::test]
async fn response_starts_before_the_origin_finishes() {
    let (origin_url, release) = origin_holding_its_body();
    let (app, _assets) = common::service(&origin_url).await;

    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        test::call_service(&app, test::TestRequest::get().uri("/stream").to_request()),
    )
    .await
    .expect("the proxy waited for the whole origin body before responding");

    assert_eq!(resp.status(), StatusCode::OK);
    release.send(()).unwrap();
    assert_eq!(test::read_body(resp).await.as_ref(), b"firstlast");
}

#[actix_web::test]
async fn url_override_headers_do_not_reach_origin() {
    let origin = origin_accepting("GET", "/page").await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/page")
            .insert_header(("x-original-url", "/admin"))
            .insert_header(("x-rewrite-url", "/admin"))
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = only_request(&origin).await;
    for name in ["x-original-url", "x-rewrite-url"] {
        assert!(
            received.headers.get(name).is_none(),
            "{name} reached the origin"
        );
    }
}
