use crate::common;

use bytes::Bytes;
use http_body_util::Empty;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;

/// longer than any step in these tests should take on a loaded CI runner.
const PATIENCE: Duration = Duration::from_secs(10);

/// what a raw origin read on its one connection.
struct Received {
    bytes: Vec<u8>,
    /// the bytes held a whole request, which the origin then answered
    complete: bool,
    /// the proxy closed the connection
    closed: bool,
}

impl std::fmt::Debug for Received {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Received")
            .field("bytes", &String::from_utf8_lossy(&self.bytes))
            .field("complete", &self.complete)
            .field("closed", &self.closed)
            .finish()
    }
}

impl Received {
    fn body(&self) -> &[u8] {
        let end = find(&self.bytes, b"\r\n\r\n").expect("no end of request head");
        &self.bytes[end + 4..]
    }

    fn head(&self) -> String {
        let end = find(&self.bytes, b"\r\n\r\n").expect("no end of request head");
        String::from_utf8_lossy(&self.bytes[..end]).to_ascii_lowercase()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// whether `bytes` hold a whole request with a `Content-Length` or chunked
/// body. the test bodies hold only letters, so a `0\r\n\r\n` can only be the
/// last chunk.
fn is_complete(bytes: &[u8]) -> bool {
    let Some(end) = find(bytes, b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let body = &bytes[end + 4..];
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse::<usize>().unwrap());
    match length {
        Some(length) => body.len() >= length,
        None => body.starts_with(b"0\r\n\r\n") || find(body, b"\r\n0\r\n\r\n").is_some(),
    }
}

/// an origin that accepts one connection and records what arrives on it. it
/// answers 200 only once it holds a whole request. `body_started` fires
/// once the request head and some body bytes have arrived.
async fn raw_origin() -> (String, oneshot::Receiver<()>, oneshot::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = oneshot::channel();
    let (received_tx, received_rx) = oneshot::channel();
    actix_web::rt::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let received = record(stream, started_tx).await;
        let _ = received_tx.send(received);
    });
    (url, started_rx, received_rx)
}

async fn record(mut stream: TcpStream, started: oneshot::Sender<()>) -> Received {
    let mut started = Some(started);
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = match timeout(PATIENCE, stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            // a reset closes the connection as surely as an EOF
            Ok(Err(_)) => 0,
            Err(_) => {
                return Received {
                    bytes,
                    complete: false,
                    closed: false,
                }
            }
        };
        if n == 0 {
            return Received {
                bytes,
                complete: false,
                closed: true,
            };
        }
        bytes.extend_from_slice(&buf[..n]);
        if find(&bytes, b"\r\n\r\n").is_some_and(|end| bytes.len() > end + 4) {
            if let Some(started) = started.take() {
                let _ = started.send(());
            }
        }
        if is_complete(&bytes) {
            let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(ok).await.unwrap();
            return Received {
                bytes,
                complete: true,
                closed: false,
            };
        }
    }
}

/// sends `head` and `start` to the proxy, waits until the origin holds part
/// of the body, then sends `rest` and half-closes the connection. returns
/// what the origin received and the proxy's whole response.
async fn interrupted_request(head: &str, start: &[u8], rest: &[u8]) -> (Received, String) {
    let (origin_url, body_started, received) = raw_origin().await;
    let proxy = common::spawn(&origin_url);

    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    client.write_all(head.as_bytes()).await.unwrap();
    client.write_all(start).await.unwrap();
    timeout(PATIENCE, body_started)
        .await
        .expect("the origin received no body")
        .unwrap();
    client.write_all(rest).await.unwrap();
    client.shutdown().await.unwrap();

    let received = timeout(PATIENCE, received)
        .await
        .expect("the origin connection stayed open")
        .unwrap();
    let mut response = Vec::new();
    timeout(PATIENCE, client.read_to_end(&mut response))
        .await
        .expect("the proxy kept the client connection open")
        .unwrap();

    assert_healthy(&proxy).await;
    proxy.stop().await;
    (received, String::from_utf8_lossy(&response).into_owned())
}

async fn assert_healthy(proxy: &common::Running) {
    let client = common::client::<Empty<Bytes>>();
    let resp = timeout(PATIENCE, client.get(proxy.url("/health").parse().unwrap()))
        .await
        .expect("health check timed out")
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn content_length_body_cut_short_does_not_reach_origin_whole() {
    let head = "POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n";

    let (received, response) = interrupted_request(head, &[b'a'; 40], b"").await;

    assert!(
        !received.complete,
        "origin got a whole request: {received:?}"
    );
    assert!(received.closed);
    assert!(received.head().contains("content-length: 100"));
    assert!(received.body().len() < 100, "{received:?}");
    assert!(
        response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "unexpected response: {response}"
    );
}

#[actix_web::test]
async fn chunked_body_cut_short_gets_no_last_chunk_at_origin() {
    let head = "POST /submit HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";

    let (received, response) = interrupted_request(head, b"5\r\nhello\r\n", b"").await;

    assert!(
        !received.complete,
        "origin got a whole request: {received:?}"
    );
    assert!(received.closed);
    assert!(received.head().contains("transfer-encoding: chunked"));
    assert!(find(received.body(), b"hello").is_some(), "{received:?}");
    assert!(
        response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "unexpected response: {response}"
    );
}

#[actix_web::test]
async fn malformed_chunk_gets_no_last_chunk_at_origin() {
    let head = "POST /submit HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";

    let (received, response) =
        interrupted_request(head, b"5\r\nhello\r\n", b"zz\r\nworld\r\n0\r\n\r\n").await;

    assert!(
        !received.complete,
        "origin got a whole request: {received:?}"
    );
    assert!(received.closed);
    assert!(find(received.body(), b"hello").is_some(), "{received:?}");
    assert!(find(received.body(), b"world").is_none(), "{received:?}");
    // actix closes the connection on a framing error while the handler runs
    assert!(response.is_empty(), "unexpected response: {response}");
}
