use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{test, web};
use shadowstep::config::Config;
use shadowstep::{app, build_state, run, AppState};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use tempfile::TempDir;

/// a `Config` for tests, built as a literal so that env vars such as
/// `ORIGIN_URL` in the test process cannot leak in through clap.
pub fn config(origin_url: &str, asset_path: &Path) -> Config {
    Config {
        origin_url: origin_url.to_owned(),
        listen_addr: "127.0.0.1:0".to_owned(),
        asset_path: asset_path.to_owned(),
        cache_ttl_seconds: 300,
        cache_size_mb: 100,
        tls_cert_path: None,
        tls_key_path: None,
        upstream_timeout_seconds: 30,
    }
}

/// state pointing at `origin_url`, with an empty asset directory that lives
/// as long as the returned `TempDir`. `customise` adjusts the test config.
pub fn state_with(
    origin_url: &str,
    customise: impl FnOnce(&mut Config),
) -> (web::Data<AppState>, TempDir) {
    let assets = tempfile::tempdir().unwrap();
    let mut config = config(origin_url, assets.path());
    customise(&mut config);
    let state = build_state(&config).unwrap();
    (state, assets)
}

/// an in-process service for `app`, without a socket.
pub async fn service(
    origin_url: &str,
) -> (
    impl Service<
        actix_http::Request,
        Response = ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
    >,
    TempDir,
) {
    service_with(origin_url, |_| {}).await
}

/// `service` with a customised config.
pub async fn service_with(
    origin_url: &str,
    customise: impl FnOnce(&mut Config),
) -> (
    impl Service<
        actix_http::Request,
        Response = ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
    >,
    TempDir,
) {
    let (state, assets) = state_with(origin_url, customise);
    (test::init_service(app(state)).await, assets)
}

/// a real server on an ephemeral port, for tests that need a socket.
pub struct Running {
    pub addr: SocketAddr,
    handle: actix_web::dev::ServerHandle,
    _assets: TempDir,
}

impl Running {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub async fn stop(self) {
        self.handle.stop(true).await;
    }
}

pub fn spawn(origin_url: &str) -> Running {
    let (state, assets) = state_with(origin_url, |_| {});
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = run(state, listener, None, 1).unwrap();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    Running {
        addr,
        handle,
        _assets: assets,
    }
}

/// an origin URL that refuses connections: bind an ephemeral port, then
/// release it.
pub fn unreachable_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
