use actix_web::{
    web,
    Error,
    HttpRequest,
    HttpResponse,
    FromRequest,
};
use hyper::{
    body::Body,
    header::{self, HeaderValue},
    Request as HyperRequest,
    Uri,
};
use log::{debug, error};
use std::convert::TryFrom;

use crate::AppState; 

pub async fn forward_to_upstream(
    req: HttpRequest,
    payload: web::Payload, 
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("unknown")
        .to_string();

    debug!(
        "Incoming proxy request: {:?} {} from {}",
        req.method(),
        req.uri(),
        client_ip
    );

    let path_and_query = req.uri().path_and_query().map_or("", |pq| pq.as_str());
    let target_url_str = format!("{}{}", state.upstream_base_url, path_and_query);

    let target_uri = match Uri::try_from(&target_url_str) {
        Ok(uri) => uri,
        Err(e) => {
            error!(
                "Error constructing target URI '{}': {}",
                target_url_str,
                e
            );
            return Ok(HttpResponse::InternalServerError()
                .body(format!("Invalid upstream URL configuration: {}", e)));
        }
    };

    debug!("Forwarding request to: {}", target_uri);

    let mut hyper_req_builder = HyperRequest::builder()
        .method(req.method().clone())
        .uri(target_uri.clone());

    // copy headers from the original request to the new HyperRequest
    // filter out connection-specific headers or headers that might cause issues
    for (name, value) in req.headers().iter() {
        // hop-by-hop headers that should not be blindly forwarded
        match name {
            &header::CONNECTION |
            &header::PROXY_AUTHENTICATE |
            &header::PROXY_AUTHORIZATION |
            &header::TE |
            &header::TRAILER |
            &header::TRANSFER_ENCODING |
            &header::UPGRADE |
            &header::HOST => { /* Do not copy HOST, set it based on upstream_base_url */ }
            _ => {
                hyper_req_builder = hyper_req_builder.header(name.clone(), value.clone());
            }
        }
    }

    // set appropriate Host header for the upstream
    if let Some(host) = state.upstream_base_url.host_str() {
        let port_str = state
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

    // add X-Forwarded-* headers
    hyper_req_builder = hyper_req_builder.header("X-Forwarded-For", client_ip.clone());
    hyper_req_builder = hyper_req_builder.header("X-Forwarded-Proto", req.connection_info().scheme());
    hyper_req_builder = hyper_req_builder.header("X-Forwarded-Host", req.connection_info().host());

    // read the entire body from actix payload and then use it to build hyper request
    // this addresses the thread safety issue with actix_web::Payload
    let body_bytes = match web::Bytes::from_request(&req, &mut actix_web::dev::Payload::None).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(HttpResponse::InternalServerError().body("Failed to read request body."));
        }
    };
    
    let hyper_req = match hyper_req_builder.body(Body::from(body_bytes)) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to build hyper request: {}", e);
            return Ok(HttpResponse::InternalServerError().body("Failed to construct request for upstream server."));
        }
    };

    // send the request to the upstream server
    match state.http_client.request(hyper_req).await {
        Ok(upstream_response) => {
            debug!(
                "Received response from upstream: {:?}",
                upstream_response.status()
            );
            let mut client_resp_builder = HttpResponse::build(upstream_response.status());

            // copy headers from the upstream response to the client response
            for (name, value) in upstream_response.headers().iter() {
                // avoid copying hop-by-hop headers from response too
                match name {
                    &header::CONNECTION |
                    &header::PROXY_AUTHENTICATE |
                    &header::PROXY_AUTHORIZATION |
                    &header::TE |
                    &header::TRAILER |
                    &header::TRANSFER_ENCODING |
                    &header::UPGRADE => {}
                    _ => {
                        client_resp_builder.append_header((name.clone(), value.clone()));
                    }
                }
            }

            // convert the hyper response body to an actix-web response body
            let body_bytes = hyper::body::to_bytes(upstream_response.into_body()).await
                .map_err(|e| {
                    error!("Error reading upstream response body: {}", e);
                    actix_web::error::ErrorInternalServerError(format!("Failed to read upstream response: {}", e))
                })?;
            
            Ok(client_resp_builder.body(body_bytes))
        }
        Err(e) => {
            error!("Error forwarding request to upstream {}: {}", target_uri, e);
            let error_message = if e.is_connect() {
                format!(
                    "Failed to connect to upstream server at {}: {}",
                    state.upstream_base_url,
                    e
                )
            } else if e.is_timeout() {
                format!(
                    "Request to upstream server at {} timed out: {}",
                    state.upstream_base_url,
                    e
                )
            } else {
                format!(
                    "Error communicating with upstream server at {}: {}",
                    state.upstream_base_url,
                    e
                )
            };
            Ok(HttpResponse::BadGateway().body(error_message))
        }
    }
}
