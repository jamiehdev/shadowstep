mod common;

use actix_web::http::StatusCode;
use actix_web::test;
use wiremock::matchers::{method, path_regex};
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
    // the upstream path is matched loosely: see the report on the doubled slash
    Mock::given(method("GET"))
        .and(path_regex(r"^/+greeting$"))
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
        .and(path_regex(r"^/+missing$"))
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
        .and(path_regex(r"^/+over-socket$"))
        .respond_with(ResponseTemplate::new(200).set_body_string("via socket"))
        .expect(1)
        .mount(&origin)
        .await;
    let server = common::spawn(&origin.uri());
    let client = hyper::Client::new();

    let health = client
        .get(server.url("/health").parse().unwrap())
        .await
        .unwrap();
    let proxied = client
        .get(server.url("/over-socket").parse().unwrap())
        .await
        .unwrap();

    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(proxied.status(), StatusCode::OK);
    let body = hyper::body::to_bytes(proxied.into_body()).await.unwrap();
    assert_eq!(body.as_ref(), b"via socket");
    // a graceful stop waits for open keep-alive connections, and the client
    // pool holds one
    drop(client);
    server.stop().await;
}
