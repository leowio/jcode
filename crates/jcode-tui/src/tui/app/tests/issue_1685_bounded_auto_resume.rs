// Retryable provider failures remain held until success or explicit cancellation.

fn issue_1685_user_turn(
    content: &str,
    auto_retry: bool,
    retry_attempts: u32,
) -> PendingRemoteMessage {
    PendingRemoteMessage {
        content: content.to_string(),
        images: vec![],
        is_system: false,
        system_reminder: None,
        auto_retry,
        retry_attempts,
        retry_at: None,
    }
}

fn issue_1685_error(
    id: u64,
    message: &str,
    retry_after_secs: Option<u64>,
) -> crate::protocol::ServerEvent {
    crate::protocol::ServerEvent::Error {
        id,
        message: message.to_string(),
        retry_after_secs,
    }
}

#[test]
fn issue_1685_openference_json_rate_limit_uses_retry_after_seconds_not_max_rpm() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue the adapter", false, 0));
    app.is_processing = true;
    app.current_message_id = Some(41);

    app.handle_server_event(
        issue_1685_error(
            41,
            "OpenAI-compatible chat request failed\n  status: 429 Too Many Requests\n  response: {\"error\":\"Rate limit exceeded. Too many requests per minute.\",\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\",\"retry_after_seconds\":2,\"max_rpm\":25}",
            None,
        ),
        &mut remote,
    );

    assert!(!app.is_processing);
    let held = app.rate_limit_pending_message.as_ref().expect("turn held");
    assert_eq!(held.retry_attempts, 1);
    let wait = app
        .rate_limit_reset
        .expect("retry scheduled")
        .saturating_duration_since(std::time::Instant::now());
    assert!(wait <= std::time::Duration::from_secs(2), "{wait:?}");
    assert!(wait > std::time::Duration::from_secs(1), "{wait:?}");
}

#[test]
fn rate_limit_resumes_continue_beyond_old_attempt_limit() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue the adapter", false, 0));
    for attempt in 1..=300_u32 {
        app.is_processing = true;
        app.handle_server_event(
            issue_1685_error(
                u64::from(attempt),
                "429 Too Many Requests: retry after 30 seconds",
                None,
            ),
            &mut remote,
        );
        assert_eq!(
            app.rate_limit_pending_message
                .as_ref()
                .unwrap()
                .retry_attempts,
            attempt
        );
        assert!(app.rate_limit_reset.is_some());
        assert!(!app.is_processing);
    }
    crate::tui::app::commands::handle_cancel_command(&mut app, "/cancel");
    assert!(app.rate_limit_pending_message.is_none());
    assert!(app.rate_limit_reset.is_none());
}

#[test]
fn provider_dns_resumes_keep_bounded_delay_beyond_old_limit() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue the adapter", false, 0));
    let error = "Failed to send request to OpenAI API: client error (Connect): dns error";
    for attempt in 1..=300_u32 {
        app.is_processing = true;
        app.handle_server_event(
            issue_1685_error(u64::from(attempt), error, None),
            &mut remote,
        );
        let pending = app.rate_limit_pending_message.as_ref().unwrap();
        assert_eq!(pending.retry_attempts, attempt);
        assert!(pending.auto_retry);
        assert!(matches!(
            app.status,
            ProcessingStatus::WaitingForNetwork { .. }
        ));
        assert!(
            app.rate_limit_reset
                .unwrap()
                .saturating_duration_since(std::time::Instant::now())
                <= std::time::Duration::from_secs(App::AUTO_RETRY_BACKOFF_CAP_SECS)
        );
    }
}

#[test]
fn remote_retry_attempt_counter_saturates_without_stopping() {
    let mut app = create_test_app();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, u32::MAX));
    for _ in 0..2 {
        assert!(app.schedule_pending_remote_retry("temporary server failure"));
        assert_eq!(
            app.rate_limit_pending_message
                .as_ref()
                .unwrap()
                .retry_attempts,
            u32::MAX
        );
        assert!(
            app.rate_limit_reset
                .unwrap()
                .saturating_duration_since(std::time::Instant::now())
                <= std::time::Duration::from_secs(App::AUTO_RETRY_BACKOFF_CAP_SECS)
        );
    }
    assert!(
        !app.display_messages()
            .iter()
            .any(|m| m.content.contains("/0"))
    );
}

/// A Wi-Fi roam breaks the HTTP/2 stream with no timeout or reset wording.
/// The turn must be held for an automatic network retry, not ended with an
/// offer to switch models (every model shares the same broken network).
#[test]
fn wifi_roam_http2_stream_error_waits_for_network_instead_of_offering_model_switch() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("keep going", false, 0));
    app.last_submitted_input = Some("keep going".to_string());
    app.is_processing = true;

    app.handle_server_event(
        issue_1685_error(
            7,
            "Network unstable: 6 attempts over 31s all failed at the transport level: Failed to send request to Anthropic API: error sending request for url (https://api.anthropic.com/v1/messages?beta=true): client error (SendRequest): http2 error: stream error received: unspecific protocol error detected",
            None,
        ),
        &mut remote,
    );

    assert!(matches!(
        app.status,
        ProcessingStatus::WaitingForNetwork { .. }
    ));
    let pending = app.rate_limit_pending_message.as_ref().expect("turn held");
    assert_eq!(pending.retry_attempts, 1);
    assert!(pending.auto_retry);
    assert!(app.pending_fallback_offer.is_none());
}

