//! shadowstep - a minimal edge CDN implementation
//!
//! provides caching reverse proxy functionality with:
//! - HTTP/1.1 support
//! - in-memory caching
//! - TLS termination
//!
//! author: jamiehdev
//! 

use actix_web::{get, web, App, HttpResponse, HttpServer, Responder, middleware::{Compress, Logger}};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use actix_web::http::header::{CACHE_CONTROL, ETAG};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::RwLock;
use std::sync::Arc;
use log::{info, warn};
use sha2::{Sha256, Digest};

use std::io::BufReader;
use mime_guess;

// Imports for Hyper client and URL parsing
use hyper::Client;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use url::Url;

mod config;
use config::Config;
mod proxy;

use std::fs::File;
use rustls::{Certificate, PrivateKey, ServerConfig as RustlsServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys};

// cache statistics tracker
struct CacheStats {
    hits: usize,
    misses: usize,
    items: usize,
}

// application state, including cache
struct AppState {
    cache_stats: Mutex<CacheStats>,
    cache: Arc<RwLock<HashMap<String, (Vec<u8>, String)>>>, 
    http_client: Client<HttpsConnector<hyper::client::HttpConnector>>,
    upstream_base_url: Url,
    asset_path: PathBuf,
}

fn load_rustls_config(cert_path: &std::path::Path, key_path: &std::path::Path) -> std::io::Result<RustlsServerConfig> {
    let cert_file = &mut BufReader::new(File::open(cert_path)?);
    let key_file = &mut BufReader::new(File::open(key_path)?);

    let cert_chain = certs(cert_file)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid cert"))?
        .into_iter()
        .map(Certificate)
        .collect();

    let mut keys = pkcs8_private_keys(key_file)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid key"))?
        .into_iter()
        .map(PrivateKey)
        .collect::<Vec<_>>();

    if keys.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "No private keys found"));
    }
    let config = RustlsServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys.remove(0))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    Ok(config)
}

/// resolves `requested` to a file inside `root`, or `None` if it is missing
/// or would escape the root. `requested` arrives percent-decoded, so
/// `..%2f` and `%2e%2e` reach this point as `../`.
async fn resolve_asset(root: &Path, requested: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for segment in requested.split('/') {
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if name == segment => relative.push(name),
            _ => return None,
        }
    }

    // canonicalising both sides catches symlinks that point outside the root
    let root = tokio::fs::canonicalize(root).await.ok()?;
    let resolved = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
    resolved.starts_with(&root).then_some(resolved)
}

