use super::*;
use crate::auth::codex;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

struct SharedEnv(Option<std::ffi::OsString>);
impl SharedEnv {
    fn set(path: &std::path::Path) -> Self {
        let old = std::env::var_os("JCODE_SHARED_OPENAI_AUTH_HOME");
        crate::env::set_var("JCODE_SHARED_OPENAI_AUTH_HOME", path);
        Self(old)
    }
}
impl Drop for SharedEnv {
    fn drop(&mut self) {
        if let Some(old) = &self.0 { crate::env::set_var("JCODE_SHARED_OPENAI_AUTH_HOME", old); }
        else { crate::env::remove_var("JCODE_SHARED_OPENAI_AUTH_HOME"); }
    }
}

#[test]
fn shared_openai_auth_rejects_unsafe_store_paths() {
    let sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let root = sandbox.root();
    let relative = SharedEnv::set(std::path::Path::new("relative"));
    assert!(codex::openai_auth_home().is_err());
    drop(relative);
    let dir = root.join("shared");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = root.join("alias");
    symlink(&dir, &alias).unwrap();
    let alias_env = SharedEnv::set(&alias);
    assert!(codex::openai_auth_home().is_err());
    drop(alias_env);
    let _shared = SharedEnv::set(&dir);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(codex::openai_auth_home().is_err());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let auth = dir.join("openai-auth.json");
    let target = root.join("target");
    std::fs::write(&target, "{}").unwrap();
    symlink(&target, &auth).unwrap();
    assert!(codex::load_auth_file().is_err());
    std::fs::remove_file(&auth).unwrap();
    let lock = dir.join(".openai-refresh.lock");
    symlink(&target, &lock).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert!(runtime.block_on(codex::lock_openai_refresh()).is_err());
}

#[test]
fn shared_openai_auth_refresh_child() {
    let Ok(url) = std::env::var("JCODE_SHARED_AUTH_TEST_URL") else { return; };
    let label = std::env::var("JCODE_SHARED_AUTH_TEST_LABEL").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let tokens = runtime.block_on(refresh_openai_tokens_at(
        &url, Duration::from_secs(5), "old-refresh", Some(label))).unwrap();
    assert_eq!(tokens.access_token, "new-access");
    assert_eq!(tokens.refresh_token, "new-refresh");
    assert!(!std::path::Path::new(&std::env::var("JCODE_HOME").unwrap()).join("openai-auth.json").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_openai_auth_two_processes_rotate_once() {
    let sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let shared = sandbox.root().join("shared");
    let _shared_env = SharedEnv::set(&shared);
    let label = codex::upsert_account(codex::OpenAiAccount {
        label: "openai-otter".to_string(), access_token: "old-access".to_string(),
        refresh_token: "old-refresh".to_string(), id_token: None, account_id: None,
        expires_at: Some(0), email: None,
    }).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let grants = Arc::new(AtomicUsize::new(0));
    let server_grants = grants.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let grants = server_grants.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut received = Vec::new(); let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 { return; }
                    received.extend_from_slice(&buf[..n]);
                    if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&received[..end]);
                        let length = headers.lines().find_map(|line| line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, v)| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if received.len() >= end + 4 + length { break; }
                    }
                }
                grants.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for name in ["private-one", "private-two"] {
        let private = sandbox.root().join(name); std::fs::create_dir(&private).unwrap();
        let child = std::process::Command::new(&executable)
            .args(["--exact", "auth::oauth::shared_openai_auth_tests::shared_openai_auth_refresh_child", "--nocapture"])
            .env("JCODE_HOME", &private).env("JCODE_SHARED_OPENAI_AUTH_HOME", &shared)
            .env("JCODE_SHARED_AUTH_TEST_URL", &url).env("JCODE_SHARED_AUTH_TEST_LABEL", &label)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        children.push(tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()));
    }
    for child in children {
        let output = tokio::time::timeout(Duration::from_secs(15), child).await.unwrap().unwrap();
        assert!(output.status.success(), "child failed: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
    assert_eq!(grants.load(Ordering::SeqCst), 1, "only one rotating-token grant across private runtimes");
    let account = codex::list_accounts().unwrap().remove(0);
    assert_eq!(account.refresh_token, "new-refresh");
    assert!(!sandbox.root().join("openai-auth.json").exists());
    server.abort();
}
