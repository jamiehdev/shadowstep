use crate::common::{self, Running};

use bytes::Bytes;
use http_body_util::Empty;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// the upstream timeout in these tests, which is also the longest the origin
/// may stay silent during a response body.
const IDLE: Duration = Duration::from_secs(1);

/// how long past `IDLE` the proxy may take to give up on a silent origin.
const MARGIN: Duration = Duration::from_millis(1500);

/// longer than any step in these tests should take on a loaded CI runner.
const PATIENCE: Duration = Duration::from_secs(10);

/// what the origin sends on one connection after the request head.
struct Reply {
    head: String,
    /// sent in order, each after its delay
    parts: Vec<(Duration, Vec<u8>)>,
    /// after the parts, hold the connection open until the proxy closes it
    stall: bool,
}

fn cacheable_head(framing: &str) -> String {
    format!("HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\n{framing}\r\n\r\n")
}

/// 40 of 100 bytes, then silence.
fn stalled_sized() -> Reply {
    Reply {
        head: cacheable_head("content-length: 100"),
        parts: vec![(Duration::ZERO, vec![b'a'; 40])],
        stall: true,
    }
}

/// one chunk of 40 bytes, then silence with no last chunk.
fn stalled_chunked() -> Reply {
    let mut chunk = b"28\r\n".to_vec();
    chunk.extend_from_slice(&[b'a'; 40]);
    chunk.extend_from_slice(b"\r\n");
    Reply {
        head: cacheable_head("transfer-encoding: chunked"),
        parts: vec![(Duration::ZERO, chunk)],
        stall: true,
    }
}

fn whole(body: &[u8]) -> Reply {
    Reply {
        head: cacheable_head(&format!("content-length: {}", body.len())),
        parts: vec![(Duration::ZERO, body.to_vec())],
        stall: false,
    }
}

/// an origin that answers its n-th connection with `replies[n]`. it counts
/// connections and reports on `closed` each stalled connection that the
/// proxy closes.
struct Origin {
    url: String,
    connections: Arc<AtomicUsize>,
    closed: mpsc::UnboundedReceiver<usize>,
}

