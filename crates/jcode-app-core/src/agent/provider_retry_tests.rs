use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

enum Step {
    InitialError(&'static str),
    Stream(Vec<Result<StreamEvent, &'static str>>),
}

#[derive(Clone)]
struct MockProvider {
    name: &'static str,
    steps: Arc<Mutex<VecDeque<Step>>>,
    attempts: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl MockProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            name: "mock-without-native-retry",
            steps: Arc::new(Mutex::new(steps.into())),
            attempts: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system_static: &str,
        system_dynamic: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        // Inspect the adapter's exact call boundary, without the trait default
        // creating a newly timestamped dynamic context message on every call.
        self.complete(
            messages,
            tools,
            &format!("{system_static}\n{system_dynamic}"),
            resume_session_id,
        )
        .await
    }
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(serde_json::json!({
            "messages": messages, "tools": tools, "system": system, "resume": resume_session_id,
        }));
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected retry");
        match step {
            Step::InitialError(message) => anyhow::bail!(message),
            Step::Stream(events) => Ok(Box::pin(futures::stream::iter(
                events
                    .into_iter()
                    .map(|event| event.map_err(anyhow::Error::msg)),
            ))),
        }
    }

    fn name(&self) -> &str {
        self.name
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn start(provider: Arc<dyn Provider>, policy: RetryPolicy) -> EventStream {
    retrying_stream(
        provider,
        vec![Message::user("original user request")],
        vec![],
        "static system".into(),
        "dynamic system".into(),
        Some("existing-provider-session".into()),
        policy,
    )
}

fn unlimited() -> RetryPolicy {
    RetryPolicy::new(0, Duration::from_millis(1))
}

#[tokio::test]
async fn provider_retry_zero_setting_enables_generic_runtime_recovery() {
    let _env_lock = crate::storage::lock_test_env();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_MAX_RETRIES", value),
                None => crate::env::remove_var("JCODE_MAX_RETRIES"),
            }
            crate::config::invalidate_config_cache();
        }
    }
    let _restore = Restore(std::env::var_os("JCODE_MAX_RETRIES"));
    crate::env::set_var("JCODE_MAX_RETRIES", "0");
    crate::config::invalidate_config_cache();
    let provider = Arc::new(MockProvider::new(vec![
        Step::InitialError("503 service unavailable"),
        Step::Stream(vec![Ok(StreamEvent::TextDelta("recovered".into()))]),
    ]));
    let stream = complete_split(
        provider.clone(),
        &[Message::user("request")],
        &[],
        "",
        "",
        None,
    )
    .await
    .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("zero budget must recover rather than propagate the first failure");
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 2);
    assert!(events.iter().all(Result::is_ok));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::TextDelta(text)) if text == "recovered"))
    );
}

#[tokio::test]
async fn provider_retry_finite_setting_does_not_add_an_outer_retry_budget() {
    let _env_lock = crate::storage::lock_test_env();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_MAX_RETRIES", value),
                None => crate::env::remove_var("JCODE_MAX_RETRIES"),
            }
            crate::config::invalidate_config_cache();
        }
    }
    let _restore = Restore(std::env::var_os("JCODE_MAX_RETRIES"));
    crate::env::set_var("JCODE_MAX_RETRIES", "2");
    crate::config::invalidate_config_cache();
    let provider = Arc::new(MockProvider::new(vec![Step::InitialError(
        "503 service unavailable",
    )]));
    let result = complete_split(
        provider.clone(),
        &[Message::user("request")],
        &[],
        "system",
        "",
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "finite provider failure must propagate directly"
    );
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_retry_unlimited_recovers_after_nine_initial_failures_with_identical_input() {
    let mut steps: Vec<_> = (0..9)
        .map(|_| Step::InitialError("503 service unavailable"))
        .collect();
    steps.push(Step::Stream(vec![Ok(StreamEvent::TextDelta(
        "recovered".into(),
    ))]));
    let provider = Arc::new(MockProvider::new(steps));
    let events = start(provider.clone(), unlimited())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 10);
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, Ok(StreamEvent::TextDelta(text)) if text == "recovered")
            )
            .count(),
        1
    );
    let requests = provider.requests.lock().unwrap();
    assert!(requests.iter().all(|request| request == &requests[0]));
}

