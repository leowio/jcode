use super::*;
use crate::tui::app::{AppRuntimeMode, InertRuntimeProvider};
use std::sync::Arc;

struct EnvironmentGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value { crate::env::set_var(key, value); }
            else { crate::env::remove_var(key); }
        }
        crate::config::invalidate_config_cache();
    }
}

fn setup(configured: bool) -> (tempfile::TempDir, EnvironmentGuard) {
    let private = tempfile::tempdir().unwrap();
    let saved = ["JCODE_HOME", "JCODE_TUI_AUTO_POKE"].into_iter()
        .map(|key| (key, std::env::var_os(key))).collect();
    crate::env::set_var("JCODE_HOME", private.path());
    crate::env::remove_var("JCODE_TUI_AUTO_POKE");
    std::fs::write(private.path().join("config.toml"),
        format!("[features]\nauto_poke = {configured}\n")).unwrap();
    crate::config::invalidate_config_cache();
    assert_eq!(crate::config::config().features.auto_poke, configured);
    (private, EnvironmentGuard(saved))
}

fn local_app() -> App {
    App::new(Arc::new(InertRuntimeProvider::new(AppRuntimeMode::TestHarness)),
        crate::tool::Registry::empty())
}

fn assert_poke(app: &App, expected: bool) {
    assert_eq!(app.auto_poke_incomplete_todos, expected);
    assert_eq!(app.auto_poke_default_on, expected);
    assert!(app.queued_messages.is_empty());
}

#[tokio::test]
async fn tui_auto_poke_env_off_precedes_new_and_resumed_clients() {
    let _lock = crate::storage::lock_test_env();
    let (_private, _restore) = setup(true);
    let mut saved = crate::session::Session::create(None, Some("Unfinished incident".into()));
    saved.add_message(crate::message::Role::Assistant, vec![serde_json::from_value(
        serde_json::json!({"type":"text", "text":"Incident investigation remains unfinished."})).unwrap()]);
    saved.save().unwrap();
    assert!(crate::session::session_exists(&saved.id));
    for disabled in ["0", "false", "off"] {
        crate::env::set_var("JCODE_TUI_AUTO_POKE", disabled);
        let mut local = local_app();
        let mut fresh = App::new_for_remote(None);
        let mut resumed = App::new_for_remote(Some(saved.id.clone()));
        assert_eq!(resumed.session.id, saved.id);
        for app in [&mut local, &mut fresh, &mut resumed] {
            assert_poke(app, false);
            let local_state: serde_json::Value = serde_json::from_str(&app.handle_debug_command("state")).unwrap();
            assert_eq!(local_state["auto_poke_enabled"], false);
            let mut remote = RemoteConnection::dummy();
            let remote_state: serde_json::Value = serde_json::from_str(
                &handle_debug_command(app, "state", &mut remote).await).unwrap();
            assert_eq!(remote_state["auto_poke_enabled"], false);
            app.auto_poke_incomplete_todos = true;
            let updated: serde_json::Value = serde_json::from_str(
                &handle_debug_command(app, "state", &mut remote).await).unwrap();
            assert_eq!(updated["auto_poke_enabled"], true, "debug reports live state, not env default");
        }
        assert!(crate::config::config().features.auto_poke, "shared configuration remains enabled");
    }
}

#[tokio::test]
async fn tui_auto_poke_env_unset_and_invalid_keep_configured_default() {
    let _lock = crate::storage::lock_test_env();
    let (_private, _restore) = setup(true);
    assert_poke(&local_app(), true);
    assert_poke(&App::new_for_remote(None), true);
    crate::env::set_var("JCODE_TUI_AUTO_POKE", "invalid");
    assert_poke(&local_app(), true);
    assert_poke(&App::new_for_remote(None), true);
}

#[tokio::test]
async fn tui_auto_poke_env_on_can_override_disabled_default() {
    let _lock = crate::storage::lock_test_env();
    let (_private, _restore) = setup(false);
    assert_poke(&local_app(), false);
    assert_poke(&App::new_for_remote(None), false);
    for enabled in ["1", "true", "on"] {
        crate::env::set_var("JCODE_TUI_AUTO_POKE", enabled);
        assert_poke(&local_app(), true);
        assert_poke(&App::new_for_remote(None), true);
    }
}
