use crate::common::{self, Running};

use bytes::Bytes;
use futures_util::future::join_all;
use http_body_util::Empty;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use wiremock::matchers::{any, header, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// how long the origin takes to answer, which is the window in which
/// followers join the leader.
const ORIGIN_DELAY: Duration = Duration::from_secs(1);

/// longer than any step in these tests should take on a loaded CI runner,
/// and shorter than the default upstream timeout of 30 seconds, so that a
/// follower left waiting for its whole bounded wait fails the test.
const PATIENCE: Duration = Duration::from_secs(10);

/// how many followers each test sends after its leader.
const FOLLOWERS: usize = 5;

const WORKERS: usize = 4;

type HttpClient = Client<HttpConnector, Empty<Bytes>>;

/// what a client got back from the proxy.
#[derive(Debug)]
struct Answer {
    status: u16,
    cache: String,
    variant: String,
    body: Bytes,
}

async fn fetch(client: &HttpClient, url: &str, headers: &[(&str, &str)]) -> Answer {
    let mut request = hyper::Request::get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = timeout(
        PATIENCE,
        client.request(request.body(Empty::new()).unwrap()),
    )
    .await
    .expect("no response within the test's patience")
    .unwrap();
    let text = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let (status, cache, variant) = (
        response.status().as_u16(),
        text("x-shadowstep-cache"),
        text("x-variant"),
    );
    let body = timeout(PATIENCE, common::body_bytes(response.into_body()))
        .await
        .expect("no whole body within the test's patience");
    Answer {
        status,
        cache,
        variant,
        body,
    }
}

/// `n` concurrent GETs of `path`, each with `headers`.
async fn fetch_many(
    client: &HttpClient,
    server: &Running,
    path: &str,
    headers: &[(&str, &str)],
    n: usize,
) -> Vec<Answer> {
    let url = server.url(path);
    join_all((0..n).map(|_| fetch(client, &url, headers))).await
}

/// starts a GET of `path` and returns once the origin has received it, so
/// that requests sent next find it in flight.
fn lead(
    client: &HttpClient,
    server: &Running,
    path: &str,
    headers: &[(&'static str, &'static str)],
) -> actix_web::rt::task::JoinHandle<Answer> {
    let (client, url, headers) = (client.clone(), server.url(path), headers.to_vec());
    actix_web::rt::spawn(async move { fetch(&client, &url, &headers).await })
}

async fn origin_requests(origin: &MockServer) -> usize {
    origin.received_requests().await.unwrap().len()
}

/// waits until the origin has received `n` requests.
async fn origin_receives(origin: &MockServer, n: usize) {
    let started = Instant::now();
    while origin_requests(origin).await < n {
        assert!(
            started.elapsed() < PATIENCE,
            "the origin never received {n} requests"
        );
        actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn health(client: &HttpClient, server: &Running) -> serde_json::Value {
    let answer = fetch(client, &server.url("/health"), &[]).await;
    serde_json::from_slice(&answer.body).unwrap()
}

async fn origin_responding(template: ResponseTemplate) -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(any())
        .respond_with(template)
        .mount(&origin)
        .await;
    origin
}

fn cacheable(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("cache-control", "max-age=60")
        .set_body_string(body)
        .set_delay(ORIGIN_DELAY)
}

fn cache_statuses(answers: &[Answer]) -> Vec<&str> {
    answers.iter().map(|a| a.cache.as_str()).collect()
}

#[actix_web::test]
async fn concurrent_misses_reach_the_origin_once() {
    let origin = origin_responding(cacheable("shared")).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();

    let leader = lead(&client, &server, "/page", &[]);
    origin_receives(&origin, 1).await;
    let followers = fetch_many(&client, &server, "/page", &[], FOLLOWERS).await;
    let leader = leader.await.unwrap();

    assert_eq!((leader.status, leader.cache.as_str()), (200, "MISS"));
    assert_eq!(leader.body, "shared");
    for follower in &followers {
        assert_eq!(follower.status, 200);
        assert_eq!(follower.body, "shared");
    }
    assert_eq!(cache_statuses(&followers), ["COALESCED"; FOLLOWERS]);
    assert_eq!(origin_requests(&origin).await, 1);
    let health = health(&client, &server).await;
    assert_eq!(health["cache"]["coalesced"], FOLLOWERS);
    assert_eq!(health["cache"]["misses"], 1);
    server.stop().await;
}

/// answers with `template` and a body that echoes the request's
/// `X-Client`, so that each client can tell its own response.
fn echoing(template: ResponseTemplate) -> impl Fn(&Request) -> ResponseTemplate {
    move |request: &Request| {
        let client = request.headers.get("x-client").unwrap().to_str().unwrap();
        template.clone().set_body_string(client.to_owned())
    }
}

#[actix_web::test]
async fn uncacheable_responses_are_not_shared() {
    let templates = [
        ResponseTemplate::new(200).insert_header("cache-control", "private, max-age=60"),
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("set-cookie", "session=1"),
    ];
    for template in templates {
        let origin = MockServer::start().await;
        Mock::given(any())
            .respond_with(echoing(template.set_delay(ORIGIN_DELAY)))
            .mount(&origin)
            .await;
        let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
        let client = common::client();

        let leader = lead(&client, &server, "/page", &[("x-client", "leader")]);
        origin_receives(&origin, 1).await;
        let url = server.url("/page");
        let names: Vec<String> = (0..FOLLOWERS).map(|i| format!("follower-{i}")).collect();
        let followers = join_all(names.iter().map(|name| {
            let (client, url) = (&client, &url);
            async move { fetch(client, url, &[("x-client", name.as_str())]).await }
        }))
        .await;
        let leader = leader.await.unwrap();

        assert_eq!(leader.body, "leader");
        for (name, follower) in names.iter().zip(&followers) {
            assert_eq!(
                follower.body,
                name.as_str(),
                "got another client's response"
            );
            assert_eq!(follower.cache, "MISS");
        }
        assert_eq!(origin_requests(&origin).await, 1 + FOLLOWERS);
        assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
        server.stop().await;
    }
}

/// an origin that answers with `Vary: Accept-Encoding` and names the
/// variant it served in `X-Variant`.
async fn varying_origin(encodings: &[&str]) -> MockServer {
    let origin = MockServer::start().await;
    for encoding in encodings {
        Mock::given(header("accept-encoding", *encoding))
            .respond_with(
                cacheable("body")
                    .insert_header("vary", "Accept-Encoding")
                    .insert_header("x-variant", *encoding),
            )
            .mount(&origin)
            .await;
    }
    origin
}

#[actix_web::test]
async fn followers_with_other_vary_values_go_to_the_origin() {
    let origin = varying_origin(&["gzip", "br"]).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();

    let leader = lead(&client, &server, "/page", &[("accept-encoding", "gzip")]);
    origin_receives(&origin, 1).await;
    let url = server.url("/page");
    let (gzip, br) = futures_util::join!(
        join_all((0..3).map(|_| fetch(&client, &url, &[("accept-encoding", "gzip")]))),
        join_all((0..3).map(|_| fetch(&client, &url, &[("accept-encoding", "br")]))),
    );
    let leader = leader.await.unwrap();

    assert_eq!(
        (leader.cache.as_str(), leader.variant.as_str()),
        ("MISS", "gzip")
    );
    for answer in &gzip {
        assert_eq!(
            (answer.cache.as_str(), answer.variant.as_str()),
            ("COALESCED", "gzip")
        );
    }
    for answer in &br {
        assert_eq!(
            (answer.cache.as_str(), answer.variant.as_str()),
            ("MISS", "br")
        );
    }
    assert_eq!(origin_requests(&origin).await, 1 + 3);
    server.stop().await;
}

#[actix_web::test]
async fn known_vary_variants_coalesce_separately() {
    let origin = varying_origin(&["deflate", "gzip", "br"]).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();
    // a stored variant tells the cache that the URL varies on
    // Accept-Encoding before the other variants are requested
    fetch(
        &client,
        &server.url("/page"),
        &[("accept-encoding", "deflate")],
    )
    .await;

    let gzip_leader = lead(&client, &server, "/page", &[("accept-encoding", "gzip")]);
    origin_receives(&origin, 2).await;
    let br_leader = lead(&client, &server, "/page", &[("accept-encoding", "br")]);
    origin_receives(&origin, 3).await;
    let url = server.url("/page");
    let (gzip, br) = futures_util::join!(
        join_all((0..3).map(|_| fetch(&client, &url, &[("accept-encoding", "gzip")]))),
        join_all((0..3).map(|_| fetch(&client, &url, &[("accept-encoding", "br")]))),
    );

    assert_eq!(br_leader.await.unwrap().cache, "MISS", "br followed gzip");
    assert_eq!(gzip_leader.await.unwrap().cache, "MISS");
    for (answers, encoding) in [(&gzip, "gzip"), (&br, "br")] {
        for answer in answers {
            assert_eq!(
                (answer.cache.as_str(), answer.variant.as_str()),
                ("COALESCED", encoding)
            );
        }
    }
    assert_eq!(origin_requests(&origin).await, 3);
    server.stop().await;
}

#[actix_web::test]
async fn response_over_the_size_cap_is_not_shared() {
    let body: Vec<u8> = (0..2 * MIB).map(|i| (i % 251) as u8).collect();
    let origin = origin_responding(
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_bytes(body.clone())
            .set_delay(ORIGIN_DELAY),
    )
    .await;
    // a 1 MiB cache caps entries at 1 MiB
    let server = common::spawn_workers(&origin.uri(), WORKERS, |c| c.cache_size_mb = 1);
    let client = common::client();

    let leader = lead(&client, &server, "/large", &[]);
    origin_receives(&origin, 1).await;
    let followers = fetch_many(&client, &server, "/large", &[], FOLLOWERS).await;
    let leader = leader.await.unwrap();

    for answer in followers.iter().chain([&leader]) {
        assert_eq!((answer.status, answer.cache.as_str()), (200, "MISS"));
        assert!(answer.body == body, "client received a different body");
    }
    assert_eq!(origin_requests(&origin).await, 1 + FOLLOWERS);
    assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
    server.stop().await;
}

#[actix_web::test]
async fn followers_of_a_failed_leader_get_their_own_answer() {
    let origin = origin_responding(ResponseTemplate::new(502).set_delay(ORIGIN_DELAY)).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();

    let leader = lead(&client, &server, "/page", &[]);
    origin_receives(&origin, 1).await;
    let followers = fetch_many(&client, &server, "/page", &[], FOLLOWERS).await;
    let leader = leader.await.unwrap();

    for answer in followers.iter().chain([&leader]) {
        assert_eq!((answer.status, answer.cache.as_str()), (502, "MISS"));
    }
    assert_eq!(origin_requests(&origin).await, 1 + FOLLOWERS);
    assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
    server.stop().await;
}

#[actix_web::test]
async fn followers_of_a_timed_out_leader_are_answered_in_time() {
    let origin = origin_responding(cacheable("late").set_delay(Duration::from_secs(5))).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |c| {
        c.upstream_timeout_seconds = 1;
    });
    let client = common::client();

    let started = Instant::now();
    let leader = lead(&client, &server, "/page", &[]);
    origin_receives(&origin, 1).await;
    let followers = fetch_many(&client, &server, "/page", &[], FOLLOWERS).await;
    let leader = leader.await.unwrap();

    for answer in followers.iter().chain([&leader]) {
        assert_eq!(answer.status, 504);
    }
    // the leader's timeout releases the followers, and each then waits at
    // most one timeout of its own
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
    server.stop().await;
}

/// how the origin sends the rest of the first response's body.
#[derive(Clone, Copy)]
enum FirstBody {
    /// one chunk every 100 ms, so the whole body takes over 6 seconds
    Trickle,
    /// never, holding the connection open
    Stall,
}

const CHUNK: usize = 16 * KIB;
const CHUNKS: usize = 64;

/// an origin whose cacheable response is "first" and then `CHUNKS` chunks of
/// `CHUNK` bytes. the first connection gets the head and "first", then the
/// rest as `first_body` says. every later connection gets the whole
/// response at once. the counter holds the number of connections.
async fn origin_slowing_the_first_body(first_body: FirstBody) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    actix_web::rt::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            actix_web::rt::spawn(answer_slowly(stream, first.then_some(first_body)));
        }
    });
    (url, connections)
}

