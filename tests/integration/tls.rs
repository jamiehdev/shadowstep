use crate::common;

use clap::Parser;
use shadowstep::config::Config;
use shadowstep::run;
use shadowstep::tls::bind_listeners;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{Certificate, ClientConfig, RootCertStore, ServerName};
use tokio_rustls::TlsConnector;

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

#[actix_web::test]
async fn https_listener_binds_tls_listen_addr_and_completes_handshake() {
    let (certs, cert_der) = self_signed_cert();
    let (state, _assets) = common::state_with(&common::unreachable_origin(), |_| {});
    let mut config = common::config(&common::unreachable_origin(), certs.path());
    config.tls_cert_path = Some(certs.path().join("cert.pem"));
    config.tls_key_path = Some(certs.path().join("key.pem"));
    config.tls_listen_addr = "127.0.0.1:0".to_owned();

    let (http, tls) = bind_listeners(&config).unwrap();
    let (https, tls_config) = tls.expect("both tls paths are set");
    let https_addr = https.local_addr().unwrap();
    assert_eq!(https_addr.ip().to_string(), "127.0.0.1");
    assert_ne!(https_addr.port(), 8443);

    let server = run(state, http, Some((https, tls_config)), 1).unwrap();
    let handle = server.handle();
    actix_web::rt::spawn(server);

    let mut roots = RootCertStore::empty();
    roots.add(&Certificate(cert_der)).unwrap();
    let client_config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(https_addr).await.unwrap();
    let mut stream = TlsConnector::from(Arc::new(client_config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("tls handshake");

    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {response}"
    );

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
