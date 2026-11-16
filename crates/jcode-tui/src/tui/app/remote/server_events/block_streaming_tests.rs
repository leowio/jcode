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
