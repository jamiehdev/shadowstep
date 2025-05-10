use crate::cache::{CachedResponse, CdnCache};
use crate::config::Config;
use crate::fetcher::OriginFetcher;
use crate::tls::load_rustls_config;
use crate::util::{Result, ShadowError};

use actix_web::{
    dev::Payload,
    middleware::Logger, // provides access logging
    web,
    App,
    HttpRequest,
    HttpResponse,
    HttpServer,
    Responder,
};
use bytes::Bytes;
use http::{
    header::{HeaderName, HeaderValue},
    Version as HttpVersion,
};
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Instant;

struct AppState {
    fetcher: Arc<OriginFetcher>,
    cache: Arc<CdnCache>,
    // config: Arc<Config>, // config is cloned per-server thread by actix, or app_config can be used directly
}

async fn actix_to_hyper_request(
    actix_req: &HttpRequest,
    mut payload: Payload, // actix_web::dev::Payload
) -> Result<http::Request<hyper::Body>> {
    let mut hyper_req_builder = http::Request::builder()
        .method(actix_req.method().clone())
        .uri(actix_req.uri().to_string()) // actix_req.uri() provides the complete uri
        .version(match actix_req.version() {
            actix_web::http::Version::HTTP_09 => HttpVersion::HTTP_09,
            actix_web::http::Version::HTTP_10 => HttpVersion::HTTP_10,
            actix_web::http::Version::HTTP_11 => HttpVersion::HTTP_11,
            actix_web::http::Version::HTTP_2 => HttpVersion::HTTP_2,
            actix_web::http::Version::HTTP_3 => HttpVersion::HTTP_3,
            other => {
                warn!("unknown http version {:?}, defaulting to 1.1", other);
                HttpVersion::HTTP_11
            }
        });

    for (name, value) in actix_req.headers() {
        hyper_req_builder = hyper_req_builder.header(name, value.clone());
    }

    // actix_web::dev::Payload needs to be mapped to a stream of bytes for hyper::Body
    let body_stream = futures_util::stream::poll_fn(move |cx| payload.poll_next(cx));
    let hyper_body = hyper::Body::wrap_stream(body_stream);

    hyper_req_builder.body(hyper_body).map_err(ShadowError::Http)
}

fn hyper_to_actix_response(hyper_resp: http::Response<Bytes>) -> Result<HttpResponse> {
    let mut actix_resp_builder = HttpResponse::build(hyper_resp.status());
    for (name, value) in hyper_resp.headers() {
        // care should be taken with headers that actix might set automatically
        // or handle differently. for now, all headers are copied, but this
        // may require refinement
        actix_resp_builder.insert_header((name.clone(), value.clone()));
    }
    Ok(actix_resp_builder.body(hyper_resp.into_body()))
}

async fn forward_request(
    req: HttpRequest,
    payload: Payload, // actix_web::dev::Payload, not web::Payload
    app_state: web::Data<AppState>,
) -> impl Responder {
    let start_time = Instant::now();
    let cache_key = req.uri().to_string();

    // currently, only get requests are considered for caching
    if req.method() == actix_web::http::Method::GET {
        if let Some(cached_response) = app_state.cache.get(&cache_key).await {
            debug!("cache hit for: {}", cache_key);
            
            // build response from cached data, preserving status and headers
            let mut response_builder = HttpResponse::build(cached_response.status);
            
            // copy all original headers from the cached response
            for (name, value) in cached_response.headers.iter() {
                response_builder.insert_header((name, value));
            }
            
            // add cache hit indicator header
            response_builder.insert_header(("X-Shadowstep-Cache", "HIT"));
            
            info!(
                "{} {} -> {} {}ms (cached)",
                req.method(),
                req.uri(),
                cached_response.status,
                start_time.elapsed().as_millis()
            );
            
            return response_builder.body(cached_response.body.clone());
        }
        debug!("cache miss for: {}", cache_key);
    }

    let hyper_request = match actix_to_hyper_request(&req, payload).await {
        Ok(h_req) => h_req,
        Err(e) => {
            error!("failed to convert request: {}", e);
            return HttpResponse::InternalServerError()
                .body(format!("request conversion error: {}", e));
        }
    };

    match app_state.fetcher.fetch_from_origin(hyper_request).await {
        Ok(origin_response) => {
            let status = origin_response.status();
            let headers = origin_response.headers().clone(); // clone headers for caching
            let response_bytes = origin_response.body().clone(); // clone bytes for potential caching

            // cache successful get responses
            if req.method() == actix_web::http::Method::GET && status.is_success() {
                if !response_bytes.is_empty() {
                    debug!("caching response for: {}", cache_key);
                    app_state
                        .cache
                        .insert(cache_key.clone(), status, headers.clone(), response_bytes.clone())
                        .await;
                } else {
                    debug!("not caching empty response for: {}", cache_key);
                }
            }

            let mut actix_http_response = match hyper_to_actix_response(origin_response) {
                Ok(r) => r,
                Err(e) => {
                    error!("failed to convert origin response: {}", e);
                    return HttpResponse::InternalServerError()
                        .body(format!("response conversion error: {}", e));
                }
            };

            actix_http_response.headers_mut().insert(
                HeaderName::from_static("x-shadowstep-cache"),
                HeaderValue::from_static("MISS"),
            );

            info!(
                "{} {} -> {} {}ms",
                req.method(),
                req.uri(),
                status,
                start_time.elapsed().as_millis()
            );
            actix_http_response
        }
        Err(e) => {
            error!("failed to fetch from origin: {}", e);
            info!(
                "{} {} -> {} {}ms (error)",
                req.method(),
                req.uri(),
                "502 Bad Gateway",
                start_time.elapsed().as_millis()
            );
            HttpResponse::BadGateway().body(format!("origin fetch error: {}", e))
        }
    }
}

pub async fn run(config: Config) -> Result<()> {
    let app_config = Arc::new(config.clone()); // arc for sharing config across httpserver threads

    let fetcher = Arc::new(OriginFetcher::new(&app_config)?);
    let cache = Arc::new(CdnCache::new(
        app_config.cache_size_mb,
        app_config.cache_ttl_seconds,
    ));

    // appstate is constructed once and cloned by actix for each worker thread
    // when passed as web::Data::new(...)
    let app_state_data = web::Data::new(AppState {
        fetcher,
        cache,
        // config: app_config.clone(), // no longer storing full config directly in appstate
    });

    info!("shadowstep server starting on {}", app_config.listen_addr);
    if app_config.is_tls_enabled() {
        info!("tls is enabled.");
    } else {
        info!("tls is disabled (http only).");
    }

    let server_builder = HttpServer::new(move || {
        App::new()
            .app_data(app_state_data.clone()) // clones the web::Data<AppState> for this worker
            .wrap(Logger::default()) // default logger format: "%r" %s %b "%R" %Dms
            .default_service(web::to(forward_request))
    });

    let server = if app_config.is_tls_enabled() {
        let tls_rustls_config = load_rustls_config(&app_config)?.ok_or_else(|| {
            ShadowError::TlsConfig("tls enabled but rustls config failed to load".to_string())
        })?;
        server_builder
            .bind_rustls_0_22(&app_config.listen_addr, tls_rustls_config)
            .map_err(|e| ShadowError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
    } else {
        server_builder
            .bind(&app_config.listen_addr)
            .map_err(|e| ShadowError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
    };

    // handle graceful shutdown with signals
    let server = server.shutdown_timeout(30); // 30 second graceful shutdown period

    server.run().await.map_err(ShadowError::Io)
} 