fn slow_body() -> Vec<u8> {
    let mut body = b"first".to_vec();
    body.resize(5 + CHUNKS * CHUNK, b'x');
    body
}

async fn answer_slowly(mut stream: TcpStream, first_body: Option<FirstBody>) {
    let mut head = Vec::new();
    let mut buf = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let body = slow_body();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    match first_body {
        None => {
            let _ = stream.write_all(&body).await;
        }
        Some(FirstBody::Trickle) => {
            let _ = stream.write_all(&body[..5]).await;
            for chunk in body[5..].chunks(CHUNK) {
                actix_web::rt::time::sleep(Duration::from_millis(100)).await;
                if stream.write_all(chunk).await.is_err() {
                    return;
                }
            }
        }
        Some(FirstBody::Stall) => {
            let _ = stream.write_all(&body[..5]).await;
            // hold the connection open until the proxy closes it
            while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
        }
    }
}

/// sends a GET for `/page` on a raw connection and returns it once the
/// start of the body has arrived.
async fn raw_leader(server: &Running) -> TcpStream {
    let mut leader = TcpStream::connect(server.addr).await.unwrap();
    // the same host as the followers', which is part of the cache key
    let request = format!("GET /page HTTP/1.1\r\nhost: {}\r\n\r\n", server.addr);
    leader.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut buf = [0; 1024];
    while !received.ends_with(b"first") {
        let n = timeout(PATIENCE, leader.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "the proxy closed the leader's connection");
        received.extend_from_slice(&buf[..n]);
    }
    leader
}

