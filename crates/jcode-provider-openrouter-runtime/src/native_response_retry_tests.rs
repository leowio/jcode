use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn run_native_failure(message: &str) -> (Vec<Result<StreamEvent>>, usize) {
    run_native_payload(serde_json::json!({"type":"error", "message":message}))
}

fn run_native_payload(error: Value) -> (Vec<Result<StreamEvent>>, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let mut requests = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        'requests: while !server_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    if headers.starts_with("get / http/") {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        continue 'requests;
                    }
                    assert!(headers.starts_with("post /v1/responses "), "{headers}");
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests += 1;
            let body = if requests == 1 {
                format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\ndata: {error}\n\n"
                )
            } else {
                concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"final\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
                )
                .to_string()
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            if requests == 2 {
                break;
            }
        }
        requests
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(run_stream_with_retries(
            reqwest::Client::new(),
            base,
            ProviderAuth::None {
                label: "test".into(),
            },
            false,
            "native-body-retry-test".into(),
            serde_json::json!({"model":"test", "input":[], "stream":true}),
            false,
            tx,
            Arc::new(Mutex::new(None)),
            "test".into(),
            true,
        ));
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        })
        .await;
        task.abort();
        result
    });
    stop.store(true, Ordering::SeqCst);
    let requests = server.join().unwrap();
    (result.expect("native retry must finish"), requests)
}

#[test]
fn native_response_body_failure_retries_and_rolls_back_partial_output() {
    let (events, requests) = run_native_failure("native upstream response body failed");
    assert_eq!(
        requests, 2,
        "native SSE errors must enter the provider retry loop"
    );
    let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
    let rollback = events
        .iter()
        .position(|event| matches!(event, StreamEvent::RetryRollback { .. }))
        .unwrap();
    assert!(
        events[..rollback]
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "partial"))
    );
    assert!(
        events[rollback + 1..]
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "final"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error { .. }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageEnd { .. }))
            .count(),
        1
    );
}

#[test]
fn native_response_body_wording_does_not_override_permanent_or_unsafe_failure() {
    for prefix in [
        "invalid_request_error",
        "invalid API key",
        "unsafe_to_replay",
    ] {
        let (events, requests) =
            run_native_failure(&format!("{prefix}: native upstream response body failed"));
        assert_eq!(requests, 1, "{prefix} must not retry");
        let error = events
            .into_iter()
            .find_map(Result::err)
            .expect("surface permanent error");
        assert!(error.to_string().contains(prefix));
    }
    for error in [
        serde_json::json!({"type":"error", "error": {
            "type":"invalid_request_error", "message":"native upstream response body failed"
        }}),
        serde_json::json!({"type":"response.failed", "response":{"error": {
            "code":"invalid_request_error", "message":"native upstream response body failed"
        }}}),
    ] {
        let (events, requests) = run_native_payload(error);
        assert_eq!(requests, 1, "structured permanent error must not retry");
        let error = events.into_iter().find_map(Result::err).unwrap();
        assert!(error.to_string().contains("invalid_request_error"));
    }
}

#[test]
fn chat_completions_sse_websocket_connection_limit_retries_and_rolls_back() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let mut requests = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        'requests: while !server_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    if headers.starts_with("get / http/") {
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        continue 'requests;
                    }
                    assert!(headers.starts_with("post /v1/chat/completions "), "{headers}");
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests += 1;
            let body = if requests == 1 {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                    "data: {\"error\":{\"message\":\"Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.\"}}\n\n",
                )
            } else {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                )
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            if requests == 2 {
                break;
            }
        }
        requests
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(run_stream_with_retries(
            reqwest::Client::new(),
            base,
            ProviderAuth::None {
                label: "test".into(),
            },
            false,
            "chat-ws-limit-retry-test".into(),
            serde_json::json!({"model":"gpt-6.2-sol", "messages":[], "stream":true}),
            false,
            tx,
            Arc::new(Mutex::new(None)),
            "gpt-6.2-sol".into(),
            false,
        ));
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        })
        .await;
        task.abort();
        result
    });
    stop.store(true, Ordering::SeqCst);
    let requests = server.join().unwrap();
    assert_eq!(requests, 2, "websocket 60-minute limit SSE error must retry");
    let events: Vec<_> = result
        .expect("chat SSE retry must finish")
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let rollback = events
        .iter()
        .position(|event| matches!(event, StreamEvent::RetryRollback { .. }))
        .expect("partial output must be rolled back before replay");
    assert!(
        events[..rollback]
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "partial"))
    );
    assert!(
        events[rollback + 1..]
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "recovered"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error { .. }))
    );
}
