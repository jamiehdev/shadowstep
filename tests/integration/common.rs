use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{test, web};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use shadowstep::config::Config;
use shadowstep::{app, build_state, run, run_metrics, AppState};
use std::collections::BTreeMap;
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
        tls_listen_addr: "127.0.0.1:0".to_owned(),
        upstream_timeout_seconds: 30,
        metrics_addr: None,
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
    spawn_with(origin_url, |_| {})
}

/// `spawn` with a customised config.
pub fn spawn_with(origin_url: &str, customise: impl FnOnce(&mut Config)) -> Running {
    spawn_workers(origin_url, 1, customise)
}

/// `spawn_with` on `workers` worker threads, so that concurrent connections
/// can land on different workers.
pub fn spawn_workers(
    origin_url: &str,
    workers: usize,
    customise: impl FnOnce(&mut Config),
) -> Running {
    let (state, assets) = state_with(origin_url, customise);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = run(state, listener, None, workers).unwrap();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    Running {
        addr,
        handle,
        _assets: assets,
    }
}

/// a proxy with `--metrics-addr` set, and the metrics listener's address.
pub struct WithMetricsListener {
    pub proxy: Running,
    pub metrics_addr: SocketAddr,
    metrics_handle: actix_web::dev::ServerHandle,
}

impl WithMetricsListener {
    pub fn metrics_url(&self, path: &str) -> String {
        format!("http://{}{}", self.metrics_addr, path)
    }

    pub async fn stop(self) {
        self.metrics_handle.stop(true).await;
        self.proxy.stop().await;
    }
}

/// `spawn` with a separate metrics listener on an ephemeral port.
pub fn spawn_with_metrics_listener(origin_url: &str) -> WithMetricsListener {
    let metrics_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let metrics_addr = metrics_listener.local_addr().unwrap();
    let (state, assets) = state_with(origin_url, |c| {
        c.metrics_addr = Some(metrics_addr.to_string());
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = run(state.clone(), listener, None, 1).unwrap();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let metrics = run_metrics(state, metrics_listener).unwrap();
    let metrics_handle = metrics.handle();
    actix_web::rt::spawn(metrics);
    WithMetricsListener {
        proxy: Running {
            addr,
            handle,
            _assets: assets,
        },
        metrics_addr,
        metrics_handle,
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

/// the `X-Shadowstep-Cache` value on a response, or "" when it is absent.
pub fn cache_status<B>(resp: &ServiceResponse<B>) -> String {
    resp.headers()
        .get("x-shadowstep-cache")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

/// a plain HTTP client that sends request bodies of type `B`.
pub fn client<B>() -> Client<HttpConnector, B>
where
    B: Body + Send,
    B::Data: Send,
{
    Client::builder(TokioExecutor::new()).build_http()
}

/// the whole body of a client response.
pub async fn body_bytes(body: Incoming) -> Bytes {
    body.collect().await.unwrap().to_bytes()
}

/// one sample line of a Prometheus text exposition.
#[derive(Debug)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// the samples in a Prometheus text exposition (format 0.0.4). comment
/// lines and blank lines are skipped.
pub fn parse_exposition(text: &str) -> Vec<Sample> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(parse_sample)
        .collect()
}

fn parse_sample(line: &str) -> Sample {
    let (series, value) = line.rsplit_once(' ').expect("sample without a value");
    let value = value
        .parse()
        .unwrap_or_else(|_| panic!("bad value in {line}"));
    let Some((name, labels)) = series.split_once('{') else {
        return Sample {
            name: series.to_owned(),
            labels: BTreeMap::new(),
            value,
        };
    };
    let labels = labels.strip_suffix('}').expect("unclosed label set");
    Sample {
        name: name.to_owned(),
        labels: parse_labels(labels),
        value,
    }
}

/// `key="value",...` with the text format's escapes for `\`, `"` and newline.
fn parse_labels(text: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        assert_eq!(chars.next(), Some('"'), "label {key} has no opening quote");
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some(other) => value.push(other),
                    None => panic!("dangling escape in label {key}"),
                },
                c => value.push(c),
            }
        }
        labels.insert(key.trim_start_matches(',').to_owned(), value);
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    labels
}

/// the value of the series `name` whose labels are exactly `labels`, or
/// `None` when the exposition has no such series.
pub fn metric(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let wanted: BTreeMap<String, String> = labels
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    samples
        .iter()
        .find(|s| s.name == name && s.labels == wanted)
        .map(|s| s.value)
}

/// the sum over every series of `name` that has all of `labels`.
pub fn metric_sum(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> f64 {
    samples
        .iter()
        .filter(|s| s.name == name)
        .filter(|s| {
            labels
                .iter()
                .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
        })
        .map(|s| s.value)
        .sum()
}
