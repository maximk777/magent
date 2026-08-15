//! How a working directory becomes a repository and a workspace.
//!
//! Getting this wrong is quiet and expensive: runs and memory would leak
//! between unrelated projects, or the same project would fragment into several
//! identities depending on which path the session happened to start from.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use magent_store::{Store, normalize_origin};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        status.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A repository with one commit, and optionally an `origin`.
fn init_repo(root: &Path, origin: Option<&str>) {
    std::fs::create_dir_all(root).expect("mkdir");
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "seed"]);
    if let Some(url) = origin {
        git(root, &["remote", "add", "origin", url]);
    }
}

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("magent.db")).expect("open");
    (dir, store)
}

// --- origin normalisation --------------------------------------------------

/// The same repository cloned over SSH and over HTTPS must not become two
/// projects with two separate memories.
#[test]
fn ssh_and_https_forms_of_one_origin_normalise_together() {
    let ssh = normalize_origin("git@github.com:maximk777/magent.git");
    let https = normalize_origin("https://github.com/maximk777/magent.git");
    let no_suffix = normalize_origin("https://github.com/maximk777/magent");

    assert_eq!(ssh, https);
    assert_eq!(ssh, no_suffix);
    assert_eq!(ssh, "github.com/maximk777/magent");
}

#[test]
fn origin_normalisation_is_case_insensitive_on_the_host_only() {
    assert_eq!(
        normalize_origin("https://GitHub.com/maximk777/Magent.git"),
        "github.com/maximk777/Magent",
        "host case is irrelevant, but path case can be significant"
    );
}

#[test]
fn different_origins_stay_distinct() {
    assert_ne!(
        normalize_origin("git@github.com:maximk777/magent.git"),
        normalize_origin("git@github.com:someone/magent.git")
    );
}

// --- repository identity ---------------------------------------------------

#[test]
fn a_repository_with_an_origin_is_identified_by_that_origin() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("with-origin");
    init_repo(&repo, Some("git@github.com:maximk777/magent.git"));

    let resolved = store.resolve_workspace_for(&repo).expect("resolve");

    assert_eq!(resolved.identity_key, "git:github.com/maximk777/magent");
    assert_eq!(
        resolved.origin_url.as_deref(),
        Some("git@github.com:maximk777/magent.git")
    );
}

/// Local-only repositories are common (scratch work, `git init` experiments).
/// They must still get a stable identity rather than being rejected.
#[test]
fn a_repository_without_a_remote_is_identified_by_its_path() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("no-remote");
    init_repo(&repo, None);

    let resolved = store.resolve_workspace_for(&repo).expect("resolve");

    assert!(
        resolved.identity_key.starts_with("path:"),
        "expected a path identity, got {}",
        resolved.identity_key
    );
    assert!(resolved.origin_url.is_none());
}

/// Hooks fire wherever the session was started, including outside any
/// repository. Failing there would break the session for no good reason.
#[test]
fn a_directory_outside_any_repository_still_resolves() {
    let (dir, store) = temp_store();
    let plain = dir.path().join("not-a-repo");
    std::fs::create_dir_all(&plain).expect("mkdir");

    let resolved = store.resolve_workspace_for(&plain).expect("resolve");

    assert!(resolved.identity_key.starts_with("path:"));
    assert!(resolved.git.is_none(), "no git state outside a repository");
}

#[test]
fn resolving_from_a_subdirectory_finds_the_repository_root() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("nested");
    init_repo(&repo, Some("https://github.com/acme/nested.git"));
    let deep = repo.join("src").join("inner");
    std::fs::create_dir_all(&deep).expect("mkdir");

    let from_root = store.resolve_workspace_for(&repo).expect("root");
    let from_deep = store.resolve_workspace_for(&deep).expect("deep");

    assert_eq!(from_deep.repository_id, from_root.repository_id);
    assert_eq!(from_deep.toplevel, from_root.toplevel);
}

/// A session started through a symlinked path is the same project. Without
/// canonicalisation it would silently get its own empty memory.
#[test]
fn a_symlinked_path_resolves_to_the_same_repository() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("real");
    init_repo(&repo, None);

    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&repo, &link).expect("symlink");

    let direct = store.resolve_workspace_for(&repo).expect("direct");
    let through_link = store.resolve_workspace_for(&link).expect("link");

    assert_eq!(direct.repository_id, through_link.repository_id);
}

/// `superpowers:using-git-worktrees` puts parallel work in linked worktrees.
/// They are one project, so they must share repository-scoped memory.
#[test]
fn a_linked_worktree_shares_the_repository_of_its_main_checkout() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("main-checkout");
    init_repo(&repo, Some("git@github.com:acme/service.git"));

    let worktree = dir.path().join("feature-worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            worktree.to_str().expect("utf-8 path"),
        ],
    );

    let main = store.resolve_workspace_for(&repo).expect("main");
    let linked = store.resolve_workspace_for(&worktree).expect("worktree");

    assert_eq!(
        linked.repository_id, main.repository_id,
        "a worktree is a checkout of the same repository"
    );
    assert_eq!(
        linked.git.expect("git state").branch.as_deref(),
        Some("feature"),
        "but it reports its own branch"
    );
}

