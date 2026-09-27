use actix_web::body::SizedStream;
use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::StreamExt;
use hyper::body::Body;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse, Uri};
use log::{debug, error, warn};
use std::convert::TryFrom;
use url::Position;

use crate::forwarded::{ClientInfo, CLIENT_FORWARDING_HEADERS};
use crate::AppState;

pub async fn forward_to_upstream(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
) -> HttpResponse {
    let client = ClientInfo::from_request(&req);

    debug!(
        "Incoming proxy request: {:?} {} from {:?}",
        req.method(),
        req.uri(),
        client.ip
    );

    let path_and_query = req.uri().path_and_query().map_or("", |pq| pq.as_str());
    // `Url` prints a bare host as `http://host/`, so trim the base path's
    // trailing slash before appending the request's own leading slash. slicing
    // at `AfterPath` also drops any query or fragment on the origin URL.
    let upstream_base = state.upstream_base_url[..Position::AfterPath].trim_end_matches('/');
    let target_url_str = format!("{}{}", upstream_base, path_and_query);

    let target_uri = match Uri::try_from(&target_url_str) {
        Ok(uri) => uri,
        Err(e) => {
            error!("Error constructing target URI '{}': {}", target_url_str, e);
            return HttpResponse::InternalServerError().body("internal server error");
        }
    };

    debug!("Forwarding request to: {}", target_uri);

    let mut hyper_req_builder = HyperRequest::builder()
        .method(req.method().clone())
        .uri(target_uri.clone());

    let options = connection_options(req.headers().get_all(header::CONNECTION));
    for (name, value) in req.headers().iter() {
        // the Host and forwarding headers are set below
        if name != header::HOST
            && !CLIENT_FORWARDING_HEADERS.contains(name)
            && is_end_to_end(name, &options)
        {
            hyper_req_builder = hyper_req_builder.header(name.clone(), value.clone());
        }
    }

    // set appropriate Host header for the upstream
    if let Some(host) = state.upstream_base_url.host_str() {
        let port_str =
            state
                .upstream_base_url
                .port_or_known_default()
                .map_or_else(String::new, |p| {
                    if p == 80 || p == 443 {
                        String::new()
                    } else {
                        format!(":{}", p)
                    }
                });
        let host_header_val = format!("{}{}", host, port_str);
        hyper_req_builder = hyper_req_builder.header(header::HOST, host_header_val);
    }

    if let Some(ip) = client.ip {
        hyper_req_builder = hyper_req_builder.header("X-Forwarded-For", ip.to_string());
    }
    hyper_req_builder = hyper_req_builder.header("X-Forwarded-Proto", client.scheme);
    hyper_req_builder = hyper_req_builder.header("X-Forwarded-Host", client.host);

    let body = match request_body(payload).await {
        Ok(body) => body,
        Err(e) => {
            warn!("Failed to read request body: {}", e);
            return HttpResponse::BadRequest().body("bad request");
        }
    };

    let hyper_req = match hyper_req_builder.body(body) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to build hyper request: {}", e);
            return HttpResponse::InternalServerError().body("internal server error");
        }
    };

    let upstream =
        tokio::time::timeout(state.upstream_timeout, state.http_client.request(hyper_req)).await;

    match upstream {
        Ok(Ok(upstream_response)) => {
            debug!(
                "Received response from upstream: {:?}",
                upstream_response.status()
            );
            client_response(upstream_response)
        }
        Ok(Err(e)) => {
            error!("Error forwarding request to upstream {}: {}", target_uri, e);
            HttpResponse::BadGateway().body("bad gateway")
        }
        Err(_) => {
            error!(
                "Upstream {} sent no response headers within {:?}",
                target_uri, state.upstream_timeout
            );
            HttpResponse::GatewayTimeout().body("gateway timeout")
        }
    }
}

/// the request body for the origin. actix's `Payload` is `!Send` and hyper
/// needs a `Send` body, so a task on this worker's local executor copies the
/// payload into a hyper body channel as chunks arrive.
async fn request_body(mut payload: web::Payload) -> Result<Body, actix_web::error::PayloadError> {
    // an empty body stays `Body::empty()` so that hyper does not send
    // `Transfer-Encoding: chunked` on a GET
    let first = match payload.next().await {
        None => return Ok(Body::empty()),
        Some(chunk) => chunk?,
    };

    let (mut sender, body) = Body::channel();
    actix_web::rt::spawn(async move {
        if sender.send_data(first).await.is_err() {
            return;
        }
        while let Some(chunk) = payload.next().await {
            match chunk {
                Ok(chunk) => {
                    // the origin request has ended, so the rest is unwanted
                    if sender.send_data(chunk).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    warn!("Failed to read request body: {}", e);
                    sender.abort();
                    return;
                }
            }
        }
    });
    Ok(body)
}

/// turns the origin's response into the client's, streaming the body. the
/// response carries the origin's length when it sent one.
fn client_response(upstream: HyperResponse<Body>) -> HttpResponse {
    let (parts, body) = upstream.into_parts();
    let mut builder = HttpResponse::build(parts.status);

    let options = connection_options(parts.headers.get_all(header::CONNECTION));
    for (name, value) in parts.headers.iter() {
        // actix writes Content-Length from the body size, so a copied header
        // would go stale when the Compress middleware re-encodes the body
        if name != header::CONTENT_LENGTH && is_end_to_end(name, &options) {
            builder.append_header((name.clone(), value.clone()));
        }
    }

    let body = body.map(|chunk| {
        chunk.map_err(|e| {
            error!("Error reading upstream response body: {}", e);
            e
        })
    });

    let length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    match length {
        Some(length) => builder.body(SizedStream::new(length, body)),
        None => builder.streaming(body),
    }
}

/// the lowercased header names that a `Connection` header lists, which are
/// hop-by-hop for this message (RFC 9110 section 7.6.1).
fn connection_options<'a>(values: impl IntoIterator<Item = &'a HeaderValue>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// whether a proxy forwards `name`. RFC 9110 section 7.6.1 names
/// `Connection`, `Keep-Alive`, `Proxy-Connection`, `TE`, `Transfer-Encoding`
/// and `Upgrade`. the proxy auth headers apply to one hop (section 11.7).
/// `Trailer` goes too, because the body stream drops trailer fields.
fn is_end_to_end(name: &HeaderName, connection_options: &[String]) -> bool {
    let name = name.as_str();
    let hop_by_hop = matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    );
    !hop_by_hop && !connection_options.iter().any(|option| option == name)
}
