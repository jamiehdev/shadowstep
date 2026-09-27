use crate::common;

use clap::Parser;
use shadowstep::config::Config;
use shadowstep::run;
use shadowstep::tls::bind_listeners;
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{Certificate, ClientConfig, RootCertStore, ServerName};
use tokio_rustls::TlsConnector;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// a self-signed certificate for `localhost`, written as PEM files into a
/// temporary directory. returns the directory and the certificate DER.
fn self_signed_cert() -> (TempDir, Vec<u8>) {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), key_pair.serialize_pem()).unwrap();
    (dir, cert.der().to_vec())
}

/// a proxy for `origin_url` listening on HTTP and HTTPS ports, both
/// ephemeral. returns the HTTPS and HTTP addresses, the server handle, the
/// certificate DER for the client to trust, and the directories to keep.
fn spawn_tls(
    origin_url: &str,
) -> (
    (SocketAddr, SocketAddr),
    actix_web::dev::ServerHandle,
    Vec<u8>,
    (TempDir, TempDir),
) {
    let (certs, cert_der) = self_signed_cert();
    let (state, assets) = common::state_with(origin_url, |_| {});
    let mut config = common::config(origin_url, certs.path());
    config.tls_cert_path = Some(certs.path().join("cert.pem"));
    config.tls_key_path = Some(certs.path().join("key.pem"));
    config.tls_listen_addr = "127.0.0.1:0".to_owned();

    let (http, tls) = bind_listeners(&config).unwrap();
    let (https, tls_config) = tls.expect("both tls paths are set");
    let https_addr = https.local_addr().unwrap();
    let http_addr = http.local_addr().unwrap();

    let server = run(state, http, Some((https, tls_config)), 1).unwrap();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    ((https_addr, http_addr), handle, cert_der, (certs, assets))
}

/// sends the raw HTTP/1.1 `request` over TLS to `addr`, trusting only
/// `cert_der` for `localhost`, and returns the whole response.
async fn tls_exchange(addr: SocketAddr, cert_der: Vec<u8>, request: &[u8]) -> String {
    let mut roots = RootCertStore::empty();
    roots.add(&Certificate(cert_der)).unwrap();
    let client_config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut stream = TlsConnector::from(Arc::new(client_config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("tls handshake");

    stream.write_all(request).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

#[actix_web::test]
async fn https_listener_binds_tls_listen_addr_and_completes_handshake() {
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&common::unreachable_origin());
    assert_eq!(https_addr.ip().to_string(), "127.0.0.1");
    assert_ne!(https_addr.port(), 8443);

    let response = tls_exchange(
        https_addr,
        cert_der,
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {response}"
    );

    handle.stop(true).await;
}

#[actix_web::test]
async fn https_request_reaches_origin_as_https_whatever_the_client_claims() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&origin)
        .await;
    let ((https_addr, _), handle, cert_der, _dirs) = spawn_tls(&origin.uri());

    let response = tls_exchange(
        https_addr,
        cert_der,
        b"GET /page HTTP/1.1\r\nHost: localhost\r\n\
          X-Forwarded-Proto: http\r\nX-Forwarded-Host: evil.example\r\n\
          Connection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {response}"
    );

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
    assert_eq!(values("x-forwarded-host"), ["localhost"]);
    assert_eq!(values("x-forwarded-for"), ["127.0.0.1"]);

    handle.stop(true).await;
}

/// answers with a cacheable body holding the `X-Forwarded-Proto` value the
/// origin received.
struct EchoForwardedProto;

impl wiremock::Respond for EchoForwardedProto {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let proto = request.headers.get("x-forwarded-proto").unwrap();
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_string(proto.to_str().unwrap())
    }
}

#[actix_web::test]
async fn https_and_http_responses_are_cached_apart() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(EchoForwardedProto)
        .mount(&origin)
        .await;
    let ((https_addr, http_addr), handle, cert_der, _dirs) = spawn_tls(&origin.uri());
    let request = b"GET /page HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    let first = tls_exchange(https_addr, cert_der.clone(), request).await;
    let client = hyper::Client::new();
    // the same Host, so that only the scheme tells the two requests apart
    let plain = client
        .request(
            hyper::Request::get(format!("http://{http_addr}/page"))
                .header("host", "localhost")
                .body(hyper::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let plain_status = plain.headers()["x-shadowstep-cache"].clone();
    let plain_body = hyper::body::to_bytes(plain.into_body()).await.unwrap();
    let second = tls_exchange(https_addr, cert_der, request).await;

    assert!(first.contains("x-shadowstep-cache: MISS"), "{first}");
    assert!(first.ends_with("https"), "{first}");
    assert_eq!(plain_status, "MISS");
    assert_eq!(plain_body.as_ref(), b"http");
    assert!(second.contains("x-shadowstep-cache: HIT"), "{second}");
    assert!(second.ends_with("https"), "{second}");
    assert_eq!(origin.received_requests().await.unwrap().len(), 2);

    drop(client);
    handle.stop(true).await;
}

#[test]
fn one_tls_path_binds_no_https_listener() {
    let (certs, _) = self_signed_cert();
    let mut config = common::config(&common::unreachable_origin(), certs.path());
    config.tls_listen_addr = "127.0.0.1:0".to_owned();

    config.tls_cert_path = Some(certs.path().join("cert.pem"));
    let (_http, tls) = bind_listeners(&config).unwrap();
    assert!(tls.is_none(), "cert path alone must not start https");

    config.tls_cert_path = None;
    config.tls_key_path = Some(certs.path().join("key.pem"));
    let (_http, tls) = bind_listeners(&config).unwrap();
    assert!(tls.is_none(), "key path alone must not start https");
}

#[test]
fn tls_listen_addr_defaults_to_8443_on_all_interfaces() {
    // fails spuriously if TLS_LISTEN_ADDR is set in the test environment
    let config =
        Config::try_parse_from(["shadowstep", "--origin-url", "http://127.0.0.1:1"]).unwrap();
    assert_eq!(config.tls_listen_addr, "0.0.0.0:8443");
}