/// starts `FOLLOWERS` GETs for `/page` and gives them time to join the
/// leader's flight. no event shows that a follower is waiting, and a
/// follower that joins late goes to the origin and still passes.
async fn spawn_followers(
    client: &HttpClient,
    server: &Running,
) -> actix_web::rt::task::JoinHandle<Vec<Answer>> {
    let (client, url) = (client.clone(), server.url("/page"));
    let followers = actix_web::rt::spawn(async move {
        join_all((0..FOLLOWERS).map(|_| fetch(&client, &url, &[]))).await
    });
    actix_web::rt::time::sleep(Duration::from_millis(300)).await;
    followers
}

fn assert_own_whole_responses(followers: &[Answer]) {
    for answer in followers {
        assert_eq!((answer.status, answer.cache.as_str()), (200, "MISS"));
        assert!(answer.body == slow_body(), "follower got a different body");
    }
}

#[actix_web::test]
async fn followers_are_released_when_the_leaders_client_disconnects() {
    let (origin_url, connections) = origin_slowing_the_first_body(FirstBody::Trickle).await;
    let server = common::spawn_workers(&origin_url, WORKERS, |_| {});
    let client = common::client();

    let leader = raw_leader(&server).await;
    let followers = spawn_followers(&client, &server).await;
    let disconnected = Instant::now();
    drop(leader);
    let followers = followers.await.unwrap();

    // the leader's body would take over 6 seconds to arrive, and the
    // upstream timeout is 30 seconds
    assert!(
        disconnected.elapsed() < Duration::from_secs(3),
        "followers waited {:?} after the disconnect",
        disconnected.elapsed()
    );
    assert_own_whole_responses(&followers);
    assert_eq!(connections.load(Ordering::SeqCst), 1 + FOLLOWERS);
    assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
    server.stop().await;
}