async fn scripted_origin(replies: Vec<Reply>) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let counter = connections.clone();
    actix_web::rt::spawn(async move {
        for reply in replies {
            let (stream, _) = listener.accept().await.unwrap();
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let closed_tx = closed_tx.clone();
            actix_web::rt::spawn(async move {
                if answer(stream, reply).await {
                    let _ = closed_tx.send(n);
                }
            });
        }
        // any further connection counts but gets no answer
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    Origin {
        url,
        connections,
        closed,
    }
}

/// sends `reply` after the request head. returns true when the reply
/// stalled and the proxy then closed the connection.
async fn answer(mut stream: TcpStream, reply: Reply) -> bool {
    let mut head = Vec::new();
    let mut buf = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let _ = stream.write_all(reply.head.as_bytes()).await;
    for (delay, part) in reply.parts {
        actix_web::rt::time::sleep(delay).await;
        if stream.write_all(&part).await.is_err() {
            return false;
        }
    }
    if !reply.stall {
        return false;
    }
    // a reset closes the connection as surely as an EOF
    while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
    true
}

fn spawn(origin: &Origin) -> Running {
    common::spawn_with(&origin.url, |c| {
        c.upstream_timeout_seconds = IDLE.as_secs();
    })
}

/// the proxy's whole response to a GET of `path` on a raw connection, which
/// ends when the proxy closes it, and how long that took.
async fn raw_get(proxy: &Running, path: &str) -> (Vec<u8>, Duration) {
    let started = Instant::now();
    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    client.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(PATIENCE, client.read_to_end(&mut response))
        .await
        .expect("the proxy kept the client connection open")
        // a reset ends the response as surely as an EOF
        .ok();
    (response, started.elapsed())
}

fn split(response: &[u8]) -> (String, &[u8]) {
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no end of response head");
    let head = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
    (head, &response[end + 4..])
}

fn assert_given_up_in_time(elapsed: Duration) {
    assert!(elapsed >= IDLE - Duration::from_millis(100), "{elapsed:?}");
    assert!(elapsed < IDLE + MARGIN, "took {elapsed:?}");
}

/// waits until the proxy has closed the origin connection `n`.
async fn origin_connection_closed(origin: &mut Origin, n: usize) {
    let closed = timeout(PATIENCE, origin.closed.recv())
        .await
        .expect("the proxy kept the origin connection open");
    assert_eq!(closed, Some(n));
}

async fn assert_healthy(proxy: &Running) {
    let client = common::client::<Empty<Bytes>>();
    let resp = timeout(PATIENCE, client.get(proxy.url("/health").parse().unwrap()))
        .await
        .expect("health check timed out")
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn sized_body_that_stalls_closes_the_client_connection_short() {
    let mut origin = scripted_origin(vec![stalled_sized()]).await;
    let proxy = spawn(&origin);

    let (response, elapsed) = raw_get(&proxy, "/page").await;

    assert_given_up_in_time(elapsed);
    let (head, body) = split(&response);
    assert!(head.starts_with("http/1.1 200 ok"), "{head}");
    assert!(head.contains("content-length: 100"), "{head}");
    assert!(body.len() < 100, "got {} bytes", body.len());
    origin_connection_closed(&mut origin, 0).await;
    assert_healthy(&proxy).await;
    proxy.stop().await;
}

#[actix_web::test]
async fn chunked_body_that_stalls_gets_no_last_chunk() {
    let mut origin = scripted_origin(vec![stalled_chunked()]).await;
    let proxy = spawn(&origin);

    let (response, elapsed) = raw_get(&proxy, "/page").await;

    assert_given_up_in_time(elapsed);
    let (head, body) = split(&response);
    assert!(head.starts_with("http/1.1 200 ok"), "{head}");
    assert!(head.contains("transfer-encoding: chunked"), "{head}");
    // the body holds only letters, so a `0\r\n\r\n` can only be the last chunk
    assert!(
        !body.windows(5).any(|w| w == b"0\r\n\r\n"),
        "{}",
        String::from_utf8_lossy(body)
    );
    origin_connection_closed(&mut origin, 0).await;
    assert_healthy(&proxy).await;
    proxy.stop().await;
}

#[actix_web::test]
async fn slow_but_steady_body_arrives_whole() {
    // ten chunks 300 ms apart take 3 seconds, three times the idle limit
    let parts = (0..10)
        .map(|_| (Duration::from_millis(300), vec![b'a'; 10]))
        .collect();
    let steady = Reply {
        head: cacheable_head("content-length: 100"),
        parts,
        stall: false,
    };
    let origin = scripted_origin(vec![steady]).await;
    let proxy = spawn(&origin);

    let (response, elapsed) = raw_get(&proxy, "/page").await;

    assert!(elapsed >= Duration::from_secs(3), "{elapsed:?}");
    let (head, body) = split(&response);
    assert!(head.starts_with("http/1.1 200 ok"), "{head}");
    assert_eq!(body, [b'a'; 100]);
    assert_healthy(&proxy).await;
    proxy.stop().await;
}

#[actix_web::test]
async fn stalled_cacheable_body_is_not_stored() {
    let mut origin = scripted_origin(vec![stalled_sized(), whole(b"whole")]).await;
    let proxy = spawn(&origin);

    let (_, elapsed) = raw_get(&proxy, "/page").await;
    assert_given_up_in_time(elapsed);
    origin_connection_closed(&mut origin, 0).await;
    let (response, _) = raw_get(&proxy, "/page").await;

    let (head, body) = split(&response);
    assert!(head.contains("x-shadowstep-cache: miss"), "{head}");
    assert_eq!(body, b"whole");
    assert_eq!(origin.connections.load(Ordering::SeqCst), 2);
    assert_healthy(&proxy).await;
    proxy.stop().await;
}
