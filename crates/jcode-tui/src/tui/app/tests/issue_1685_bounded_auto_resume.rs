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
                <= std::time::Duration::from_secs(
                    crate::config::Config::load()
                        .provider
                        .retry_backoff_cap_secs
                        .max(1)
                )
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
                <= std::time::Duration::from_secs(
                    crate::config::Config::load()
                        .provider
                        .retry_backoff_cap_secs
                        .max(1)
                )
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
            .unwrap_or_else(|| panic!("retryable user intent retained: {error}"));
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

fn with_unlimited_remote_retry(f: impl FnOnce()) {
    // Keep legacy-alias configuration isolated from other retry configuration tests.
    let name = std::thread::current().name().unwrap().to_string();
    if std::env::var("JCODE_UNLIMITED_RETRY_TEST").as_deref() == Ok(name.as_str()) {
        with_temp_jcode_home(|| {
            let _retry_env = EnvRestoreGuard::set("JCODE_MAX_RETRIES", u32::MAX.to_string());
            crate::config::Config::invalidate_cache();
            assert!(App::auto_retry_is_unlimited());
            f();
        });
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--test-threads=1", "--nocapture"])
        .env("JCODE_UNLIMITED_RETRY_TEST", &name)
        .env("JCODE_HOME", home.path())
        .output()
        .expect("run isolated unlimited retry test");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unlimited_remote_retry_saturates_and_caps_backoff() {
    with_unlimited_remote_retry(|| {
        let mut app = create_test_app();
        app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, u32::MAX));
        for _ in 0..20 {
            assert!(app.schedule_pending_remote_retry("temporary service failure"));
            let pending = app.rate_limit_pending_message.as_ref().unwrap();
            assert_eq!(pending.retry_attempts, u32::MAX);
            let wait = pending
                .retry_at
                .unwrap()
                .saturating_duration_since(Instant::now());
            assert!(wait <= Duration::from_secs(30));
        }
        let notice = &app.display_messages().last().unwrap().content;
        assert!(notice.contains("attempt 4294967295,"), "{notice}");
        assert!(!notice.contains("4294967295/3"), "{notice}");
        assert!(!app.schedule_pending_remote_retry_with_limit("deterministic recovery", 2));
    });
}

#[test]
fn unlimited_remote_retry_holds_transient_errors_past_old_limits() {
    with_unlimited_remote_retry(|| {
        for (message, hint) in [
            ("HTTP 503 Service Unavailable", None),
            ("Stream read timeout: no data received", None),
            ("429 Too Many Requests: rate_limit_error", Some(130_000)),
            (
                "429 rate_limit_error: {\"retry_after_seconds\":130000}",
                None,
            ),
        ] {
            let mut app = create_test_app();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _guard = rt.enter();
            let mut remote = crate::tui::backend::RemoteConnection::dummy();
            app.rate_limit_pending_message =
                Some(issue_1685_user_turn("continue", false, u32::MAX));
            app.last_submitted_input = Some("continue".into());
            app.is_processing = true;
            app.handle_server_event(issue_1685_error(42, message, hint), &mut remote);
            let pending = app.rate_limit_pending_message.as_ref().expect(message);
            assert_eq!(pending.retry_attempts, u32::MAX, "{message}");
            let wait = app
                .rate_limit_reset
                .expect(message)
                .saturating_duration_since(Instant::now());
            if hint.is_some() || message.contains("retry_after_seconds") {
                assert!(wait > Duration::from_secs(129_999), "{wait:?}");
            } else {
                assert!(pending.auto_retry, "{message}");
                assert!(wait <= Duration::from_secs(30));
            }
        }
    });
}

#[test]
fn unlimited_remote_retry_stops_permanent_errors_before_network_or_rate_waits() {
    with_unlimited_remote_retry(|| {
        for message in [
            "401 Unauthorized then connection reset",
            "invalid API key: connection reset, please try again",
            "invalid_request_error: Invalid request",
            "insufficient_quota: usage_limit_reached",
            "unsafe_to_replay: connection reset after native tools",
            "an unexplained failure with no transient details",
        ] {
            let mut app = create_test_app();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _guard = rt.enter();
            let mut remote = crate::tui::backend::RemoteConnection::dummy();
            app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, 0));
            app.last_submitted_input = Some("continue".into());
            app.is_processing = true;
            let hint = (!message.starts_with("an unexplained")).then_some(1);
            app.handle_server_event(issue_1685_error(42, message, hint), &mut remote);
            assert!(app.rate_limit_pending_message.is_none(), "{message}");
            assert!(app.rate_limit_reset.is_none(), "{message}");
            assert!(!App::transient_remote_retry_error(message), "{message}");
        }
    });
}

#[test]
fn unlimited_retry_phase_roundtrips_and_cancelled_turn_stays_cancelled() {
    with_unlimited_remote_retry(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.handle_server_event(
            crate::protocol::ServerEvent::ConnectionPhase {
                phase: "retrying (12/unlimited)".into(),
            },
            &mut remote,
        );
        assert!(matches!(
            app.status,
            ProcessingStatus::Connecting(crate::message::ConnectionPhase::Retrying {
                attempt: 12,
                max: u32::MAX
            })
        ));
        app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, 0));
        assert!(app.schedule_pending_remote_retry("HTTP 503"));
        app.clear_pending_remote_retry();
        app.handle_server_event(
            issue_1685_error(42, "HTTP 503 Service Unavailable", None),
            &mut remote,
        );
        assert!(app.rate_limit_pending_message.is_none());
        assert!(app.rate_limit_reset.is_none());
    });
}

#[test]
fn finite_provider_budget_does_not_limit_transient_client_recovery() {
    with_temp_jcode_home(|| {
        write_test_config("[provider]\nmax_retries = 3\nretry_backoff_cap_secs = 7\n");
        assert!(!App::auto_retry_is_unlimited());
        let mut app = create_test_app();
        app.rate_limit_pending_message = Some(issue_1685_user_turn("continue", true, 300));
        assert!(app.schedule_pending_remote_retry("HTTP 503 Service Unavailable"));
        let pending = app.rate_limit_pending_message.as_ref().unwrap();
        assert_eq!(pending.retry_attempts, 301);
        assert!(
            pending
                .retry_at
                .unwrap()
                .saturating_duration_since(Instant::now())
                <= Duration::from_secs(7)
        );
        assert!(!app.schedule_pending_remote_retry_with_limit("explicit finite recovery", 2));
    });
}

#[test]
fn finite_provider_budget_never_enables_permanent_client_recovery() {
    with_temp_jcode_home(|| {
        write_test_config("[provider]\nmax_retries = 2\n");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        for error in [
            "HTTP 503 Service Unavailable: invalid api key",
            "HTTP 500 Internal Server Error: invalid_request_error",
            "HTTP 429 Too Many Requests: insufficient_quota",
        ] {
            let mut app = create_test_app();
            let mut remote = crate::tui::backend::RemoteConnection::dummy();
            app.auto_poke_incomplete_todos = false;
            app.last_submitted_input = Some("keep the prompt".into());
            app.rate_limit_pending_message = Some(issue_1685_user_turn("keep the prompt", true, 0));
            app.is_processing = true;
            app.handle_server_event(issue_1685_error(42, error, Some(30)), &mut remote);
            assert!(app.rate_limit_pending_message.is_none(), "{error}");
            assert!(app.rate_limit_reset.is_none(), "{error}");
            assert_eq!(app.input, "keep the prompt", "{error}");
        }
    });
}
