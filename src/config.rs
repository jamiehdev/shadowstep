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

    /// https listen address, used only when both tls paths are set
    #[clap(long, env = "TLS_LISTEN_ADDR", default_value = "0.0.0.0:8443")]
    pub tls_listen_addr: String,

    /// seconds to wait for the origin's response headers, including the time
    /// to send the request body. the proxy answers 504 when it runs out. it
    /// is also the longest the origin may send nothing during a response
    /// body before the proxy closes the client's connection.
    #[clap(long, env = "UPSTREAM_TIMEOUT_SECONDS", default_value_t = 30)]
    pub upstream_timeout_seconds: u64,

    /// address for a listener that serves only `/metrics`. when it is set,
    /// the proxy listeners forward `/metrics` to the origin like any other
    /// path. when it is unset, they serve `/metrics` as they serve `/health`.
    #[clap(long, env = "METRICS_ADDR")]
    pub metrics_addr: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        Config::parse()
    }

    pub fn is_tls_enabled(&self) -> bool {
        self.tls_cert_path.is_some() && self.tls_key_path.is_some()
    }
}
