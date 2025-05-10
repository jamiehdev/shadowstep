use crate::config::Config;
use crate::util::{Result, ShadowError};
use bytes::Bytes;
use http::{Request, Response, StatusCode, Uri};
use hyper::client::HttpConnector;
use hyper::{body::to_bytes, Body, Client as HyperClient};
use std::sync::Arc;

#[derive(Clone)]
pub struct OriginFetcher {
    client: Arc<HyperClient<HttpConnector>>,
    origin_base_url: Arc<url::Url>,
}

impl OriginFetcher {
    pub fn new(config: &Config) -> Result<Self> {
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false); // this configuration allows both http and https schemes.
        
        let client = Arc::new(
            HyperClient::builder()
                .build(http_connector),
        );
        let origin_base_url = Arc::new(
            url::Url::parse(&config.origin_url)
                .map_err(ShadowError::UrlParse)?,
        );
        Ok(Self {
            client,
            origin_base_url,
        })
    }

    pub async fn fetch_from_origin(
        &self,
        mut req: Request<Body>, // represents the incoming request to the cdn.
    ) -> Result<Response<Bytes>> {
        // construct the target uri for forwarding the request to the origin server.
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");

        // resolve the request's path (which might be relative) against the base origin url.
        let target_url = self
            .origin_base_url
            .join(path_and_query) // url::join correctly handles relative paths and base urls.
            .map_err(ShadowError::UrlParse)?;
        
        let target_uri: Uri = target_url.as_str().parse().map_err(ShadowError::UriParse)?;

        // update the request's uri to the target origin; hyper will set the host header accordingly.
        *req.uri_mut() = target_uri;
        req.headers_mut().remove(http::header::HOST); // remove original host; hyper sets it from the uri.

        log::debug!("fetching from origin: {}", req.uri());

        let origin_response = self.client.request(req).await.map_err(ShadowError::Hyper)?;

        let (parts, body) = origin_response.into_parts();
        let body_bytes = to_bytes(body).await.map_err(ShadowError::Hyper)?;

        Ok(Response::from_parts(parts, body_bytes))
    }
} 
 