#[test]
fn unrelated_repositories_get_separate_workspaces() {
    let (dir, store) = temp_store();
    let first = dir.path().join("alpha");
    let second = dir.path().join("beta");
    init_repo(&first, Some("git@github.com:acme/alpha.git"));
    init_repo(&second, Some("git@github.com:acme/beta.git"));

    let alpha = store.resolve_workspace_for(&first).expect("alpha");
    let beta = store.resolve_workspace_for(&second).expect("beta");

    assert_ne!(alpha.repository_id, beta.repository_id);
    assert_ne!(
        alpha.workspace_id, beta.workspace_id,
        "grouping repositories into one workspace is an explicit action"
    );
}

#[test]
fn resolving_the_same_repository_twice_is_stable() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("stable");
    init_repo(&repo, Some("git@github.com:acme/stable.git"));

    let first = store.resolve_workspace_for(&repo).expect("first");
    let second = store.resolve_workspace_for(&repo).expect("second");

    assert_eq!(first.repository_id, second.repository_id);
    assert_eq!(first.workspace_id, second.workspace_id);
}

/// A repository first seen while git was unavailable gets a path identity,
/// because that is all that can be known then. When git comes back, the same
/// directory resolves to an origin identity — and creating a second row for it
/// would split the project's memory in two, silently and permanently.
///
/// This is not hypothetical: it happened on this machine when the Xcode command
/// line tools vanished mid-session and took `git` with them.
#[test]
fn a_repository_first_seen_without_git_is_upgraded_rather_than_duplicated() {
    let (dir, store) = temp_store();

    // An ordinary neighbour, so the count below distinguishes "upgraded" from
    // "nothing was recorded at all".
    let neighbour = dir.path().join("service");
    init_repo(&neighbour, Some("git@github.com:acme/service.git"));
    store.resolve_workspace_for(&neighbour).expect("neighbour");

    // Stand in for a broken git: resolve the path before the repository is
    // discoverable, which is the state a missing toolchain leaves behind.
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).expect("mkdir");
    let degraded = store.resolve_workspace_for(&plain).expect("degraded");
    assert!(degraded.identity_key.starts_with("path:"));

    // Now give that same path an origin, as restoring git would.
    init_repo(&plain, Some("git@github.com:acme/plain.git"));
    let recovered = store.resolve_workspace_for(&plain).expect("recovered");

    assert_eq!(
        recovered.repository_id, degraded.repository_id,
        "the repository was duplicated instead of upgraded, so its memory is now split"
    );
    assert_eq!(recovered.identity_key, "git:github.com/acme/plain");
    assert_eq!(
        store.repository_count().expect("count"),
        2,
        "one for each directory, not three"
    );
}

// --- git state -------------------------------------------------------------

#[test]
fn git_state_reports_branch_head_and_uncommitted_count() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("dirty");
    init_repo(&repo, None);
    std::fs::write(repo.join("README.md"), "changed\n").expect("write");
    std::fs::write(repo.join("extra.txt"), "new\n").expect("write");

    let state = store
        .resolve_workspace_for(&repo)
        .expect("resolve")
        .git
        .expect("git state");

    assert_eq!(state.branch.as_deref(), Some("main"));
    assert_eq!(
        state.sha.as_ref().map(String::len),
        Some(40),
        "a full object id, so it can be compared exactly"
    );
    assert_eq!(
        state.dirty_files, 2,
        "one modified and one untracked file are both uncommitted work"
    );
}

/// Magent records dirty state; it never cleans it. Losing a colleague's
/// work-in-progress is unrecoverable, so this stays a read-only observation.
#[test]
fn resolving_never_modifies_the_working_tree() {
    let (dir, store) = temp_store();
    let repo = dir.path().join("untouched");
    init_repo(&repo, None);
    std::fs::write(repo.join("wip.txt"), "precious\n").expect("write");

    store.resolve_workspace_for(&repo).expect("resolve");

    assert_eq!(
        std::fs::read_to_string(repo.join("wip.txt")).expect("read"),
        "precious\n"
    );
    assert!(status_is_dirty(&repo), "uncommitted work must survive");
}

fn status_is_dirty(repo: &Path) -> bool {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()
        .expect("git status");
    !output.stdout.is_empty()
}

// --- integration with runs -------------------------------------------------

/// Two sessions started in the same repository must land on the same workspace,
/// which is what lets a resumed run find its checkpoint.
#[test]
fn runs_started_from_different_paths_of_one_repository_share_a_workspace() {
    use magent_core::{HarnessKind, OperationId, StartRunCommand};

    let (dir, store) = temp_store();
    let repo = dir.path().join("shared");
    init_repo(&repo, Some("git@github.com:acme/shared.git"));
    let deep = repo.join("crates").join("thing");
    std::fs::create_dir_all(&deep).expect("mkdir");

    let start = |root: PathBuf| StartRunCommand {
        operation_id: OperationId::new(),
        task: "trace the leak".into(),
        resume_run_id: None,
        external_session_hint: None,
        workspace_roots: vec![root],
    };

    let from_root = store
        .start_run(&start(repo.clone()), HarnessKind::ClaudeCode)
        .expect("root run");
    let from_deep = store
        .start_run(&start(deep), HarnessKind::ClaudeCode)
        .expect("deep run");

    assert_eq!(from_deep.workspace_id, from_root.workspace_id);
    assert_ne!(from_deep.run_id, from_root.run_id, "still separate runs");
}