#[tokio::test]
async fn provider_retry_rolls_back_partial_tool_definitions_and_hides_transient_errors() {
    let failed = || {
        Step::Stream(vec![
            Ok(StreamEvent::TextDelta("discarded".into())),
            Ok(StreamEvent::ToolUseStart {
                id: "discarded-call".into(),
                name: "read".into(),
            }),
            Ok(StreamEvent::ToolInputDelta("{}".into())),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::Error {
                message: "server_error: overloaded".into(),
                retry_after_secs: Some(0),
            }),
        ])
    };
    let mut steps: Vec<_> = (0..9).map(|_| failed()).collect();
    steps.push(Step::Stream(vec![
        Ok(StreamEvent::ToolUseStart {
            id: "final-call".into(),
            name: "read".into(),
        }),
        Ok(StreamEvent::ToolInputDelta("{}".into())),
        Ok(StreamEvent::ToolUseEnd),
        Ok(StreamEvent::TextDelta("final".into())),
    ]));
    let provider = Arc::new(MockProvider::new(steps));
    let mut stream = start(provider.clone(), unlimited());
    let (mut text, mut calls, mut rollbacks) = (String::new(), Vec::new(), 0);
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::ToolUseStart { id, .. } => calls.push(id),
            StreamEvent::RetryRollback { .. } => {
                text.clear();
                calls.clear();
                rollbacks += 1;
            }
            StreamEvent::Error { message, .. } => panic!("transient event escaped: {message}"),
            _ => {}
        }
    }
    assert_eq!(rollbacks, 9);
    assert_eq!(text, "final");
    assert_eq!(calls, ["final-call"]);
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 10);
}

#[tokio::test]
async fn provider_retry_permanent_errors_stop_even_with_transport_words_or_retry_hint() {
    for message in [
        "401 Unauthorized: connection reset",
        "invalid_request_error: timeout parameter is invalid",
        "429 insufficient_quota",
        "[jcode-provider-failover]503 unavailable",
        "unsafe_to_replay: connection reset",
    ] {
        let provider = Arc::new(MockProvider::new(vec![Step::Stream(vec![Ok(
            StreamEvent::Error {
                message: message.into(),
                retry_after_secs: Some(0),
            },
        )])]));
        let events = start(provider.clone(), unlimited())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 1, "{message}");
        assert!(
            matches!(events.last(), Some(Ok(StreamEvent::Error { message: actual, .. })) if actual == message)
        );
    }
}

#[tokio::test]
async fn provider_retry_does_not_replay_live_native_tool_execution() {
    for live in [
        StreamEvent::NativeToolCall {
            request_id: "native-call".into(),
            tool_name: "write".into(),
            input: serde_json::json!({}),
        },
        StreamEvent::ToolResult {
            tool_use_id: "executed-call".into(),
            content: "already wrote file".into(),
            is_error: false,
        },
    ] {
        let provider = Arc::new(MockProvider::new(vec![Step::Stream(vec![
            Ok(live),
            Err("connection reset"),
        ])]));
        let events = start(provider.clone(), unlimited())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
        assert!(
            matches!(events.last(), Some(Err(error)) if error.to_string().contains("unsafe_to_replay"))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::RetryRollback { .. })))
        );
    }
}

#[tokio::test]
async fn provider_retry_preserves_server_delay_and_cancels_backoff_on_stream_drop() {
    let provider = Arc::new(MockProvider::new(vec![Step::Stream(vec![Ok(
        StreamEvent::Error {
            message: "429 too many requests".into(),
            retry_after_secs: Some(60),
        },
    )])]));
    let mut stream = start(provider.clone(), unlimited());
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::Retrying { .. }
        }))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stream.next())
            .await
            .is_err()
    );
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
}