#[actix_web::test]
async fn followers_of_a_stalled_leader_stop_waiting_after_the_upstream_timeout() {
    let (origin_url, connections) = origin_slowing_the_first_body(FirstBody::Stall).await;
    let server = common::spawn_workers(&origin_url, WORKERS, |c| {
        c.upstream_timeout_seconds = 2;
    });
    let client = common::client();

    let _leader = raw_leader(&server).await;
    let started = Instant::now();
    let followers = spawn_followers(&client, &server).await.await.unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "followers did not wait for the leader"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "followers waited {:?}",
        started.elapsed()
    );
    assert_own_whole_responses(&followers);
    assert_eq!(connections.load(Ordering::SeqCst), 1 + FOLLOWERS);
    server.stop().await;
}

#[actix_web::test]
async fn requests_that_bypass_the_cache_do_not_coalesce() {
    let origin = origin_responding(cacheable("body")).await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();
    let cases = [
        ("/authorization", ("authorization", "Bearer token")),
        ("/cookie", ("cookie", "session=1")),
        ("/no-cache", ("cache-control", "no-cache")),
        ("/if-none-match", ("if-none-match", "\"v1\"")),
    ];

    for (n, (path, field)) in cases.into_iter().enumerate() {
        let before = 2 * n;
        let leader = lead(&client, &server, path, &[]);
        origin_receives(&origin, before + 1).await;
        let follower = fetch(&client, &server.url(path), &[field]).await;
        let leader = leader.await.unwrap();

        assert_eq!(leader.cache, "MISS");
        assert_eq!(follower.cache, "MISS", "{field:?} coalesced");
        assert_eq!(origin_requests(&origin).await, before + 2, "{field:?}");
    }
    assert_eq!(health(&client, &server).await["cache"]["coalesced"], 0);
    server.stop().await;
}

