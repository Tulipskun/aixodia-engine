//! Port of the Go `io/wsock` tests.
//!
//! The Go tests drive the handshake and the frame bytes end to end over a real
//! `httptest` server; these do the same over a real axum server on
//! `127.0.0.1:0`, so what is asserted is the wire, not the wrapper.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use aixodia_wsock::client::Client;
use aixodia_wsock::{
    accept, accept_value, bearer_ok, read_frame, write_frame, Error, ServerConn, UpgradedStream,
    OP_BINARY, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT,
};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Serves one WebSocket endpoint on an ephemeral port and returns its
/// `ws://127.0.0.1:<port>` URL. `handler` owns the connection, exactly as the
/// Go handlers do.
async fn spawn_server<F>(handler: F) -> String
where
    F: Fn(ServerConn<UpgradedStream>) -> BoxFuture + Send + Sync + Clone + 'static,
{
    let app = Router::new().fallback(move |request: Request<Body>| {
        let handler = handler.clone();
        // `accept` wants an `FnOnce` that owns its state, so the clone has to be
        // moved into a closure; `&handler` would not outlive this block.
        #[allow(clippy::redundant_closure)]
        async move {
            accept(request, move |conn| handler(conn))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{addr}")
}

/// Serves a plain HTTP endpoint that answers everything with one status, which
/// is what the Go test's rejected-handshake server is.
async fn spawn_status_server(status: StatusCode) -> String {
    let app =
        Router::new().fallback(move || async move { (status, "token rejected").into_response() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.parse().expect("header value"),
        );
    }
    map
}

// ---------------------------------------------------------------------------
// wsock_test.go
// ---------------------------------------------------------------------------

#[test]
fn bearer_ok_matches_the_go_table() {
    let cases: [(&str, &str, &str, bool); 6] = [
        ("matching token", "Bearer good", "good", true),
        ("wrong token", "Bearer bad", "good", false),
        ("no header", "", "good", false),
        ("engine has no token", "Bearer good", "", false),
        (
            "extra text after the token",
            "Bearer good extra",
            "good",
            false,
        ),
        // The scheme is optional here because /ws never required it: the header
        // is compared after trimming `Bearer `, so a bare token matched before
        // this package existed and must keep matching.
        ("bare token, no scheme", "good", "good", true),
    ];
    for (name, header, token, want) in cases {
        let map = if header.is_empty() {
            HeaderMap::new()
        } else {
            headers(&[("Authorization", header)])
        };
        assert_eq!(bearer_ok(&map, token), want, "case {name}");
    }
}

#[test]
fn is_upgrade_needs_the_whole_handshake() {
    assert!(!aixodia_wsock::is_upgrade(&HeaderMap::new()));
    assert!(!aixodia_wsock::is_upgrade(&headers(&[(
        "Upgrade",
        "websocket"
    )])));
    let full = headers(&[
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ]);
    assert!(aixodia_wsock::is_upgrade(&full));
    // A version other than 13 is not this protocol.
    let mut wrong = full.clone();
    wrong.insert("sec-websocket-version", "8".parse().expect("value"));
    assert!(!aixodia_wsock::is_upgrade(&wrong));
}

#[tokio::test]
async fn accept_rejects_a_plain_request() {
    // Nothing has been written to the response when the request is not an
    // upgrade, so the caller can still answer with a plain error.
    let request = Request::builder()
        .uri("/terminal")
        .body(Body::empty())
        .expect("request");
    let response = accept(request, |_| async {});
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accept_writes_the_handshake() {
    let url = spawn_server(|mut conn| {
        Box::pin(async move {
            let _ = conn.close().await;
        })
    })
    .await;
    let addr = url.trim_start_matches("ws://").to_string();

    let mut stream = TcpStream::connect(&addr).await.expect("dial");
    // The RFC 6455 example key, whose accept value is fixed by the spec.
    let request = "GET /terminal HTTP/1.1\r\n\
        Host: engine\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\
        Sec-WebSocket-Version: 13\r\n\
        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write handshake");

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.expect("read response");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    assert!(
        head.starts_with("HTTP/1.1 101 "),
        "status line = {:?}",
        head.lines().next().unwrap_or_default()
    );
    // Header names come back lower-cased from hyper, so compare case-insensitively.
    assert!(
        head.to_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "handshake response lacks the RFC example accept value:\n{head}"
    );
}

#[test]
fn the_accept_value_is_the_rfc_example() {
    assert_eq!(
        accept_value("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[tokio::test]
async fn read_write_frame_round_trip() {
    for len in [5usize, 200, 70000] {
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut buffer: Vec<u8> = Vec::new();
        write_frame(&mut buffer, OP_BINARY, &payload)
            .await
            .unwrap_or_else(|err| panic!("write len={len}: {err}"));
        let (op, got) = read_frame(&mut &buffer[..])
            .await
            .unwrap_or_else(|err| panic!("read len={len}: {err}"));
        assert_eq!(op, OP_BINARY, "len={len}");
        assert_eq!(got, payload, "payload round trip differs at len={len}");
    }
}

#[tokio::test]
async fn read_frame_unmasks_client_frames() {
    let payload = b"hello masked";
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut buffer: Vec<u8> = Vec::new();
    buffer.push(0x80 | OP_BINARY);
    buffer.push(0x80 | payload.len() as u8);
    buffer.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        buffer.push(byte ^ mask[index % 4]);
    }
    let (op, got) = read_frame(&mut &buffer[..]).await.expect("read frame");
    assert_eq!(op, OP_BINARY);
    assert_eq!(got, payload.to_vec());
}

// ---------------------------------------------------------------------------
// client_test.go
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_echoes_with_masked_frames_and_auth_header() {
    let url = spawn_server(|mut conn| {
        Box::pin(async move {
            loop {
                match conn.read_message().await {
                    Err(Error::Closed) => return,
                    Err(_) => return,
                    Ok((op, payload)) => {
                        if conn.write_message(op, &payload).await.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    })
    .await;

    let mut client = Client::dial(
        &url,
        &[("Authorization".to_string(), "Bearer secret".to_string())],
    )
    .await
    .expect("dial");
    client
        .write_message(OP_TEXT, br#"{"ch":"chat"}"#)
        .expect("write");
    let (op, payload) = client.read_message().await.expect("read");
    assert_eq!(op, OP_TEXT);
    assert_eq!(payload, br#"{"ch":"chat"}"#);

    let blob: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    client
        .write_message(OP_BINARY, &blob)
        .expect("write binary");
    let (op, payload) = client.read_message().await.expect("read binary");
    assert_eq!(op, OP_BINARY);
    assert_eq!(payload, blob);
}

#[tokio::test]
async fn client_answers_a_server_ping() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let url = spawn_server(move |mut conn| {
        let tx = tx.clone();
        Box::pin(async move {
            if conn.write_message(OP_PING, b"hi").await.is_err() {
                return;
            }
            loop {
                match read_frame(conn.stream_mut()).await {
                    Ok((OP_PONG, payload)) => {
                        let _ = tx.send(payload);
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })
    })
    .await;

    let mut client = Client::dial(&url, &[]).await.expect("dial");
    let reader = tokio::spawn(async move {
        let _ = client.read_message().await;
    });
    let payload = tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("client did not answer the ping")
        .expect("pong");
    assert_eq!(payload, b"hi");
    reader.await.expect("read task");
}

#[tokio::test]
async fn client_close_is_eof() {
    let url = spawn_server(|mut conn| {
        Box::pin(async move {
            let _ = conn.write_message(OP_CLOSE, &[]).await;
            let _ = conn.close().await;
        })
    })
    .await;

    let mut client = Client::dial(&url, &[]).await.expect("dial");
    let err = client.read_message().await.expect_err("close is an end");
    assert!(err.is_eof(), "close = {err}, want an end-of-stream");
}

/// A peer that accepts the handshake and then says nothing (no data, no ping, no
/// close) must not park the reader forever: liveness turns the silence into an
/// error the relay can reconnect on.
#[tokio::test]
async fn client_liveness_times_out_a_silent_peer() {
    // The peer accepts the handshake and then says nothing at all, while
    // keeping the socket open.
    let url = spawn_server(|conn| {
        Box::pin(async move {
            let _conn = conn;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
    })
    .await;

    let mut client = Client::dial(&url, &[]).await.expect("dial");
    client.set_liveness(Some(Duration::from_millis(200)));

    let start = Instant::now();
    let err = client.read_message().await.expect_err("want a timeout");
    assert!(err.is_timeout(), "err = {err}, want a timeout");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "liveness took {:?}, want well under 3s",
        start.elapsed()
    );
}

/// An idle but healthy peer is kept alive by its pings: each pong refreshes the
/// liveness clock, so a socket that never carries data is not torn down.
#[tokio::test]
async fn client_liveness_survives_pongs() {
    let url = spawn_server(|conn| {
        Box::pin(async move {
            let (mut reader, writer) = conn.split();
            let pinger = writer.clone();
            let stop = tokio::spawn(async move {
                loop {
                    if pinger.write_message(OP_PING, &[]).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
            loop {
                match reader.read_message().await {
                    Ok((OP_TEXT, payload)) => {
                        if writer.write_message(OP_TEXT, &payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            stop.abort();
        })
    })
    .await;

    let mut client = Client::dial(&url, &[]).await.expect("dial");
    client.set_liveness(Some(Duration::from_millis(300)));

    // Wait past several pings before sending anything: the pongs must have kept
    // the reader alive, so this round trip succeeds instead of timing out.
    tokio::time::sleep(Duration::from_millis(900)).await;
    client
        .write_message(OP_TEXT, b"still here")
        .expect("write after pings");
    let (op, payload) = client.read_message().await.expect("read after pings");
    assert_eq!(op, OP_TEXT);
    assert_eq!(payload, b"still here");
}

#[tokio::test]
async fn client_rejects_an_unauthorized_handshake() {
    let url = spawn_status_server(StatusCode::UNAUTHORIZED).await;
    let ws_url = format!("ws{}", url.trim_start_matches("http"));
    let err = match Client::dial(&ws_url, &[]).await {
        Ok(_) => panic!("a 401 must not dial"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("401"),
        "dial error = {err}, want a 401"
    );
}

/// The client half must mask: a frame it writes has the mask bit set on the
/// wire. `read_frame` on the server side already unmasks, so this asserts the
/// bit itself rather than a round trip.
#[tokio::test]
async fn client_masks_frames() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<[u8; 2]>();
    let url = spawn_server(move |mut conn| {
        let tx = tx.clone();
        Box::pin(async move {
            let mut buf = [0u8; 2];
            if conn.stream_mut().read_exact(&mut buf).await.is_ok() {
                let _ = tx.send(buf);
            }
        })
    })
    .await;

    let client = Client::dial(&url, &[]).await.expect("dial");
    client.write_message(OP_TEXT, b"x").expect("write");
    let head = tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("server never saw the frame")
        .expect("head");
    assert_ne!(head[1] & 0x80, 0, "mask bit not set: {:08b}", head[1]);
}
