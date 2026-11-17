#[derive(Clone, Copy)]
enum OpenAIRetryCase {
    RecoverAfterTenFailures,
    CancelAfterTenFailures,
    FiniteBudget,
    InvalidRequest,
    Unauthorized,
    RejectedRefreshedCredentials,
    CredentialsChangedBeforeSend,
}

// Exercise the public provider against real HTTP failures. Zero-delay upstream
// hints keep the regression fast while checking more than either old cap.
async fn openai_request_retry_case(case: OpenAIRetryCase) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _retries = EnvVarGuard::set(
        "JCODE_MAX_RETRIES",
        if matches!(case, OpenAIRetryCase::FiniteBudget) {
            "4"
        } else {
            "0"
        },
    );
    jcode_base::config::invalidate_config_cache();
    let (cancel_check_tx, cancel_check_rx) = tokio::sync::oneshot::channel();
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
                    let length = String::from_utf8_lossy(&data[..end])
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
            assert!(data.starts_with(b"POST /v1/responses "));
            attempts += 1;
            if matches!(case, OpenAIRetryCase::RecoverAfterTenFailures) && attempts == 11
                || matches!(case, OpenAIRetryCase::CredentialsChangedBeforeSend) && attempts == 4
            {
                let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
                tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                return attempts;
            }
            let status = if matches!(case, OpenAIRetryCase::InvalidRequest) {
                "400 Bad Request"
            } else if matches!(case, OpenAIRetryCase::Unauthorized) {
                "401 Unauthorized"
            } else if matches!(
                case,
                OpenAIRetryCase::RejectedRefreshedCredentials
                    | OpenAIRetryCase::CredentialsChangedBeforeSend
            ) {
                "503 Service Unavailable"
            } else {
                [
                    "429 Too Many Requests",
                    "500 Internal Server Error",
                    "502 Bad Gateway",
                    "503 Service Unavailable",
                    "504 Gateway Timeout",
                    "507 Insufficient Storage",
                ][(attempts - 1) % 6]
            };
            let body = if matches!(case, OpenAIRetryCase::CredentialsChangedBeforeSend) {
                r#"{"error":{"message":"openai credentials changed before request, retrying"}}"#
            } else if matches!(case, OpenAIRetryCase::RejectedRefreshedCredentials) {
                // Exercise the retry loop's one-refresh allowance without
                // contacting an OAuth token endpoint or using real credentials.
                r#"{"error":{"message":"openai token refreshed, retrying"}}"#
            } else if status.starts_with("507") {
                r#"{"error":{"message":"exceeded request buffer limit while retrying upstream"}}"#
            } else if status.starts_with("400") {
                r#"{"error":{"message":"invalid request parameters","type":"invalid_request_error"}}"#
            } else {
                r#"{"error":{"message":"temporary rate limit or server failure"}}"#
            };
            let cancelling =
                matches!(case, OpenAIRetryCase::CancelAfterTenFailures) && attempts == 11;
            let delay = if cancelling { 60 } else { 0 };
            tcp.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: {delay}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            if matches!(
                case,
                OpenAIRetryCase::InvalidRequest | OpenAIRetryCase::Unauthorized
            ) || matches!(case, OpenAIRetryCase::FiniteBudget) && attempts == 4
                || matches!(case, OpenAIRetryCase::RejectedRefreshedCredentials) && attempts == 2
            {
                return attempts;
            }
            if cancelling {
                let _ = cancel_check_rx.await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), listener.accept())
                        .await
                        .is_err(),
                    "consumer cancellation must stop the retry sleep without another request"
                );
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
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut text = String::new();
        let mut errors = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(StreamEvent::ConnectionPhase {
                    phase:
                        jcode_message_types::ConnectionPhase::Retrying {
                            attempt: 12,
                            max: 0,
                        },
                }) if matches!(case, OpenAIRetryCase::CancelAfterTenFailures) => break,
                Err(error) => errors.push(format!("{error:#}")),
                _ => {}
            }
        }
        drop(stream);
        let _ = cancel_check_tx.send(());
        let attempts = server.await.unwrap();
        match case {
            OpenAIRetryCase::RecoverAfterTenFailures => {
                assert_eq!(attempts, 11);
                assert_eq!(text, "recovered");
                assert!(errors.is_empty(), "{errors:?}");
            }
            OpenAIRetryCase::CredentialsChangedBeforeSend => {
                assert_eq!(attempts, 4);
                assert_eq!(text, "recovered");
                assert!(errors.is_empty(), "{errors:?}");
            }
            OpenAIRetryCase::CancelAfterTenFailures => {
                assert_eq!(attempts, 11);
                assert!(errors.is_empty(), "{errors:?}");
            }
            OpenAIRetryCase::FiniteBudget => {
                assert_eq!(attempts, 4);
                assert_eq!(errors.len(), 1, "{errors:?}");
            }
            OpenAIRetryCase::InvalidRequest => {
                assert_eq!(attempts, 1);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(errors[0].contains("400 Bad Request"), "{errors:?}");
            }
            OpenAIRetryCase::Unauthorized => {
                assert_eq!(attempts, 1);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(errors[0].contains("re-authenticate"), "{errors:?}");
            }
            OpenAIRetryCase::RejectedRefreshedCredentials => {
                assert_eq!(attempts, 2);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(
                    errors[0].contains("rejected refreshed credentials"),
                    "{errors:?}"
                );
            }
        }
    })
    .await
    .expect("public OpenAI retry fixture stalled");
}

