//! Opt-in repository knowledge shared by otherwise isolated Jcode homes.
//!
//! Only project memory and initiatives use this root. Global knowledge,
//! credentials, session attachments and all other state stay private.
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

fn git(project: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn normalize_origin(origin: &str, project: &Path) -> Option<String> {
    let origin = origin.trim().trim_end_matches('/');
    if origin.is_empty() {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = origin.split_once("://") {
        if scheme == "file" {
            return Some(format!(
                "file:{}",
                Path::new(rest).canonicalize().ok()?.display()
            ));
        }
        let (authority, path) = rest.split_once('/')?;
        (
            authority.rsplit('@').next()?.to_ascii_lowercase(),
            path.to_owned(),
        )
    } else if let Some((authority, path)) = origin.split_once(':') {
        if authority.contains('/') {
            return None;
        }
        (
            authority.rsplit('@').next()?.to_ascii_lowercase(),
            path.to_owned(),
        )
    } else {
        let path = Path::new(origin);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            project.join(path)
        };
        return Some(format!("file:{}", path.canonicalize().ok()?.display()));
    };
    // URL query/fragment can contain credentials and never identify a repository.
    let path = path
        .split(['?', '#'])
        .next()?
        .trim_matches('/')
        .trim_end_matches(".git");
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let path = if host == "github.com" || host == "gitlab.com" {
        path.to_ascii_lowercase()
    } else {
        path.to_owned()
    };
    Some(format!("origin:{host}/{path}"))
}

/// A stable, credential-free repository identity. Non-Git directories are not shared.
pub fn shared_repository_identity(project: &Path) -> Option<String> {
    let common = git(
        project,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if let Some(origin) = git(project, &["config", "--get", "remote.origin.url"])
        && let Some(identity) = normalize_origin(&origin, project)
    {
        return Some(identity);
    }
    Some(format!(
        "git:{}",
        Path::new(&common).canonicalize().ok()?.display()
    ))
}

/// Stable FNV-1a key, versioned so it does not change with Rust's DefaultHasher.
pub fn shared_repository_key(identity: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("v1-{hash:016x}")
}

/// `<JCODE_SHARED_PROJECT_HOME>/<repository-key>` when sharing is enabled.
/// Existing legacy project files are deliberately not moved or overwritten here.
pub fn shared_project_dir(project: &Path) -> Result<Option<PathBuf>> {
    let Some(root) = std::env::var_os("JCODE_SHARED_PROJECT_HOME").filter(|x| !x.is_empty()) else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    anyhow::ensure!(
        root.is_absolute(),
        "JCODE_SHARED_PROJECT_HOME must be absolute"
    );
    let Some(identity) = shared_repository_identity(project) else {
        return Ok(None);
    };
    let dir = root.join(shared_repository_key(&identity));
    crate::ensure_dir(&dir)?;
    let dir = dir.canonicalize()?;
    // Detect the unlikely hash collision instead of mixing two repositories.
    let _guard = lock_path(&dir.join("identity.lock"))?;
    let manifest = dir.join("repository.json");
    if manifest.exists() {
        let saved: serde_json::Value = crate::read_json(&manifest)?;
        anyhow::ensure!(
            saved["identity"].as_str() == Some(identity.as_str()),
            "Shared repository key collision at {}",
            dir.display()
        );
    } else {
        crate::write_json(
            &manifest,
            &serde_json::json!({"version":1,"identity":identity}),
        )?;
    }
    Ok(Some(dir))
}

thread_local! {
    // Nested native operations (e.g. remember -> load -> save) share one lock.
    static HELD: RefCell<HashMap<PathBuf, (File, usize)>> = RefCell::new(HashMap::new());
}

/// A short-lived, thread-reentrant cross-process transaction lock.
pub struct ProjectDataLock {
    path: PathBuf,
    _same_thread: PhantomData<Rc<()>>,
}

fn lock_path(path: &Path) -> Result<ProjectDataLock> {
    let path = path.to_owned();
    HELD.with(|held| -> Result<()> {
        let mut held = held.borrow_mut();
        if let Some((_, count)) = held.get_mut(&path) {
            *count += 1;
            return Ok(());
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Open shared knowledge lock {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("Shared project knowledge locking is currently supported on Unix only");
        held.insert(path.clone(), (file, 1));
        Ok(())
    })?;
    Ok(ProjectDataLock {
        path,
        _same_thread: PhantomData,
    })
}

impl Drop for ProjectDataLock {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some((_, count)) = held.get_mut(&self.path) {
                *count -= 1;
                if *count == 0 {
                    held.remove(&self.path);
                }
            }
        });
    }
}

/// Lock one repository store for a complete native read-modify-write operation.
pub fn shared_project_transaction(
    project: Option<&Path>,
    store: &str,
) -> Result<Option<ProjectDataLock>> {
    anyhow::ensure!(
        matches!(store, "memory" | "goals"),
        "Unknown project knowledge store"
    );
    let Some(project) = project else {
        return Ok(None);
    };
    let Some(dir) = shared_project_dir(project)? else {
        return Ok(None);
    };
    lock_path(&dir.join(format!("{store}.lock"))).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_transport_and_credentials_do_not_change_identity() {
        let a = normalize_origin(
            "https://token@github.com/Team/Repo.git?token=secret",
            Path::new("/tmp"),
        )
        .unwrap();
        let b = normalize_origin("git@github.com:team/repo.git", Path::new("/tmp")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "origin:github.com/team/repo");
        assert!(!a.contains("token"));
    }

    #[test]
    fn repository_paths_share_across_profiles_and_worktrees_only_when_enabled() {
        let _guard = crate::test_jcode_home_lock();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        run(&["init", "-q"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/Team/Repository.git",
        ]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        let worktree = temp.path().join("worktree");
        run(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ]);
        let previous = std::env::var_os("JCODE_SHARED_PROJECT_HOME");
        jcode_core::env::remove_var("JCODE_SHARED_PROJECT_HOME");
        assert!(shared_project_dir(&repo).unwrap().is_none());
        jcode_core::env::set_var("JCODE_SHARED_PROJECT_HOME", temp.path().join("shared"));
        assert_eq!(
            shared_project_dir(&repo).unwrap(),
            shared_project_dir(&worktree).unwrap()
        );
        let nonrepo = temp.path().join("not-a-repo");
        std::fs::create_dir(&nonrepo).unwrap();
        assert!(shared_project_dir(&nonrepo).unwrap().is_none());
        let guard = shared_project_transaction(Some(&repo), "memory").unwrap();
        let nested = shared_project_transaction(Some(&worktree), "memory").unwrap();
        assert!(guard.is_some() && nested.is_some());
        drop(nested);
        drop(guard);
        match previous {
            Some(value) => jcode_core::env::set_var("JCODE_SHARED_PROJECT_HOME", value),
            None => jcode_core::env::remove_var("JCODE_SHARED_PROJECT_HOME"),
        }
    }
}
