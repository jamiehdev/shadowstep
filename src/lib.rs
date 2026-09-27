//! shadowstep - a minimal edge CDN implementation
//!
//! provides caching reverse proxy functionality with:
//! - HTTP/1.1 support
//! - in-memory caching
//! - TLS termination
//!
//! author: jamiehdev

pub mod config;
pub mod tls;

mod assets;
mod forwarded;
mod proxy;

use actix_web::body::MessageBody;
use actix_web::dev::{Server, ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::{Compress, Logger};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use hyper::client::HttpConnector;
use hyper::Client;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use log::info;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;

use crate::assets::AssetCache;
use crate::config::Config;

/// cache statistics tracker
pub struct CacheStats {
    hits: usize,
    misses: usize,
    items: usize,
}

/// application state, including cache
pub struct AppState {
    cache_stats: Mutex<CacheStats>,
    cache: AssetCache,
    http_client: Client<HttpsConnector<HttpConnector>>,
    upstream_base_url: Url,
    asset_path: PathBuf,
    upstream_timeout: Duration,
}

#[get("/health")]
async fn health_check(state: web::Data<AppState>) -> impl Responder {
    let stats = state.cache_stats.lock().unwrap();
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "cache": {
            "hits": stats.hits,
            "misses": stats.misses,
            "items": stats.items,
            "hit_ratio": if stats.hits + stats.misses > 0 {
                stats.hits as f32 / (stats.hits + stats.misses) as f32
            } else {
                0.0
            }
        }
    }))
}

/// builds the shared application state from `config`, creating the asset
/// directory if it does not exist.
pub fn build_state(config: &Config) -> io::Result<web::Data<AppState>> {
    std::fs::create_dir_all(&config.asset_path)?;

    // HTTPS connector with native-trust roots for TLS
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http1()
        .build();

    let http_client = Client::builder().build(https);

    let upstream_base_url = Url::parse(&config.origin_url).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Failed to parse ORIGIN_URL from config: {e}"),
        )
    })?;

    info!("Proxying requests to: {}", upstream_base_url);
    info!("Serving assets from: {:?}", config.asset_path);

    Ok(web::Data::new(AppState {
        cache_stats: Mutex::new(CacheStats {
            hits: 0,
            misses: 0,
            items: 0,
        }),
        cache: AssetCache::default(),
        http_client,
        upstream_base_url,
        asset_path: config.asset_path.clone(),
        upstream_timeout: Duration::from_secs(config.upstream_timeout_seconds),
    }))
}

/// the actix application: `/health`, `/assets/*` and a catch-all proxy route.
pub fn app(
    state: web::Data<AppState>,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(Compress::default())
        .wrap(Logger::new("%r %s %b %D ms"))
        .service(health_check)
        .service(assets::serve_asset)
        .route("/{path:.*}", web::to(proxy::forward_to_upstream))
}

/// starts the server on pre-bound listeners. the returned `Server` runs
/// once awaited or spawned.
pub fn run(
    state: web::Data<AppState>,
    http: TcpListener,
    tls: Option<(TcpListener, rustls::ServerConfig)>,
    workers: usize,
) -> io::Result<Server> {
    let mut server = HttpServer::new(move || app(state.clone()))
        .keep_alive(Duration::from_secs(75))
        .workers(workers)
        .listen(http)?;

    if let Some((listener, tls_config)) = tls {
        server = server.listen_rustls(listener, tls_config)?;
    }

    Ok(server.run())
}
