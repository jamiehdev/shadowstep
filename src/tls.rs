use crate::config::Config;
use crate::util::{Result, ShadowError};
use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// loads rustls server configuration from the certificate and key files
/// specified in the application config.
pub fn load_rustls_config(config: &Config) -> Result<Option<ServerConfig>> {
    // if either cert path or key path is missing, tls is disabled
    if !config.is_tls_enabled() {
        return Ok(None);
    }
    
    // both paths should be present at this point, but unwrap with custom error for clarity
    let cert_path = config.tls_cert_path.as_ref()
        .ok_or_else(|| ShadowError::TlsConfig("tls cert path is missing".to_string()))?;
    let key_path = config.tls_key_path.as_ref()
        .ok_or_else(|| ShadowError::TlsConfig("tls key path is missing".to_string()))?;
    
    // load certificate(s) from file
    let cert_file = File::open(cert_path)
        .map_err(|e| ShadowError::TlsConfig(format!("failed to open cert file: {}", e)))?;
    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain = certs(&mut cert_reader)
        .map_err(|e| ShadowError::TlsConfig(format!("failed to parse certs: {}", e)))?
        .into_iter()
        .map(Certificate)
        .collect();
    
    // load private key from file
    let key_file = File::open(key_path)
        .map_err(|e| ShadowError::TlsConfig(format!("failed to open key file: {}", e)))?;
    let mut key_reader = BufReader::new(key_file);
    let keys = pkcs8_private_keys(&mut key_reader)
        .map_err(|e| ShadowError::TlsConfig(format!("failed to parse private key: {}", e)))?;
    
    if keys.is_empty() {
        return Err(ShadowError::TlsConfig("no private keys found in key file".to_string()));
    }
    
    // use the first private key found
    let private_key = PrivateKey(keys[0].clone());
    
    // create server config with modern TLS settings
    let server_config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| ShadowError::TlsConfig(format!("tls config error: {}", e)))?;
    
    Ok(Some(server_config))
} 