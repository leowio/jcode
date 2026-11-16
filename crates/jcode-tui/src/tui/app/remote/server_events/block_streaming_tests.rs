use super::*;
use crate::tui::app::tests::create_test_app;
use crate::tui::backend::RemoteConnection;
use crate::tui::stream_buffer::StreamBuffer;

#[test]
fn block_streaming_terminal_events_flush_partial_paragraphs() {
    for event in [
        ServerEvent::Done { id: 7 },
        ServerEvent::Interrupted,
        ServerEvent::Error {
            id: 7,
            message: "invalid request: test failure".into(),
            retry_after_secs: None,
        },
    ] {
        let mut app = create_test_app();
        app.suppress_terminal_title_updates = true;
        app.auto_poke_incomplete_todos = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let mut remote = RemoteConnection::dummy();
        app.is_processing = true;
        app.current_message_id = Some(7);
        app.status = ProcessingStatus::Streaming;
        app.stream_buffer = StreamBuffer::new().with_block_streaming(true);
        let ops = app.stream_buffer.push_text("unfinished paragraph 你好");
        assert!(ops.is_empty());
        assert!(app.streaming.streaming_text.is_empty());

        handle_server_event(&mut app, event, &mut remote);

        assert!(app.stream_buffer.is_empty());
        assert!(app.deferred_stream_done_id.is_none());
        assert!(!app.is_processing);
        assert!(app.display_messages.iter().any(|message| {
            message.role == "assistant" && message.content == "unfinished paragraph 你好"
        }));
    }
}

#[test]
fn block_streaming_tool_commit_flushes_without_waiting() {
    let mut app = create_test_app();
    app.stream_buffer = StreamBuffer::new().with_block_streaming(true);
    assert!(app.stream_buffer.push_text("before tool").is_empty());
    assert!(app.commit_pending_streaming_assistant_message());
    assert!(app.stream_buffer.is_empty());
    assert_eq!(app.display_messages.last().unwrap().content, "before tool");
    assert!(app.stream_buffer.block_streaming_enabled());
}

#[test]
fn block_streaming_local_tick_does_not_force_partial_reveal() {
    let mut app = create_test_app();
    app.suppress_terminal_title_updates = true;
    app.stream_buffer = StreamBuffer::new().with_block_streaming(true);
    assert!(app.stream_buffer.push_text("still arriving").is_empty());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    crate::tui::app::local::handle_tick(&mut app);
    assert!(app.streaming.streaming_text.is_empty());
    assert!(!app.stream_buffer.is_empty());
}

/// Observe real ui::draw output, excluding status animations and other chrome.
/// Each paragraph fits on one row at this terminal width.
fn rendered_acceptance_answer(
    app: &App,
    terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
) -> String {
    terminal.draw(|frame| crate::tui::ui::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut paragraphs = Vec::new();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        for marker in ["ALPHA", "BRAVO", "CHARLIE"] {
            if let Some(start) = row.find(marker) {
                paragraphs.push(row[start..].trim_end().to_string());
            }
        }
    }
    paragraphs.join("\n\n")
}

