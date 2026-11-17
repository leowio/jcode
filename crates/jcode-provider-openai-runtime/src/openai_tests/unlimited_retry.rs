// Exercise the public provider stream against a loopback server so a finite
// transport budget cannot silently defeat the configured unlimited setting.
async fn retry_policy_public_case(
    max_attempts: u32,
    failures: u32,
    permanent: bool,
    permanent_sse: bool,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _budget = EnvVarGuard::set("JCODE_MAX_RETRIES", &max_attempts.to_string());
    let _backoff = EnvVarGuard::set("JCODE_RETRY_BACKOFF_CAP_SECS", "1");
    jcode_base::config::invalidate_config_cache();
    let expected_attempts = if permanent {
        1
    } else {
        max_attempts.min(failures + 1)
    };
    let server = tokio::spawn(async move {
        let mut attempts = 0;
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = tcp.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")?
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            if data.starts_with(b"GET /v1/models ") {
                tcp.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}").await.unwrap();
                continue;
            }
            if data.starts_with(b"GET /v1/responses ") {
                // A background WebSocket prewarm can race the HTTPS fixture.
                tcp.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                continue;
            }
            assert!(
                data.starts_with(b"POST /v1/responses "),
                "{}",
                String::from_utf8_lossy(&data[..data.len().min(200)])
            );
            attempts += 1;
            if permanent {
                // A permanent error must win even when its message also names
                // a retryable transport symptom or status.
                let body = r#"{"error":{"type":"invalid_request_error","message":"invalid schema; timeout after service unavailable"}}"#;
                if permanent_sse {
                    let body = format!(
                        "data: {{\"type\":\"error\",\"error\":{{\"type\":\"invalid_request_error\",\"message\":\"invalid schema; timeout after service unavailable\"}}}}\n\n"
                    );
                    tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                } else {
                    tcp.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            } else if attempts <= failures {
                let body = r#"{"error":{"type":"service_unavailable_error","message":"server is overloaded"}}"#;
                tcp.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            } else {
                let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
                tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            if attempts == expected_attempts {
                return attempts;
            }
        }
    });
    let provider = OpenAIProvider::new(prewarm_test_credentials());
    provider.set_transport("https").unwrap();
    *provider.model.write().await = "gpt-5.6-sol".into();
    let mut stream = provider
        .complete(&[ChatMessage::user("fixture")], &[], "fixture", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut errors = Vec::new();
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(StreamEvent::Error { message, .. }) => errors.push(message),
                Err(error) => errors.push(error.to_string()),
                _ => {}
            }
        }
        assert_eq!(server.await.unwrap(), expected_attempts);
        if permanent || max_attempts <= failures {
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(text.is_empty());
        } else {
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(text, "recovered");
        }
    })
    .await
    .expect("configured retry loop did not terminate as expected");
}

#[tokio::test]
async fn retry_policy_public_unlimited_survives_ten_transient_failures() {
    retry_policy_public_case(u32::MAX, 10, false, false).await;
}

#[tokio::test]
async fn retry_policy_public_finite_budget_is_respected() {
    retry_policy_public_case(2, 10, false, false).await;
}

#[tokio::test]
async fn retry_policy_public_permanent_failure_stops_unlimited_retries() {
    retry_policy_public_case(u32::MAX, 0, true, false).await;
}

#[test]
fn retry_policy_stream_hint_survives_error_conversion() {
    let error = super::openai_stream_runtime::stream_event_error("rate_limit_exceeded", Some(7));
    let delay = jcode_provider_core::retry_after::retry_after_from_error(&error).unwrap();
    assert!(delay <= Duration::from_secs(7) && delay > Duration::from_secs(6));
    let error = super::openai_stream_runtime::stream_event_error("server_error", Some(0));
    assert_eq!(
        jcode_provider_core::retry_after::retry_after_from_error(&error),
        Some(Duration::ZERO)
    );
}

#[test]
fn retry_policy_classification_preserves_permanent_errors_and_refresh_transport_failures() {
    for error in [
        "openai api error 400 bad request: timeout",
        "http 401: server overloaded",
        "http status 403: network timeout",
        "status: 404: service unavailable",
        "insufficient_quota: rate limit exceeded",
        "openai token refresh failed: invalid_grant; connection reset",
    ] {
        assert!(!is_retryable_error(error), "{error}");
    }
    for error in [
        "openai token refresh failed: connection reset by peer",
        "openai token refresh failed: 503 service unavailable",
        "http 503: service_unavailable",
    ] {
        assert!(is_retryable_error(error), "{error}");
    }
}

#[tokio::test]
async fn retry_policy_public_cancels_during_server_requested_backoff() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _budget = EnvVarGuard::set("JCODE_MAX_RETRIES", &u32::MAX.to_string());
    jcode_base::config::invalidate_config_cache();
    let server = tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut data = vec![0; 65536];
            let n = tcp.read(&mut data).await.unwrap();
            if data[..n].starts_with(b"GET /v1/models ") {
                tcp.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}").await.unwrap();
                continue;
            }
            let body = r#"{"error":{"type":"server_error","message":"server overloaded"}}"#;
            tcp.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 30\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            break;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "cancelled retry sent another request"
        );
    });
    let provider = OpenAIProvider::new(prewarm_test_credentials());
    provider.set_transport("https").unwrap();
    *provider.model.write().await = "gpt-5.6-sol".into();
    let mut stream = provider
        .complete(&[ChatMessage::user("fixture")], &[], "fixture", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::ConnectionPhase {
                    phase: jcode_message_types::ConnectionPhase::Retrying { .. },
                } => break,
                StreamEvent::Error { message, .. } => {
                    panic!("unexpected terminal failure: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("retry did not enter backoff");
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn retry_policy_opaque_https_transient_failure_returns_to_retry_owner() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut request = [0; 65536];
        assert!(tcp.read(&mut request).await.unwrap() > 0);
        let body = r#"{"error":{"type":"server_error","message":"server overloaded"}}"#;
        tcp.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 7\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let (tx, mut rx) = mpsc::channel(100);
    let error = super::openai_stream_runtime::recover_opaque_ws_rejection(
        Arc::new(RwLock::new(prewarm_test_credentials())),
        None,
        &serde_json::json!({"model":"gpt-5.6-sol","input":[],"stream":true}),
        &tx,
    )
    .await
    .unwrap_err();
    assert!(is_retryable_error(
        &format!("{error:#}").to_ascii_lowercase()
    ));
    assert!(jcode_provider_core::retry_after::retry_after_from_error(&error).is_some());
    drop(tx);
    while let Some(event) = rx.recv().await {
        assert!(
            !matches!(event.unwrap(), StreamEvent::Error { .. }),
            "transient recovery must not emit a terminal event"
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn retry_policy_public_permanent_sse_error_does_not_retry_its_eof() {
    retry_policy_public_case(u32::MAX, 0, true, true).await;
}
