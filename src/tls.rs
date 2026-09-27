use rustls::crypto::ring;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

/// loads a rustls server config from a PEM certificate chain and a PEM
/// private key in PKCS#8, PKCS#1 (RSA) or SEC1 (EC) form. only the first key
/// in the key file is used.
pub fn load_rustls_config(cert_path: &Path, key_path: &Path) -> io::Result<ServerConfig> {
    let cert_chain = CertificateDer::pem_reader_iter(BufReader::new(File::open(cert_path)?))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_input("invalid cert"))?;

    let key = match PrivateKeyDer::from_pem_reader(BufReader::new(File::open(key_path)?)) {
        Ok(key) => key,
        Err(pem::Error::NoItemsFound) => return Err(invalid_input("No private keys found")),
        Err(_) => return Err(invalid_input("invalid key")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// a test-only PEM file under `tests/fixtures/tls`, made with openssl.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls")
            .join(name)
    }

    #[test]
    fn loads_pkcs1_rsa_key() {
        load_rustls_config(&fixture("rsa-cert.pem"), &fixture("rsa-pkcs1-key.pem")).unwrap();
    }

    #[test]
    fn loads_sec1_ec_key() {
        load_rustls_config(&fixture("ec-cert.pem"), &fixture("ec-sec1-key.pem")).unwrap();
    }

    #[test]
    fn uses_only_the_first_key_in_the_key_file() {
        let read = |name| std::fs::read_to_string(fixture(name)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ec_first = dir.path().join("ec-first.pem");
        let rsa_first = dir.path().join("rsa-first.pem");
        let (ec, rsa) = (read("ec-sec1-key.pem"), read("rsa-pkcs1-key.pem"));
        std::fs::write(&ec_first, format!("{ec}{rsa}")).unwrap();
        std::fs::write(&rsa_first, format!("{rsa}{ec}")).unwrap();

        load_rustls_config(&fixture("ec-cert.pem"), &ec_first).unwrap();
        // rustls rejects a key that does not match the certificate
        assert!(load_rustls_config(&fixture("ec-cert.pem"), &rsa_first).is_err());
    }
}