#[actix_web::test]
async fn concurrent_stale_requests_send_one_conditional_request() {
    let origin = MockServer::start().await;
    Mock::given(header("if-none-match", "\"v1\""))
        .respond_with(
            ResponseTemplate::new(304)
                .insert_header("cache-control", "max-age=60")
                .insert_header("etag", "\"v1\"")
                .set_delay(ORIGIN_DELAY),
        )
        .mount(&origin)
        .await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=1")
                .insert_header("etag", "\"v1\"")
                .set_body_string("stored"),
        )
        .mount(&origin)
        .await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();
    fetch(&client, &server.url("/page"), &[]).await;
    actix_web::rt::time::sleep(Duration::from_millis(1100)).await;

    let leader = lead(&client, &server, "/page", &[]);
    origin_receives(&origin, 2).await;
    let followers = fetch_many(&client, &server, "/page", &[], FOLLOWERS).await;
    let leader = leader.await.unwrap();

    assert_eq!(
        (leader.cache.as_str(), leader.body.as_ref()),
        ("REVALIDATED", &b"stored"[..])
    );
    for follower in &followers {
        assert_eq!(follower.status, 200);
        assert_eq!(follower.body, "stored");
    }
    assert_eq!(cache_statuses(&followers), ["COALESCED"; FOLLOWERS]);
    assert_eq!(origin_requests(&origin).await, 2);
    server.stop().await;
}

#[actix_web::test]
async fn different_keys_do_not_wait_for_each_other() {
    let origin = MockServer::start().await;
    Mock::given(path("/slow"))
        .respond_with(cacheable("slow").set_delay(Duration::from_secs(4)))
        .mount(&origin)
        .await;
    Mock::given(path("/fast"))
        .respond_with(cacheable("fast"))
        .mount(&origin)
        .await;
    let server = common::spawn_workers(&origin.uri(), WORKERS, |_| {});
    let client = common::client();

    let slow = lead(&client, &server, "/slow", &[]);
    origin_receives(&origin, 1).await;
    let started = Instant::now();
    let fast_leader = lead(&client, &server, "/fast", &[]);
    origin_receives(&origin, 2).await;
    let fast = fetch_many(&client, &server, "/fast", &[], FOLLOWERS).await;
    let fast_leader = fast_leader.await.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "/fast waited {:?} for /slow",
        started.elapsed()
    );
    assert_eq!(fast_leader.cache, "MISS");
    assert_eq!(cache_statuses(&fast), ["COALESCED"; FOLLOWERS]);
    assert_eq!(slow.await.unwrap().body, "slow");
    assert_eq!(origin_requests(&origin).await, 2);
    server.stop().await;
}
