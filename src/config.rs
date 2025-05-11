use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
pub struct Config {
    /// upstream
    #[clap(long, env = "ORIGIN_URL")]
    pub origin_url: String,

    #[clap(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen_addr: String,

    /// asset path for serving static files
    #[clap(long, env = "ASSET_PATH", default_value = "/app/assets")]
    pub asset_path: PathBuf,

    /// cache ttl
    #[clap(long, env = "CACHE_TTL_SECONDS", default_value_t = 300)]
    pub cache_ttl_seconds: u64,

    /// max cache size in mb
    #[clap(long, env = "CACHE_SIZE_MB", default_value_t = 100)]
    pub cache_size_mb: u64,

    /// tls cert path
    #[clap(long, env = "TLS_CERT_PATH", long = "tls-cert")]
    pub tls_cert_path: Option<PathBuf>,

    /// tls key path
    #[clap(long, env = "TLS_KEY_PATH", long = "tls-key")]
    pub tls_key_path: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Self {
        Config::parse()
    }

    pub fn is_tls_enabled(&self) -> bool {
        self.tls_cert_path.is_some() && self.tls_key_path.is_some()
    }
} 