struct DropProbe(Arc<AtomicBool>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct PendingProvider(Arc<AtomicBool>);

#[async_trait::async_trait]
impl Provider for PendingProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        let _guard = DropProbe(self.0.clone());
        std::future::pending().await
    }
    fn name(&self) -> &str {
        "pending"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self(self.0.clone()))
    }
}

#[tokio::test]
async fn provider_retry_cancels_in_flight_request_on_stream_drop() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut stream = start(Arc::new(PendingProvider(dropped.clone())), unlimited());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stream.next())
            .await
            .is_err()
    );
    assert!(!dropped.load(Ordering::SeqCst));
    drop(stream);
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn provider_retry_classifier_recognizes_remaining_native_runtimes_without_false_status_matches() {
    for message in [
        "ThrottlingException",
        "ServiceUnavailableException",
        "HTTP 503",
        "gRPC UNAVAILABLE: connection reset",
        "HTTP2 stream error",
        "token refresh failed: connection reset",
        "authentication refresh failed: HTTP 503",
    ] {
        assert!(is_retryable(message), "{message}");
    }
    for message in [
        "request_id=req_15030",
        "ValidationException",
        "AccessDeniedException",
        "unknown error",
        "cancelled",
        "HTTP 400: timeout parameter is invalid",
        "HTTP 422: validation request timed out",
        "invalid_grant: connection reset",
    ] {
        assert!(!is_retryable(message), "{message}");
    }
}

#[test]
fn provider_retry_clamps_untrusted_stream_retry_after_before_sleeping() {
    let hint = Failure::Event {
        message: "429 too many requests".into(),
        retry_after_secs: Some(u64::MAX),
    }
    .retry_after(unlimited())
    .unwrap();
    assert!(hint <= jcode_provider_core::retry_after::MAX_RETRY_AFTER);
}

#[tokio::test]
async fn provider_retry_does_not_replay_opaque_sdk_output_before_tool_result_arrives() {
    for name in ["cursor", "grok-build", "Jcode Subscription"] {
        for output in [
            StreamEvent::ToolUseStart {
                id: "may-have-executed".into(),
                name: "write".into(),
            },
            StreamEvent::TextDelta("SDK progress before its tool result".into()),
        ] {
            let mut provider = MockProvider::new(vec![Step::Stream(vec![
                Ok(output),
                Err("connection reset"),
            ])]);
            provider.name = name;
            let provider = Arc::new(provider);
            let events = start(provider.clone(), unlimited())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(provider.attempts.load(Ordering::SeqCst), 1, "{name}");
            assert!(
                matches!(events.last(), Some(Err(error)) if error.to_string().contains("unsafe_to_replay")),
                "{name}"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::RetryRollback { .. }))),
                "{name}"
            );
        }
    }
}

#[test]
fn provider_retry_preserves_long_valid_hints_in_unlimited_mode() {
    let event = Failure::Event {
        message: "429 too many requests".into(),
        retry_after_secs: Some(3600),
    };
    let hint = event.retry_after(unlimited()).unwrap();
    assert!(hint > Duration::from_secs(3599));
    assert!(hint <= Duration::from_secs(3600));
    let finite = RetryPolicy::new(3, Duration::from_secs(30));
    assert!(
        event.retry_after(finite).unwrap() <= jcode_provider_core::retry_after::MAX_RETRY_AFTER
    );

    let error = Failure::Error(jcode_provider_core::retry_after::error_with_retry_after(
        "429 too many requests".into(),
        Some(jcode_provider_core::retry_after::RetryAfter::from_delay(
            Duration::from_secs(3600),
        )),
    ));
    assert!(error.retry_after(unlimited()).unwrap() > Duration::from_secs(3599));
    assert!(
        error.retry_after(finite).unwrap() <= jcode_provider_core::retry_after::MAX_RETRY_AFTER
    );
}
