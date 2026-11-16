use super::*;
use std::io::{Read, Write};
use std::time::Duration;

struct RestoreEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for RestoreEnvironment {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => jcode_base::env::set_var(name, value),
                None => jcode_base::env::remove_var(name),
            }
        }
        jcode_base::config::invalidate_config_cache();
    }
}

fn run_case(max_attempts: u32, failures: usize, permanent: bool, cancel_backoff: bool) -> usize {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let names = [
        "JCODE_HOME",
        "JCODE_MAX_RETRIES",
        "JCODE_ANTHROPIC_API_BASE",
    ];
    let _restore = RestoreEnvironment(
        names
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect(),
    );
    jcode_base::env::set_var("JCODE_HOME", home.path());
    jcode_base::env::set_var("JCODE_MAX_RETRIES", max_attempts.to_string());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    jcode_base::env::set_var(
        "JCODE_ANTHROPIC_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    jcode_base::config::invalidate_config_cache();
    let provider = AnthropicProvider::new();
    let settings = reasoning_request::RetrySettings::from_provider(&provider);
    let transport = DirectTransportConfig::from_env();
    let expected_attempts = if permanent || cancel_backoff {
        1
    } else if max_attempts == u32::MAX {
        failures + 1
    } else {
        (max_attempts as usize).min(failures + 1)
    };
    let server = std::thread::spawn(move || {
        for attempt in 0..expected_attempts {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    assert!(headers.starts_with("POST /messages "));
                    let size = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")?
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            if permanent {
                let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid request: overloaded"}}"#;
                write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            } else if attempt < failures {
                let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
                let wait = if cancel_backoff { 3600 } else { 0 };
                write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: {wait}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            } else {
                let body = concat!(
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"recovered\"}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }
        expected_attempts
    });
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let (tx, mut rx) = mpsc::channel(64);
            let task = tokio::spawn(run_stream_with_retries(
                Client::builder().no_proxy().build().unwrap(),
                "synthetic-token".into(),
                false,
                ApiRequest {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: 1024,
                    system: None,
                    messages: vec![],
                    tools: None,
                    metadata: None,
                    thinking: None,
                    output_config: None,
                    temperature: None,
                    service_tier: None,
                    stream: true,
                },
                tx,
                Arc::new(RwLock::new(None)),
                None,
                "claude-sonnet-4-6".into(),
                "retry-fixture".into(),
                Arc::new(std::sync::RwLock::new("claude-sonnet-4-6".into())),
                transport,
                settings,
            ));
            tokio::time::timeout(Duration::from_secs(5), async move {
                let mut errors = Vec::new();
                let mut text = String::new();
                let mut saw_backoff = false;
                while let Some(event) = rx.recv().await {
                    match event {
                        Ok(StreamEvent::ConnectionPhase {
                            phase: jcode_message_types::ConnectionPhase::Retrying { .. },
                        }) if cancel_backoff => {
                            saw_backoff = true;
                            break;
                        }
                        Ok(StreamEvent::TextDelta(delta)) => text.push_str(&delta),
                        Err(error) => errors.push(error.to_string()),
                        _ => {}
                    }
                }
                drop(rx);
                tokio::time::timeout(Duration::from_millis(500), task)
                    .await
                    .expect("consumer cancellation must interrupt long backoff")
                    .unwrap();
                if cancel_backoff {
                    assert!(saw_backoff);
                } else if expected_attempts > failures {
                    assert!(errors.is_empty(), "{errors:?}");
                    assert_eq!(text, "recovered");
                } else {
                    assert_eq!(errors.len(), 1, "{errors:?}");
                }
                server.join().unwrap()
            })
            .await
            .expect("Anthropic configured retry fixture stalled")
        })
}

#[test]
fn unlimited_retries_outlast_legacy_and_default_budgets() {
    assert_eq!(run_case(u32::MAX, 10, false, false), 11);
}

#[test]
fn finite_retry_budget_is_honored() {
    assert_eq!(run_case(2, 10, false, false), 2);
}

#[test]
fn unlimited_retry_stops_on_permanent_error() {
    assert_eq!(run_case(u32::MAX, 10, true, false), 1);
}

#[test]
fn unlimited_retry_cancellation_interrupts_server_requested_backoff() {
    assert_eq!(run_case(u32::MAX, 10, false, true), 1);
}
