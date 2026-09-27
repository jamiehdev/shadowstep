use crate::tls::spawn_tls;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Request, Response, Version};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::net::SocketAddr;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type H2Client<B> = Client<HttpsConnector<HttpConnector>, B>;

/// an HTTP/2-only client that trusts only `cert_der` and offers only `h2`
/// through ALPN, so a handshake that falls back to HTTP/1.1 fails.
fn h2_client<B>(cert_der: Vec<u8>) -> H2Client<B>
where
    B: hyper::body::Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(cert_der)).unwrap();
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http2()
        .build();
    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build(https)
}

fn url(addr: SocketAddr, path: &str) -> String {
    format!("https://localhost:{}{path}", addr.port())
}

async fn get(client: &H2Client<Empty<Bytes>>, addr: SocketAddr, path: &str) -> Response<Bytes> {
    let resp = client
        .request(Request::get(url(addr, path)).body(Empty::new()).unwrap())
        .await
        .expect("h2 request");
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    Response::from_parts(parts, body)
}

fn header<'a>(resp: &'a Response<Bytes>, name: &str) -> &'a str {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
}

#[actix_web::test]
async fn h2_get_is_proxied() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("x-origin", "test")
                .set_body_string("hello over h2"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let client = h2_client(cert_der);

    let resp = get(&client, https_addr, "/page").await;

    assert_eq!(resp.version(), Version::HTTP_2);
    assert_eq!(resp.status(), 201);
    assert_eq!(header(&resp, "x-origin"), "test");
    assert_eq!(resp.body().as_ref(), b"hello over h2");

    drop(client);
    handle.stop(true).await;
}

#[actix_web::test]
async fn h2_post_body_reaches_origin_intact() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submit"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let client = h2_client(cert_der);
    // larger than the 64 KiB initial h2 flow-control window, and not one
    // repeated byte, so a truncated or reordered copy does not compare equal
    let body: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();

    let resp = client
        .request(
            Request::post(url(https_addr, "/submit"))
                .body(Full::new(Bytes::from(body.clone())))
                .unwrap(),
        )
        .await
        .expect("h2 request");

    assert_eq!(resp.version(), Version::HTTP_2);
    assert_eq!(resp.status(), 200);
    drop(resp);
    let received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body.len(), body.len());
    assert!(received[0].body == body, "the origin body differs");

    drop(client);
    handle.stop(true).await;
}

#[actix_web::test]
async fn h2_cacheable_response_is_a_miss_then_a_hit() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cached"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=60")
                .set_body_string("cached body"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let client = h2_client(cert_der);

    let first = get(&client, https_addr, "/cached").await;
    let second = get(&client, https_addr, "/cached").await;

    for resp in [&first, &second] {
        assert_eq!(resp.version(), Version::HTTP_2);
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body().as_ref(), b"cached body");
    }
    assert_eq!(header(&first, "x-shadowstep-cache"), "MISS");
    assert_eq!(header(&second, "x-shadowstep-cache"), "HIT");
    assert_eq!(origin.received_requests().await.unwrap().len(), 1);

    drop(client);
    handle.stop(true).await;
}

#[actix_web::test]
async fn h2_request_reaches_origin_as_https_with_the_authority_as_host() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let client = h2_client(cert_der);

    // an HTTP/2 request carries its host only in `:authority`
    let resp = get(&client, https_addr, "/page").await;

    assert_eq!(resp.version(), Version::HTTP_2);
    assert_eq!(resp.status(), 200);
    let received = origin.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let values = |name: &str| -> Vec<String> {
        received[0]
            .headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(values("x-forwarded-proto"), ["https"]);
    assert_eq!(
        values("x-forwarded-host"),
        [format!("localhost:{}", https_addr.port())]
    );

    drop(client);
    handle.stop(true).await;
}

/// the h2 crate refuses to send a response that holds a connection-specific
/// field (RFC 9113 section 8.2.2), and a client treats one it receives as
/// malformed, so the proxy must remove them from both fresh and stored
/// responses. actix removes most of them itself, but not `TE` or the fields
/// that `Connection` lists.
#[actix_web::test]
async fn h2_responses_carry_no_connection_specific_fields() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=60")
                .insert_header("connection", "x-hop")
                .insert_header("x-hop", "1")
                .insert_header("keep-alive", "timeout=5")
                .insert_header("proxy-connection", "keep-alive")
                .insert_header("te", "gzip")
                .insert_header("upgrade", "h2c")
                .insert_header("x-end-to-end", "kept")
                .set_body_string("body"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let client = h2_client(cert_der);

    let fresh = get(&client, https_addr, "/page").await;
    let stored = get(&client, https_addr, "/page").await;

    assert_eq!(header(&fresh, "x-shadowstep-cache"), "MISS");
    assert_eq!(header(&stored, "x-shadowstep-cache"), "HIT");
    for resp in [&fresh, &stored] {
        assert_eq!(resp.version(), Version::HTTP_2);
        assert_eq!(resp.status(), 200);
        for name in [
            "connection",
            "x-hop",
            "keep-alive",
            "proxy-connection",
            "te",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(
                resp.headers().get(name).is_none(),
                "{name} reached the client"
            );
        }
        assert_eq!(header(resp, "x-end-to-end"), "kept");
        assert_eq!(resp.body().as_ref(), b"body");
    }

    drop(client);
    handle.stop(true).await;
}
