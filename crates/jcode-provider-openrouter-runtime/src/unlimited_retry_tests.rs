//! Exercise the real HTTP request loop past the former retry budgets.
use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct RetryConfig {
    retries: Option<std::ffi::OsString>,
    cap: Option<std::ffi::OsString>,
}

impl RetryConfig {
    fn new(attempts: u32) -> Self {
        let config = Self {
            retries: std::env::var_os("JCODE_MAX_RETRIES"),
            cap: std::env::var_os("JCODE_RETRY_BACKOFF_CAP_SECS"),
        };
        jcode_base::env::set_var("JCODE_MAX_RETRIES", attempts.to_string());
        jcode_base::env::set_var("JCODE_RETRY_BACKOFF_CAP_SECS", "1");
        jcode_base::config::invalidate_config_cache();
        config
    }
}

impl Drop for RetryConfig {
    fn drop(&mut self) {
        for (key, value) in [
            ("JCODE_MAX_RETRIES", self.retries.take()),
            ("JCODE_RETRY_BACKOFF_CAP_SECS", self.cap.take()),
        ] {
            match value {
                Some(value) => jcode_base::env::set_var(key, value),
                None => jcode_base::env::remove_var(key),
            }
        }
        jcode_base::config::invalidate_config_cache();
    }
}

struct Server {
    base: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new(failures: usize, status: u16, retry_after: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = socket.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length: ")?.parse::<usize>().ok()
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt < failures {
                    (
                        status,
                        r#"{"error":{"message":"synthetic timeout and overloaded failure"}}"#,
                    )
                } else {
                    (
                        200,
                        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nRetry-After: {retry_after}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

async fn request(base: String, tx: mpsc::Sender<Result<StreamEvent>>) {
    run_stream_with_retries(
        Client::builder().no_proxy().build().unwrap(),
        base,
        ProviderAuth::None {
            label: "retry-fixture".into(),
        },
        false,
        "retry-fixture".into(),
        serde_json::json!({"model":"fixture", "messages":[], "stream":true}),
        false,
        tx,
        Arc::new(Mutex::new(None)),
        "fixture".into(),
        false,
    )
    .await;
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn unlimited_retry_recovers_after_more_than_eight_http_failures() {
    let _lock = jcode_base::storage::lock_test_env();
    let _config = RetryConfig::new(0);
    let server = Server::new(10, 503, 0);
    runtime().block_on(async {
        let (tx, mut rx) = mpsc::channel(128);
        tokio::time::timeout(Duration::from_secs(5), request(server.base.clone(), tx))
            .await
            .unwrap();
        let mut recovered = false;
        while let Some(event) = rx.recv().await {
            match event.unwrap() {
                StreamEvent::TextDelta(text) => recovered |= text == "recovered",
                _ => {}
            }
        }
        assert!(recovered);
    });
    assert_eq!(server.requests.load(Ordering::SeqCst), 11);
}

#[test]
fn explicit_retry_limit_and_permanent_failure_still_terminate() {
    let _lock = jcode_base::storage::lock_test_env();
    for (attempts, status, expected) in [(3, 503, 3), (0, 401, 1)] {
        let _config = RetryConfig::new(attempts);
        let server = Server::new(usize::MAX, status, 0);
        runtime().block_on(async {
            let (tx, mut rx) = mpsc::channel(128);
            tokio::time::timeout(Duration::from_secs(5), request(server.base.clone(), tx))
                .await
                .unwrap();
            let mut error = false;
            while let Some(event) = rx.recv().await {
                error |= event.is_err();
            }
            assert!(error);
        });
        assert_eq!(server.requests.load(Ordering::SeqCst), expected);
    }
}

#[test]
fn consumer_cancellation_interrupts_long_retry_after_wait() {
    let _lock = jcode_base::storage::lock_test_env();
    let _config = RetryConfig::new(0);
    let server = Server::new(usize::MAX, 429, 60);
    runtime().block_on(async {
        let (tx, rx) = mpsc::channel(128);
        let task = tokio::spawn(request(server.base.clone(), tx));
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.requests.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(rx);
        tokio::time::timeout(Duration::from_millis(300), task)
            .await
            .unwrap()
            .unwrap();
    });
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}
