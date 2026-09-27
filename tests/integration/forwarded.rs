//! shadowstep faces clients directly, so no forwarding header a client sends
//! may reach the origin or change the values the proxy sets. these tests use
//! `common::spawn` because the in-process test service has no peer address.

use crate::common;

use bytes::Bytes;
use http_body_util::Empty;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn origin() -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    origin
}

/// sends `GET /page` with `headers` over a real socket and returns the
/// request that reached the origin, plus the proxy's `host:port`.
async fn forwarded_request(headers: &[(&str, &str)]) -> (wiremock::Request, String) {
    let origin = origin().await;
    let server = common::spawn(&origin.uri());
    let client = common::client();

    let mut request = hyper::Request::get(server.url("/page"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let resp = client
        .request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    let mut received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let proxy_host = server.addr.to_string();
    drop(client);
    server.stop().await;
    (received.remove(0), proxy_host)
}

/// every value of `name` that reached the origin, in order.
fn values(request: &wiremock::Request, name: &str) -> Vec<String> {
    request
        .headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

#[actix_web::test]
async fn client_x_forwarded_for_is_replaced_by_the_peer_ip() {
    let (received, _) = forwarded_request(&[("x-forwarded-for", "1.2.3.4")]).await;

    assert_eq!(values(&received, "x-forwarded-for"), ["127.0.0.1"]);
}

#[actix_web::test]
async fn client_forwarded_header_is_dropped_and_ignored() {
    let (received, proxy_host) =
        forwarded_request(&[("forwarded", "for=1.2.3.4;host=evil.example;proto=https")]).await;

    assert!(
        received.headers.get("forwarded").is_none(),
        "forwarded reached the origin"
    );
    assert_eq!(values(&received, "x-forwarded-for"), ["127.0.0.1"]);
    assert_eq!(values(&received, "x-forwarded-host"), [proxy_host]);
    assert_eq!(values(&received, "x-forwarded-proto"), ["http"]);
}

#[actix_web::test]
async fn client_x_forwarded_host_and_proto_are_replaced() {
    let (received, proxy_host) = forwarded_request(&[
        ("x-forwarded-host", "evil.example"),
        ("x-forwarded-proto", "https"),
    ])
    .await;

    assert_eq!(values(&received, "x-forwarded-host"), [proxy_host]);
    assert_eq!(values(&received, "x-forwarded-proto"), ["http"]);
}

#[actix_web::test]
async fn client_x_real_ip_and_other_forwarding_headers_are_dropped() {
    let (received, _) = forwarded_request(&[
        ("x-real-ip", "1.2.3.4"),
        ("x-forwarded-port", "8443"),
        ("x-forwarded-server", "evil.example"),
    ])
    .await;

    for name in ["x-real-ip", "x-forwarded-port", "x-forwarded-server"] {
        assert!(
            received.headers.get(name).is_none(),
            "{name} reached the origin"
        );
    }
    assert_eq!(values(&received, "x-forwarded-for"), ["127.0.0.1"]);
}

#[actix_web::test]
async fn client_prefix_uri_host_and_scheme_claims_are_dropped() {
    let cases = [
        ("x-forwarded-prefix", "/evil"),
        ("x-forwarded-uri", "/admin"),
        ("x-host", "evil.example"),
        ("x-forwarded-scheme", "https"),
        ("x-original-host", "evil.example"),
        ("front-end-https", "on"),
    ];
    let mut leaked = Vec::new();
    for (name, value) in cases {
        let (received, _) = forwarded_request(&[(name, value)]).await;
        if received.headers.get(name).is_some() {
            leaked.push(name);
        }
    }

    assert!(leaked.is_empty(), "reached the origin: {leaked:?}");
}