#[test]
#[ignore = "wall-clock rendered acceptance comparison, run explicitly with --ignored --nocapture"]
fn block_streaming_rendered_acceptance_comparison() {
    use std::time::{Duration, Instant};

    let _render_lock = crate::tui::ui::render_state_test_lock();
    let first = "ALPHA paragraph becomes visible as one readable block.";
    let last = "BRAVO final paragraph stays intact at completion.";
    let answer = format!("{first}\n\n{last}");
    let mut reports = Vec::new();

    for block in [false, true] {
        let mut app = create_test_app();
        app.suppress_terminal_title_updates = true;
        app.auto_poke_incomplete_todos = false;
        app.session.short_name = Some("block acceptance".into());
        app.is_processing = true;
        app.current_message_id = Some(7);
        app.status = ProcessingStatus::Streaming;
        app.stream_buffer = StreamBuffer::new().with_block_streaming(block);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let mut remote = RemoteConnection::dummy();
        let mut terminal = ratatui::Terminal::new(
            ratatui::backend::TestBackend::new(120, 60),
        ).unwrap();
        let start = Instant::now();
        let mut observed = String::new();
        let mut changes = Vec::new();
        rendered_acceptance_answer(&app, &mut terminal);

        for ch in answer.chars() {
            std::thread::sleep(Duration::from_millis(8));
            handle_server_event(
                &mut app,
                ServerEvent::TextDelta { text: ch.to_string() },
                &mut remote,
            );
            let ops = app.stream_buffer.flush_smooth_frame();
            app.apply_stream_ops(ops);
            let frame_text = rendered_acceptance_answer(&app, &mut terminal);
            if frame_text != observed {
                if block {
                    assert_eq!(frame_text, first, "no partially typed paragraph may render");
                }
                observed = frame_text;
                changes.push((start.elapsed().as_millis(), observed.clone()));
            }
        }
        let done_at = Instant::now();
        handle_server_event(&mut app, ServerEvent::Done { id: 7 }, &mut remote);
        // Normal pacing deliberately defers Done while characters remain.
        // Render its draining frames too, so both modes finish the same workflow.
        while !app.stream_buffer.is_empty() {
            std::thread::sleep(Duration::from_millis(8));
            let ops = app.stream_buffer.flush_smooth_frame();
            app.apply_stream_ops(ops);
            let frame_text = rendered_acceptance_answer(&app, &mut terminal);
            if frame_text != observed {
                observed = frame_text;
                changes.push((start.elapsed().as_millis(), observed.clone()));
            }
        }
        if let Some(id) = app.deferred_stream_done_id.take() {
            handle_server_event(&mut app, ServerEvent::Done { id }, &mut remote);
        }
        let final_text = rendered_acceptance_answer(&app, &mut terminal);
        if final_text != observed {
            changes.push((start.elapsed().as_millis(), final_text.clone()));
        }
        assert_eq!(final_text, answer, "completion must preserve every rendered character");
        assert!(!app.is_processing);
        if block {
            assert_eq!(changes.len(), 2, "exactly one reveal per paragraph");
            assert!(done_at.elapsed() < Duration::from_millis(250));
        } else {
            assert!(changes.len() >= 10, "baseline must actually show incremental typing");
        }
        reports.push(serde_json::json!({
            "block_streaming": block,
            "visible_answer_changes": changes.len(),
            "completion_latency_ms": done_at.elapsed().as_millis(),
            "snapshots": changes,
            "final_text": final_text,
        }));
    }

    // Also observe a silent gap: the timer must reveal an unfinished paragraph
    // even when the provider sends no more deltas and no completion event.
    let mut app = create_test_app();
    app.suppress_terminal_title_updates = true;
    app.is_processing = true;
    app.status = ProcessingStatus::Streaming;
    app.stream_buffer = StreamBuffer::new().with_block_streaming(true);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let mut terminal = ratatui::Terminal::new(
        ratatui::backend::TestBackend::new(120, 60),
    ).unwrap();
    let pending = "CHARLIE paragraph without a blank line or completion event.";
    assert!(app.stream_buffer.push_text(pending).is_empty());
    assert!(rendered_acceptance_answer(&app, &mut terminal).is_empty());
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let ops = app.stream_buffer.flush_smooth_frame();
        app.apply_stream_ops(ops);
        let rendered = rendered_acceptance_answer(&app, &mut terminal);
        if !rendered.is_empty() {
            assert_eq!(rendered, pending);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(2), "silent-gap reveal stalled");
    }
    let timeout_ms = start.elapsed().as_millis();
    assert!((950..1500).contains(&timeout_ms), "timeout observed at {timeout_ms}ms");
    let report = serde_json::json!({
        "workflow": "TextDelta -> ordered stream buffer -> ui::draw -> TestBackend cells -> Done",
        "comparison": reports,
        "silent_gap_reveal_ms": timeout_ms,
    });
    let report = serde_json::to_string_pretty(&report).unwrap();
    eprintln!("BLOCK_STREAMING_ACCEPTANCE {report}");
    if let Ok(path) = std::env::var("JCODE_BLOCK_STREAM_REPORT") {
        std::fs::write(path, report).unwrap();
    }
}