#[test]
fn retryable_provider_errors_retain_user_turn_without_auto_poke() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    for error in [
        "OpenAI API error 500 Internal Server Error: server_error",
        "Anthropic API error 529: overloaded_error",
        "Gemini request failed with status 503 Service Unavailable",
        "Gemini request failed with status 503: billing service temporarily unavailable",
        "OpenAI API error 507: exceeded request buffer limit while retrying upstream",
    ] {
        let mut app = create_test_app();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.auto_poke_incomplete_todos = false;
        app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", false, 100));
        app.is_processing = true;
        app.handle_server_event(issue_1685_error(42, error, None), &mut remote);
        let pending = app
            .rate_limit_pending_message
            .as_ref()
            .expect("retryable user intent retained");
        assert!(pending.auto_retry, "{error}");
        assert_eq!(pending.retry_attempts, 101);
        assert!(app.rate_limit_reset.is_some());
    }
}

#[test]
fn permanent_http_status_overrides_retryable_words_in_error_body() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    for error in [
        "OpenAI API error 401 Unauthorized: request timeout; retry after 30 seconds",
        "Anthropic API error 403 Forbidden: connection reset",
        "OpenAI API error 400 Bad Request: response mentions status 503 Service Unavailable",
    ] {
        let mut app = create_test_app();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.last_submitted_input = Some("keep going".into());
        app.rate_limit_pending_message = Some(issue_1685_user_turn("keep going", true, 100));
        app.is_processing = true;
        app.handle_server_event(issue_1685_error(42, error, None), &mut remote);
        assert!(!app.is_processing, "{error}");
        assert!(app.rate_limit_pending_message.is_none(), "{error}");
        assert!(app.rate_limit_reset.is_none(), "{error}");
        assert_eq!(app.input, "keep going", "{error}");
    }
}

#[test]
fn unsafe_external_agent_outcomes_do_not_replay_even_without_text() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    for (provider, error) in [
        (
            "openai",
            "HTTP 503 Service Unavailable: unsafe to replay accepted operation",
        ),
        (
            "openai",
            "HTTP 503 Service Unavailable: Do not retry this operation",
        ),
        (
            "cursor",
            "Cursor agent stream timed out waiting for meaningful progress",
        ),
        ("cursor", "connection reset"),
    ] {
        let mut app = create_test_app();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.remote_provider_name = Some(provider.into());
        app.last_submitted_input = Some("run the tool".into());
        app.rate_limit_pending_message = Some(issue_1685_user_turn("run the tool", true, 100));
        app.is_processing = true;
        app.handle_server_event(issue_1685_error(42, error, None), &mut remote);
        assert!(app.rate_limit_pending_message.is_none(), "{error}");
        assert!(app.rate_limit_reset.is_none(), "{error}");
        assert_eq!(app.input, "run the tool");
    }
}

#[test]
fn cursor_pre_dispatch_connect_failure_keeps_retrying() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut app = create_test_app();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.remote_provider_name = Some("cursor".into());
    app.rate_limit_pending_message = Some(issue_1685_user_turn("run the tool", false, 100));
    app.is_processing = true;
    app.handle_server_event(
        issue_1685_error(
            42,
            "Failed to connect to agent.example:443: dns error",
            None,
        ),
        &mut remote,
    );
    assert_eq!(
        app.rate_limit_pending_message
            .as_ref()
            .unwrap()
            .retry_attempts,
        101
    );
    assert!(app.rate_limit_reset.is_some());
}

#[test]
fn issue_1685_offline_probes_do_not_consume_retry_budget() {
    let mut app = create_test_app();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, 2));
    for _ in 0..10 {
        assert!(app.schedule_pending_remote_network_wait("network probe still failing"));
    }
    assert_eq!(
        app.rate_limit_pending_message
            .as_ref()
            .unwrap()
            .retry_attempts,
        2
    );
}

#[test]
fn accepted_stream_timeouts_keep_retryable_turn_held() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    for error in [
        "Stream read timeout: no data received for 180 seconds",
        "WebSocket stream timed out waiting for next websocket activity (1s)",
        "timed out waiting for next websocket activity on persistent WS (180s)",
    ] {
        for auto_retry in [false, true] {
            let mut app = create_test_app();
            let mut remote = crate::tui::backend::RemoteConnection::dummy();
            app.rate_limit_pending_message =
                Some(issue_1685_user_turn("keep going", auto_retry, 500));
            app.is_processing = true;
            app.handle_server_event(issue_1685_error(42, error, None), &mut remote);
            assert!(!app.is_processing, "{error}");
            let pending = app
                .rate_limit_pending_message
                .as_ref()
                .expect("retryable stream held");
            assert!(pending.auto_retry);
            assert_eq!(pending.retry_attempts, 501);
            assert!(app.rate_limit_reset.is_some());
        }
    }
}

#[test]
fn issue_1685_huge_server_retry_hint_is_clamped() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", false, 0));
    app.handle_server_event(
        issue_1685_error(7, "rate limited", Some(u64::MAX)),
        &mut remote,
    );
    let wait = app
        .rate_limit_reset
        .expect("retry scheduled")
        .saturating_duration_since(std::time::Instant::now());
    assert!(wait <= std::time::Duration::from_secs(24 * 60 * 60));
    assert_eq!(
        app.rate_limit_pending_message
            .as_ref()
            .unwrap()
            .retry_attempts,
        1
    );
}
