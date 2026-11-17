use super::*;
use crate::goal::{self, GoalCreateInput, GoalScope, GoalUpdateInput};
use std::path::Path;
use std::process::{Command, Stdio};

struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl RestoreEnv {
    fn capture() -> Self {
        Self(
            ["JCODE_HOME", "JCODE_SHARED_PROJECT_HOME"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        )
    }
}
impl Drop for RestoreEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => crate::env::set_var(key, value),
                None => crate::env::remove_var(key),
            }
        }
    }
}
fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn repo(path: &Path, origin: &str) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["remote", "add", "origin", origin]);
}

#[test]
fn native_shared_project_transactions() {
    if let Ok(project) = std::env::var("JCODE_NATIVE_SHARED_TEST_REPO") {
        let worker = std::env::var("JCODE_NATIVE_SHARED_TEST_WORKER").unwrap();
        let manager = MemoryManager::new().with_project_dir(&project);
        for n in 0..8 {
            manager
                .remember_project(MemoryEntry::new(
                    MemoryCategory::Fact,
                    format!("worker {worker} memory {n}"),
                ))
                .unwrap();
            goal::create_goal(
                GoalCreateInput {
                    title: "Parallel initiative".into(),
                    scope: GoalScope::Project,
                    ..Default::default()
                },
                Some(Path::new(&project)),
            )
            .unwrap();
            goal::update_goal(
                "shared-active",
                Some(GoalScope::Project),
                Some(Path::new(&project)),
                GoalUpdateInput {
                    checkpoint_summary: Some(format!("worker {worker} checkpoint {n}")),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        }
        return;
    }
    let _guard = crate::storage::lock_test_env();
    let _restore = RestoreEnv::capture();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    repo(&project, "https://github.com/Team/Repo.git");
    git(
        &project,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    let worktree = temp.path().join("worktree");
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    let other = temp.path().join("other");
    repo(&other, "git@github.com:team/other.git");
    let shared = temp.path().join("shared");
    crate::env::set_var("JCODE_HOME", temp.path().join("profile-a"));
    crate::env::remove_var("JCODE_SHARED_PROJECT_HOME");
    let manager = MemoryManager::new().with_project_dir(&project);
    manager
        .remember_project(MemoryEntry::new(
            MemoryCategory::Fact,
            "preserve old private memory",
        ))
        .unwrap();
    let legacy = project_memory_file(&project).unwrap();
    let saved = std::fs::read(&legacy).unwrap();
    crate::env::set_var("JCODE_SHARED_PROJECT_HOME", &shared);
    let goal = goal::create_goal(
        GoalCreateInput {
            id: Some("shared-active".into()),
            title: "Shared active initiative".into(),
            scope: GoalScope::Project,
            ..Default::default()
        },
        Some(&project),
    )
    .unwrap();
    goal::attach_goal_to_session("private-session", &goal, Some(&project)).unwrap();
    manager
        .remember_global(MemoryEntry::new(
            MemoryCategory::Fact,
            "private global memory",
        ))
        .unwrap();
    let mut children = Vec::new();
    for n in 0..4 {
        let target = if n % 2 == 0 { &project } else { &worktree };
        children.push(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "memory::shared_project_tests::native_shared_project_transactions",
                    "--nocapture",
                ])
                .env("JCODE_HOME", temp.path().join(format!("worker-{n}")))
                .env("JCODE_SHARED_PROJECT_HOME", &shared)
                .env("JCODE_NATIVE_SHARED_TEST_REPO", target)
                .env("JCODE_NATIVE_SHARED_TEST_WORKER", n.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let graph = manager.load_project_graph().unwrap();
    assert_eq!(
        graph.memories.len(),
        65,
        "32 memories + 32 initiatives + one shared goal; no writer may be lost"
    );
    assert_eq!(
        goal::load_goal("shared-active", Some(GoalScope::Project), Some(&project))
            .unwrap()
            .unwrap()
            .updates
            .len(),
        32
    );
    assert_eq!(goal::list_relevant_goals(Some(&project)).unwrap().len(), 33);
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        saved,
        "legacy data stays intact until explicit migration"
    );
    assert!(
        goal::load_attached_goal("private-session", Some(&worktree))
            .unwrap()
            .is_some(),
        "same-repository worktrees can resume an attachment"
    );
    goal::create_goal(
        GoalCreateInput {
            id: Some("private-global-goal".into()),
            title: "Private global goal".into(),
            scope: GoalScope::Global,
            ..Default::default()
        },
        Some(&project),
    )
    .unwrap();
    // An attachment written before sharing was enabled still resumes after
    // its initiative has been migrated, without rewriting the session file.
    use std::hash::{Hash, Hasher};
    let mut legacy_hash = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut legacy_hash);
    let attachment = temp
        .path()
        .join("profile-a/goals/sessions/private-session.json");
    let mut saved_attachment: serde_json::Value = crate::storage::read_json(&attachment).unwrap();
    saved_attachment["project_hash"] = format!("{:016x}", legacy_hash.finish()).into();
    crate::storage::write_json(&attachment, &saved_attachment).unwrap();
    assert!(
        goal::load_attached_goal("private-session", Some(&project))
            .unwrap()
            .is_some()
    );

    crate::env::set_var("JCODE_HOME", temp.path().join("profile-b"));
    let second = MemoryManager::new().with_project_dir(&worktree);
    assert_eq!(second.load_project_graph().unwrap().memories.len(), 65);
    assert!(
        second.load_global_graph().unwrap().memories.is_empty(),
        "global memories are private"
    );
    assert!(
        goal::load_attached_goal("private-session", Some(&project))
            .unwrap()
            .is_none(),
        "attachments are private"
    );
    assert!(
        goal::load_goal(
            "private-global-goal",
            Some(GoalScope::Global),
            Some(&project)
        )
        .unwrap()
        .is_none(),
        "global goals are private"
    );
    assert!(
        MemoryManager::new()
            .with_project_dir(&other)
            .load_project_graph()
            .unwrap()
            .memories
            .is_empty(),
        "other repositories are isolated"
    );
    assert!(goal::list_relevant_goals(Some(&other)).unwrap().is_empty());
}
