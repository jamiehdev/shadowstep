use actix_web::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use actix_web::{get, web, HttpRequest, HttpResponse, Responder};
use bytes::Bytes;
use log::{debug, warn};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::cache::Asset;
use crate::metrics::{self, Route};
use crate::AppState;

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
pub async fn serve_asset(
    path: web::Path<String>,
    state: web::Data<AppState>,
    req: HttpRequest,
) -> impl Responder {
    let response = asset_response(path.into_inner(), &state, &req).await;
    // a response without `X-Shadowstep-Cache` found no file, so the cache
    // was never consulted
    let cache = metrics::cache_label(&response).unwrap_or(metrics::BYPASS);
    state
        .metrics
        .observe(Route::Asset, cache, response.status());
    response
}

async fn asset_response(filename: String, state: &AppState, req: &HttpRequest) -> HttpResponse {
    let Some(path) = resolve_asset(&state.asset_path, &filename).await else {
        warn!("Asset not found: {}", filename);
        return HttpResponse::NotFound().body("not found");
    };

    let (asset, status) = match load(state, path).await {
        Ok(loaded) => loaded,
        Err(e) => {
            warn!("Asset not found: {} - Error: {}", filename, e);
            return HttpResponse::NotFound().body("not found");
        }
    };

    let not_modified = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == asset.etag);
    if not_modified {
        return HttpResponse::NotModified()
            .append_header((ETAG, asset.etag.clone()))
            .append_header(("X-Shadowstep-Cache", status))
            .finish();
    }

    HttpResponse::Ok()
        .append_header((ETAG, asset.etag.clone()))
        .append_header((CACHE_CONTROL, "public, max-age=86400"))
        .append_header(("X-Shadowstep-Cache", status))
        .content_type(
            mime_guess::from_path(&filename)
                .first_or_octet_stream()
                .as_ref(),
        )
        .body(asset.content.clone())
}

/// the asset at `path` and whether it came from the cache. a stored copy
/// counts only while the file's length and modified time are unchanged, so
/// an edited file is read again.
async fn load(state: &AppState, path: PathBuf) -> std::io::Result<(Arc<Asset>, &'static str)> {
    let metadata = tokio::fs::metadata(&path).await?;
    let modified = metadata.modified()?;

    if let Some(asset) = state.cache.asset(&path) {
        if asset.modified == modified && asset.len == metadata.len() {
            return Ok((asset, metrics::HIT));
        }
    }

    debug!("Reading asset from {:?}", path);
    let content = Bytes::from(tokio::fs::read(&path).await?);
    let etag = format!("\"{}\"", &hex::encode(Sha256::digest(&content))[..32]);
    Ok((
        state.cache.insert_asset(path, content, etag, modified),
        metrics::MISS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use actix_web::{http::StatusCode, test, App};
    use tempfile::TempDir;

    const SECRET: &str = "secret contents outside the asset root";

    // layout: <tmp>/assets/app.css is servable, <tmp>/secret.txt must not be
    fn fixture() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets").join("app.css"), "body{}").unwrap();
        std::fs::write(dir.path().join("secret.txt"), SECRET).unwrap();
        dir
    }

    fn state(asset_path: PathBuf) -> web::Data<AppState> {
        crate::build_state(&Config {
            origin_url: "http://127.0.0.1:1".into(),
            listen_addr: "127.0.0.1:0".into(),
            asset_path,
            cache_ttl_seconds: 300,
            cache_size_mb: 100,
            tls_cert_path: None,
            tls_key_path: None,
            tls_listen_addr: "127.0.0.1:0".into(),
            upstream_timeout_seconds: 30,
            metrics_addr: None,
        })
        .unwrap()
    }

    async fn get(uri: &str) -> (StatusCode, web::Bytes) {
        get_in(fixture(), uri).await
    }

    async fn get_in(dir: TempDir, uri: &str) -> (StatusCode, web::Bytes) {
        let app = test::init_service(
            App::new()
                .app_data(state(dir.path().join("assets")))
                .service(serve_asset),
        )
        .await;
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        (status, body)
    }

    async fn assert_rejected(uri: &str) {
        assert_rejected_in(fixture(), uri).await;
    }

    async fn assert_rejected_in(dir: TempDir, uri: &str) {
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
        let secret = dir.path().join("secret.txt");
        let uri = format!("/assets/{}", secret.display());
        assert_rejected_in(dir, &uri).await;
    }

    #[cfg(unix)]
    #[actix_web::test]
    async fn rejects_symlink_out_of_root() {
        let dir = fixture();
        std::os::unix::fs::symlink(
            dir.path().join("secret.txt"),
            dir.path().join("assets").join("link.txt"),
        )
        .unwrap();
        assert_rejected_in(dir, "/assets/link.txt").await;
    }
}
