use rustls::crypto::ring;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::fs::File;
use std::io::{self, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;

/// binds the HTTP listener and, when both tls paths are set, the HTTPS
/// listener with its rustls config.
pub fn bind_listeners(
    config: &Config,
) -> io::Result<(TcpListener, Option<(TcpListener, ServerConfig)>)> {
    let http = TcpListener::bind(&config.listen_addr)?;

    let tls = match (config.tls_cert_path.as_ref(), config.tls_key_path.as_ref()) {
        (Some(cert_path), Some(key_path)) => {
            let tls_config = load_rustls_config(cert_path, key_path)?;
            Some((TcpListener::bind(&config.tls_listen_addr)?, tls_config))
        }
        _ => None,
    };

    Ok((http, tls))
}

/// loads a rustls server config from a PEM certificate chain and a PKCS#8
/// private key. only the first key in the key file is used.
pub fn load_rustls_config(cert_path: &Path, key_path: &Path) -> io::Result<ServerConfig> {
    let cert_chain = CertificateDer::pem_reader_iter(BufReader::new(File::open(cert_path)?))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_input("invalid cert"))?;

    let key =
        match PrivatePkcs8KeyDer::pem_reader_iter(BufReader::new(File::open(key_path)?)).next() {
            Some(Ok(key)) => PrivateKeyDer::from(key),
            Some(Err(_)) => return Err(invalid_input("invalid key")),
            None => return Err(invalid_input("No private keys found")),
        };

    // an explicit provider, because rustls panics in `ServerConfig::builder`
    // when the build enables more than one crypto provider feature
    ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid_input(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| invalid_input(e.to_string()))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
