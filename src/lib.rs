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
mod cache;
mod coalesce;
mod forwarded;
mod metrics;
mod proxy;

use actix_web::body::MessageBody;
use actix_web::dev::{Server, ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::{Compress, Logger};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use log::info;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

use crate::cache::{FlightKey, Store};
use crate::coalesce::Flights;
use crate::config::Config;
use crate::metrics::Metrics;

/// application state, including cache
pub struct AppState {
    metrics: Metrics,
    /// whether `/metrics` is served on the proxy listeners
    metrics_on_proxy_listeners: bool,
    cache: Store,
    flights: Flights<FlightKey>,
    http_client: Client<HttpsConnector<HttpConnector>, proxy::UpstreamBody>,
    upstream_base_url: Url,
    asset_path: PathBuf,
    upstream_timeout: Duration,
}

#[get("/health")]
async fn health_check(state: web::Data<AppState>) -> impl Responder {
    let counts = state.metrics.cache_counts();
    // the share of responses whose body came from the cache
    let from_cache = counts.hits + counts.revalidations + counts.stale + counts.coalesced;
    let total = from_cache + counts.misses;
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "cache": {
            "hits": counts.hits,
            "misses": counts.misses,
            "revalidations": counts.revalidations,
            "stale": counts.stale,
            "coalesced": counts.coalesced,
            "background_refreshes": counts.background_refreshes,
            "items": state.cache.entry_count(),
            "bytes": state.cache.weighted_size(),
            "hit_ratio": if total > 0 {
                from_cache as f64 / total as f64
            } else {
                0.0
            }
        }
    }))
}

#[get("/metrics")]
async fn metrics_endpoint(state: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok()
        .content_type(metrics::CONTENT_TYPE)
        .body(state.metrics.encode(&state.cache))
}

/// builds the shared application state from `config`, creating the asset
/// directory if it does not exist.
pub fn build_state(config: &Config) -> io::Result<web::Data<AppState>> {
    std::fs::create_dir_all(&config.asset_path)?;

    let https = HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(Arc::new(rustls::crypto::ring::default_provider()))?
        .https_or_http()
        .enable_http1()
        .build();

    // the pool needs a timer to close connections after their idle timeout
    let http_client = Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .build(https);

    let upstream_base_url = Url::parse(&config.origin_url).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Failed to parse ORIGIN_URL from config: {e}"),
        )
    })?;

    info!("Proxying requests to: {}", upstream_base_url);
    info!("Serving assets from: {:?}", config.asset_path);

    Ok(web::Data::new(AppState {
        metrics: Metrics::new(),
        metrics_on_proxy_listeners: config.metrics_addr.is_none(),
        cache: Store::new(
            config.cache_size_mb,
            Duration::from_secs(config.cache_ttl_seconds),
        ),
        flights: Flights::default(),
        http_client,
        upstream_base_url,
        asset_path: config.asset_path.clone(),
        upstream_timeout: Duration::from_secs(config.upstream_timeout_seconds),
    }))
}

/// the actix application: `/health`, `/metrics` unless it has a listener of
/// its own, `/assets/*` and a catch-all proxy route.
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
    let metrics_here = state.metrics_on_proxy_listeners;
    App::new()
        .app_data(state)
        .wrap(Compress::default())
        .wrap(Logger::new("%r %s %b %D ms"))
        .service(health_check)
        .configure(|cfg| {
            if metrics_here {
                cfg.service(metrics_endpoint);
            }
        })
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
        server = server.listen_rustls_0_23(listener, tls_config)?;
    }

    Ok(server.run())
}

/// a server that answers `GET /metrics` on `listener` and 404 on every other
/// path, for `--metrics-addr`.
pub fn run_metrics(state: web::Data<AppState>, listener: TcpListener) -> io::Result<Server> {
    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .wrap(Compress::default())
            .service(metrics_endpoint)
    })
    .workers(1)
    .listen(listener)?;
    Ok(server.run())
}