#[tokio::test]
async fn openai_unlimited_retry_recovers_after_more_than_eight_failures() {
    openai_request_retry_case(OpenAIRetryCase::RecoverAfterTenFailures).await;
}

#[tokio::test]
async fn openai_unlimited_retry_stops_when_consumer_cancels_backoff() {
    openai_request_retry_case(OpenAIRetryCase::CancelAfterTenFailures).await;
}

#[tokio::test]
async fn openai_unlimited_retry_preserves_explicit_finite_budget() {
    openai_request_retry_case(OpenAIRetryCase::FiniteBudget).await;
}

#[tokio::test]
async fn openai_unlimited_retry_rejects_invalid_request_without_repeating_it() {
    openai_request_retry_case(OpenAIRetryCase::InvalidRequest).await;
}

#[test]
fn openai_unlimited_retry_classifies_upstream_buffer_and_auth_failures() {
    use super::openai_provider_impl::is_retryable_openai_request_error;
    assert!(is_retryable_openai_request_error(
        "failed to send request to openai api: connection reset by peer"
    ));
    assert!(is_retryable_openai_request_error(
        "openai api error 507 insufficient storage: exceeded request buffer limit while retrying upstream"
    ));
    assert!(!is_retryable_openai_request_error(
        "openai api error 507 insufficient storage: disk is full"
    ));
    assert!(!is_retryable_openai_request_error(
        "401 unauthorized: api_error"
    ));
    assert!(!is_retryable_openai_request_error(
        "403 forbidden: rate limit"
    ));
    assert!(!is_retryable_openai_request_error(
        "429 too many requests: insufficient_quota"
    ));
}

#[tokio::test]
async fn openai_unlimited_retry_stops_on_permanent_auth_failure() {
    openai_request_retry_case(OpenAIRetryCase::Unauthorized).await;
}

#[tokio::test]
async fn openai_unlimited_retry_stops_when_refreshed_credentials_remain_rejected() {
    openai_request_retry_case(OpenAIRetryCase::RejectedRefreshedCredentials).await;
}

#[tokio::test]
async fn openai_unlimited_retry_keeps_credentials_changed_before_send_retryable() {
    openai_request_retry_case(OpenAIRetryCase::CredentialsChangedBeforeSend).await;
}