#[get("/assets/{filename:.*}")]
async fn serve_asset(
    path: web::Path<String>,
    state: web::Data<AppState>,
    req: actix_web::HttpRequest,
) -> impl Responder {
    let filename = path.into_inner();

    let Some(path) = resolve_asset(&state.asset_path, &filename).await else {
        warn!("Asset not found: {}", filename);
        return HttpResponse::NotFound().body("not found");
    };

    let cache = state.cache.clone();
    
    // scoped read lock
    let cached_content = {
        let cache_read = cache.read().await;
        cache_read.get(&filename).cloned()
    };

    if let Some((content, etag)) = cached_content {
        // if the client sent an `if-none-match` header, check if it matches our etag.
        if let Some(if_none_match_hv) = req.headers().get("If-None-Match") {
            if let Ok(if_none_match_str) = if_none_match_hv.to_str() {
                if if_none_match_str == etag {
                    let mut stats = state.cache_stats.lock().unwrap();
                    stats.hits += 1;
                    return HttpResponse::NotModified().finish();
                }
            }
        }
        
        // cache hit but client needs content
        let mut stats = state.cache_stats.lock().unwrap();
        stats.hits += 1;
        
        // respond with cached content and appropriate headers.
        return HttpResponse::Ok()
            .append_header((ETAG, etag.clone()))
            .append_header((CACHE_CONTROL, "public, max-age=86400"))
            .append_header(("X-Shadowstep-Cache", "HIT"))
            
            // add debug print
            .append_header(("X-Debug", "Cache header was added HIT"))
            .content_type(mime_guess::from_path(&filename).first_or_octet_stream().as_ref())
            .body(content.clone());
    }
    
    // if not in cache, read from the filesystem.
    // debug print
    println!("Looking for file at: {:?}", path);
    
    match tokio::fs::read(&path).await {
        Ok(content) => {
            // generate an etag using a sha256 hash of the content.
            let mut hasher = Sha256::new();
            hasher.update(&content);
            let etag = format!("\"{}\"", hex::encode(hasher.finalize())[..32].to_string());
            
            // store the new asset in the cache.
            let mut cache_write = cache.write().await;
            cache_write.insert(filename.clone(), (content.clone(), etag.clone()));
            
            let mut stats = state.cache_stats.lock().unwrap();
            stats.misses += 1;
            stats.items = cache_write.len();
            drop(cache_write);
            
            info!("Cache miss for: {}", filename);
            
            HttpResponse::Ok()
                .append_header((ETAG, etag))
                .append_header((CACHE_CONTROL, "public, max-age=86400"))
                .append_header(("X-Shadowstep-Cache", "MISS"))
                .append_header(("X-Debug", "Cache header was added MISS"))
                .content_type(mime_guess::from_path(&filename).first_or_octet_stream().as_ref())
                .body(content)
        },
        Err(e) => {
            warn!("Asset not found: {} - Error: {}", filename, e);
            HttpResponse::NotFound().body("not found")
        },
    }
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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    
    let config = Config::load();

    std::fs::create_dir_all(&config.asset_path)
        .expect("failed to create configured assets directory");
    
    // create a new HTTPS connector with native-trust roots for TLS
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http1()
        .build();
    
    let http_client = Client::builder().build(https);

    let upstream_base_url = Url::parse(&config.origin_url)
        .expect("Failed to parse ORIGIN_URL from config");
    
    info!("Proxying requests to: {}", upstream_base_url);
    info!("Serving assets from: {:?}", config.asset_path);
    
    let num_workers = num_cpus::get();
    info!("shadowstep starting on {} with {} workers", config.listen_addr, num_workers);
    
    let app_state = web::Data::new(AppState {
        cache_stats: Mutex::new(CacheStats {
            hits: 0,
            misses: 0,
            items: 0,
        }),
        cache: Arc::new(RwLock::new(HashMap::new())),
        http_client, 
        upstream_base_url, 
        asset_path: config.asset_path.clone(), 
    });
    
    let mut server = HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(Compress::default())
            .wrap(Logger::new("%r %s %b %D ms"))
            .service(health_check)
            .service(serve_asset)
            .route("/{path:.*}", web::to(proxy::forward_to_upstream))
    })
    .keep_alive(Duration::from_secs(75))
    .workers(num_workers);

    // bind HTTP
    server = server.bind(&config.listen_addr)?;

    // bind HTTPS if TLS configured
    if let (Some(cert_path), Some(key_path)) = (config.tls_cert_path.as_ref(), config.tls_key_path.as_ref()) {
        let tls_config = load_rustls_config(cert_path, key_path)?;
        server = server.bind_rustls("0.0.0.0:8443", tls_config)?;
    }

    server.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECRET: &str = "secret contents outside the asset root";

    // layout: <tmp>/assets/app.css is servable, <tmp>/secret.txt must not be
    fn fixture() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shadowstep-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets").join("app.css"), "body{}").unwrap();
        std::fs::write(dir.join("secret.txt"), SECRET).unwrap();
        dir
    }

    fn state(asset_path: PathBuf) -> web::Data<AppState> {
        let https = HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_or_http()
            .enable_http1()
            .build();
        web::Data::new(AppState {
            cache_stats: Mutex::new(CacheStats {
                hits: 0,
                misses: 0,
                items: 0,
            }),
            cache: Arc::new(RwLock::new(HashMap::new())),
            http_client: Client::builder().build(https),
            upstream_base_url: Url::parse("http://127.0.0.1:1").unwrap(),
            asset_path,
        })
    }

    async fn get(uri: &str) -> (StatusCode, bytes::Bytes) {
        get_in(fixture(), uri).await
    }

    async fn get_in(dir: PathBuf, uri: &str) -> (StatusCode, bytes::Bytes) {
        let app = test::init_service(
            App::new()
                .app_data(state(dir.join("assets")))
                .service(serve_asset),
        )
        .await;
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let _ = std::fs::remove_dir_all(&dir);
        (status, body)
    }

    async fn assert_rejected(uri: &str) {
        assert_rejected_in(fixture(), uri).await;
    }

    async fn assert_rejected_in(dir: PathBuf, uri: &str) {
        let (status, body) = get_in(dir, uri).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{uri} returned {status}"
        );
        assert_ne!(body.as_ref(), SECRET.as_bytes(), "{uri} leaked the secret");
    }

    #[actix_web::test]
    async fn serves_asset_inside_root() {
        let (status, body) = get("/assets/app.css").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"body{}");
    }

    #[actix_web::test]
    async fn missing_asset_is_not_found() {
        let (status, _) = get("/assets/missing.css").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn rejects_literal_dot_dot() {
        assert_rejected("/assets/../secret.txt").await;
    }

    #[actix_web::test]
    async fn rejects_encoded_slash_dot_dot() {
        assert_rejected("/assets/..%2fsecret.txt").await;
    }

    #[actix_web::test]
    async fn rejects_encoded_dots() {
        assert_rejected("/assets/%2e%2e/secret.txt").await;
    }

    #[actix_web::test]
    async fn rejects_absolute_path() {
        let dir = fixture();
        let secret = dir.join("secret.txt");
        let uri = format!("/assets/{}", secret.display());
        assert_rejected_in(dir, &uri).await;
    }

    #[cfg(unix)]
    #[actix_web::test]
    async fn rejects_symlink_out_of_root() {
        let dir = fixture();
        std::os::unix::fs::symlink(dir.join("secret.txt"), dir.join("assets").join("link.txt"))
            .unwrap();
        assert_rejected_in(dir, "/assets/link.txt").await;
    }
}
