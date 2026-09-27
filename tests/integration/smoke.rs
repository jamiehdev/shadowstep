use crate::common;

use actix_web::http::StatusCode;
use actix_web::test;
use bytes::Bytes;
use http_body_util::Empty;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[actix_web::test]
async fn health_returns_ok() {
    let (app, _assets) = common::service(&common::unreachable_origin()).await;

    let resp = test::call_service(&app, test::TestRequest::get().uri("/health").to_request()).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[actix_web::test]
async fn proxies_get_to_origin() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/greeting"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("x-origin-header", "from-origin")
                .set_body_string("hello from origin"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/greeting").to_request()).await;

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("x-origin-header").unwrap(),
        "from-origin"
    );
    assert_eq!(test::read_body(resp).await.as_ref(), b"hello from origin");
    origin.verify().await;
}

#[actix_web::test]
async fn origin_not_found_passes_through() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("origin says no"))
        .expect(1)
        .mount(&origin)
        .await;
    let (app, _assets) = common::service(&origin.uri()).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/missing").to_request()).await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(test::read_body(resp).await.as_ref(), b"origin says no");
}

#[actix_web::test]
async fn unreachable_origin_is_bad_gateway() {
    let (app, _assets) = common::service(&common::unreachable_origin()).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/anything").to_request()).await;

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[actix_web::test]
async fn serves_over_a_real_socket() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/over-socket"))
        .respond_with(ResponseTemplate::new(200).set_body_string("via socket"))
        .expect(1)
        .mount(&origin)
        .await;
    let server = common::spawn(&origin.uri());
    let client = common::client::<Empty<Bytes>>();

    let health = client
        .get(server.url("/health").parse().unwrap())
        .await
        .unwrap();
    let proxied = client
        .get(server.url("/over-socket").parse().unwrap())
        .await
        .unwrap();

    assert_eq!(health.status(), hyper::StatusCode::OK);
    assert_eq!(proxied.status(), hyper::StatusCode::OK);
    let body = common::body_bytes(proxied.into_body()).await;
    assert_eq!(body.as_ref(), b"via socket");
    // a graceful stop waits for open keep-alive connections, and the client
    // pool holds one
    drop(client);
    server.stop().await;
}

/// sends `GET request_path` through the proxy to an origin configured as
/// `origin.uri() + origin_suffix` and returns the path and query the origin
/// received.
async fn upstream_target(origin_suffix: &str, request_path: &str) -> (String, Option<String>) {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    let origin_url = format!("{}{}", origin.uri(), origin_suffix);
    let (app, _assets) = common::service(&origin_url).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri(request_path).to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let url = &received[0].url;
    (url.path().to_owned(), url.query().map(str::to_owned))
}

#[actix_web::test]
async fn origin_receives_the_request_path_exactly() {
    assert_eq!(
        upstream_target("", "/greeting").await,
        ("/greeting".to_owned(), None)
    );
}

#[actix_web::test]
async fn origin_with_trailing_slash_receives_the_request_path_exactly() {
    assert_eq!(
        upstream_target("/", "/greeting").await,
        ("/greeting".to_owned(), None)
    );
}

#[actix_web::test]
async fn query_string_is_preserved() {
    assert_eq!(
        upstream_target("", "/search?q=a&b=2").await,
        ("/search".to_owned(), Some("q=a&b=2".to_owned()))
    );
}

#[actix_web::test]
async fn origin_path_prefix_is_kept() {
    assert_eq!(
        upstream_target("/api", "/users?page=2").await,
        ("/api/users".to_owned(), Some("page=2".to_owned()))
    );
}

#[actix_web::test]
async fn origin_path_prefix_with_trailing_slash_is_kept() {
    assert_eq!(
        upstream_target("/api/", "/users").await,
        ("/api/users".to_owned(), None)
    );
}
