use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys};
use std::fs::File;
use std::io::{self, BufReader};
use std::net::TcpListener;
use std::path::Path;

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
    let cert_file = &mut BufReader::new(File::open(cert_path)?);
    let key_file = &mut BufReader::new(File::open(key_path)?);

    let cert_chain = certs(cert_file)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid cert"))?
        .into_iter()
        .map(Certificate)
        .collect();

    let mut keys = pkcs8_private_keys(key_file)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid key"))?
        .into_iter()
        .map(PrivateKey)
        .collect::<Vec<_>>();

    if keys.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "No private keys found",
        ));
    }
    ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys.remove(0))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}
