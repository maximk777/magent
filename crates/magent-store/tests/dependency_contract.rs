//! Reference checkouts: repositories the agent should be able to read but does
//! not work in.
//!
//! The value here is materialisation, not search. Once a dependency is on disk
//! at a known revision, the agent's own tools — grep, read — are better than
//! anything a bespoke index would offer, and they are never stale. So a
//! dependency is a declaration plus a path, and the contract below is about
//! getting that path right and keeping it honest.

use std::path::{Path, PathBuf};

use magent_core::WorkspaceId;
use magent_store::{DependencySpec, DependencyStatus, Store};

fn git(directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A real repository on disk, so cloning is exercised rather than mocked.
fn upstream(root: &Path, contents: &str) -> String {
    std::fs::create_dir_all(root).expect("mkdir");
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("lib.rs"), contents).expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "seed"]);

    // file://, not a bare path: git deliberately ignores --depth for local
    // clones and hardlinks the object store instead, so a bare path would let
    // the shallow test pass without proving anything.
    format!("file://{}", root.display())
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    workspace_id: WorkspaceId,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");

        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        let workspace_id = store
            .resolve_workspace_for(&project)
            .expect("resolve")
            .workspace_id;

        Self {
            dir,
            store,
            workspace_id,
        }
    }

    fn deps_root(&self) -> PathBuf {
        self.dir.path().join("deps")
    }

    fn upstream(&self, name: &str, contents: &str) -> String {
        upstream(&self.dir.path().join("upstream").join(name), contents)
    }
}

// --- declaring -------------------------------------------------------------

#[test]
fn a_declared_dependency_is_listed_with_where_it_will_live() {
    let fixture = Fixture::new();

    let declared = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: "git@github.com:acme/thing.git".into(),
                git_ref: Some("v1.2.0".into()),
            },
        )
        .expect("declare");

    assert_eq!(
        declared.slug, "github.com/acme/thing@v1.2.0",
        "the slug names the project and the revision, so two refs can coexist"
    );
    assert_eq!(declared.status, DependencyStatus::Declared);
    assert_eq!(declared.url, "git@github.com:acme/thing.git", "as given");

    let listed = fixture
        .store
        .dependencies(fixture.workspace_id)
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, declared.id);
}

/// The same normalisation repositories get. Declaring a project over SSH and
/// then over HTTPS is one dependency, not two copies of the same source.
#[test]
fn the_two_url_forms_of_one_project_are_one_dependency() {
    let fixture = Fixture::new();

    let ssh = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: "git@github.com:acme/thing.git".into(),
                git_ref: None,
            },
        )
        .expect("declare");

    let https = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: "https://github.com/acme/thing".into(),
                git_ref: None,
            },
        )
        .expect("declare again");

    assert_eq!(ssh.id, https.id, "one project, one checkout");
    assert_eq!(
        fixture
            .store
            .dependencies(fixture.workspace_id)
            .expect("list")
            .len(),
        1
    );
}

/// Wanting v1 and v2 side by side is a real thing to want — comparing an API
/// across a major version is exactly when reference sources earn their keep.
#[test]
fn two_refs_of_one_project_are_two_dependencies() {
    let fixture = Fixture::new();

    for git_ref in ["v1.0.0", "v2.0.0"] {
        fixture
            .store
            .declare_dependency(
                fixture.workspace_id,
                &DependencySpec {
                    url: "https://github.com/acme/thing".into(),
                    git_ref: Some(git_ref.into()),
                },
            )
            .expect("declare");
    }

    let listed = fixture
        .store
        .dependencies(fixture.workspace_id)
        .expect("list");
    assert_eq!(listed.len(), 2);
}

#[test]
fn a_dependency_belongs_to_the_workspace_that_declared_it() {
    let fixture = Fixture::new();
    let elsewhere = fixture.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    let other = fixture
        .store
        .resolve_workspace_for(&elsewhere)
        .expect("resolve")
        .workspace_id;

    fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: "https://github.com/acme/thing".into(),
                git_ref: None,
            },
        )
        .expect("declare");

    assert!(
        fixture.store.dependencies(other).expect("list").is_empty(),
        "another workspace's reference sources are not this one's"
    );
}

// --- the path ---------------------------------------------------------------

/// A URL is attacker-adjacent input: it decides a path on disk. A ref of
/// `../../../../etc` must land inside the deps root or nowhere.
#[test]
fn a_hostile_url_cannot_escape_the_deps_root() {
    let fixture = Fixture::new();

    for (url, git_ref) in [
        ("https://github.com/../../../etc/passwd", None),
        ("https://github.com/acme/thing", Some("../../../../tmp/x")),
        ("https://github.com/acme/..%2f..%2fthing", None),
    ] {
        let declared = fixture
            .store
            .declare_dependency(
                fixture.workspace_id,
                &DependencySpec {
                    url: url.into(),
                    git_ref: git_ref.map(Into::into),
                },
            )
            .expect("declare");

        let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &declared);
        assert!(
            checkout.starts_with(fixture.deps_root()),
            "{url} escaped to {}",
            checkout.display()
        );
        assert!(
            !declared.slug.contains(".."),
            "{url} produced a traversing slug: {}",
            declared.slug
        );
    }
}

// --- syncing ----------------------------------------------------------------

#[test]
fn syncing_puts_the_sources_on_disk_and_records_the_revision() {
    let fixture = Fixture::new();
    let url = fixture.upstream("thing", "pub fn retry() {}\n");

    let declared = fixture
        .store
        .declare_dependency(fixture.workspace_id, &DependencySpec { url, git_ref: None })
        .expect("declare");

    let synced = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");

    assert_eq!(synced.status, DependencyStatus::Present);
    assert!(synced.revision.is_some(), "the revision is what pins it");
    assert!(synced.synced_at.is_some());

    let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &synced);
    assert_eq!(
        std::fs::read_to_string(checkout.join("lib.rs")).expect("the sources are readable"),
        "pub fn retry() {}\n"
    );
}

/// Shallow, because the point is to read the current sources. Full history on
/// twenty dependencies is gigabytes nobody asked for.
#[test]
fn the_checkout_is_shallow() {
    let fixture = Fixture::new();
    let url = fixture.upstream("thing", "one\n");
    let upstream_root = fixture.dir.path().join("upstream").join("thing");
    for extra in ["two", "three"] {
        std::fs::write(upstream_root.join("lib.rs"), extra).expect("write");
        git(&upstream_root, &["commit", "-am", extra]);
    }

    let declared = fixture
        .store
        .declare_dependency(fixture.workspace_id, &DependencySpec { url, git_ref: None })
        .expect("declare");
    let synced = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");

    let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &synced);
    let log = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&checkout)
        .output()
        .expect("git log");
    let commits = String::from_utf8_lossy(&log.stdout).lines().count();
    assert_eq!(commits, 1, "a shallow checkout carries one commit");
}

/// Pinning to a commit is the reproducible case, and it is the one git makes
/// awkward: `--branch` accepts a branch or tag and refuses a commit.
#[test]
fn a_dependency_can_be_pinned_to_a_commit() {
    let fixture = Fixture::new();
    let url = fixture.upstream("thing", "first\n");
    let upstream_root = fixture.dir.path().join("upstream").join("thing");

    let pinned = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&upstream_root)
        .output()
        .expect("rev-parse");
    let pinned = String::from_utf8_lossy(&pinned.stdout).trim().to_owned();

    std::fs::write(upstream_root.join("lib.rs"), "second\n").expect("write");
    git(&upstream_root, &["commit", "-am", "second"]);

    let declared = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url,
                git_ref: Some(pinned.clone()),
            },
        )
        .expect("declare");

    let synced = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");

    assert_eq!(synced.status, DependencyStatus::Present, "{synced:?}");
    assert_eq!(synced.revision.as_deref(), Some(pinned.as_str()));
    let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &synced);
    assert_eq!(
        std::fs::read_to_string(checkout.join("lib.rs")).expect("read"),
        "first\n",
        "the pinned commit, not the tip"
    );
}

#[test]
fn syncing_again_picks_up_new_commits_without_recloning() {
    let fixture = Fixture::new();
    let url = fixture.upstream("thing", "before\n");
    let upstream_root = fixture.dir.path().join("upstream").join("thing");

    let declared = fixture
        .store
        .declare_dependency(fixture.workspace_id, &DependencySpec { url, git_ref: None })
        .expect("declare");
    let first = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");

    std::fs::write(upstream_root.join("lib.rs"), "after\n").expect("write");
    git(&upstream_root, &["commit", "-am", "second"]);

    let second = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("resync");

    assert_ne!(
        first.revision, second.revision,
        "the new commit was picked up"
    );
    let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &second);
    assert_eq!(
        std::fs::read_to_string(checkout.join("lib.rs")).expect("read"),
        "after\n"
    );
}

/// Failure has to be visible and survivable. A dependency that cannot be
/// fetched is a fact about the workspace, not a reason to stop.
#[test]
fn a_url_that_cannot_be_cloned_records_why_and_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let missing = fixture.dir.path().join("does-not-exist");

    let declared = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: missing.display().to_string(),
                git_ref: None,
            },
        )
        .expect("declare");

    let failed = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("a failed sync is reported, not an error");

    assert_eq!(failed.status, DependencyStatus::Failed);
    assert!(
        failed.last_error.is_some(),
        "the reason has to reach the person who typed the URL"
    );
    assert!(
        !magent_store::dependency_checkout(&fixture.deps_root(), &failed).exists(),
        "a half-clone would be read as sources"
    );
}

/// A failure is not permanent: the network comes back, the typo gets fixed.
#[test]
fn a_failed_dependency_recovers_on_the_next_sync() {
    let fixture = Fixture::new();
    let target = fixture.dir.path().join("upstream").join("late");

    let declared = fixture
        .store
        .declare_dependency(
            fixture.workspace_id,
            &DependencySpec {
                url: target.display().to_string(),
                git_ref: None,
            },
        )
        .expect("declare");

    let failed = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");
    assert_eq!(failed.status, DependencyStatus::Failed);

    upstream(&target, "arrived\n");

    let recovered = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("resync");
    assert_eq!(recovered.status, DependencyStatus::Present);
    assert!(
        recovered.last_error.is_none(),
        "a stale error would misreport a working checkout"
    );
}

// --- forgetting -------------------------------------------------------------

#[test]
fn forgetting_a_dependency_removes_the_row_and_the_checkout() {
    let fixture = Fixture::new();
    let url = fixture.upstream("thing", "sources\n");

    let declared = fixture
        .store
        .declare_dependency(fixture.workspace_id, &DependencySpec { url, git_ref: None })
        .expect("declare");
    let synced = fixture
        .store
        .sync_dependency(declared.id, &fixture.deps_root())
        .expect("sync");
    let checkout = magent_store::dependency_checkout(&fixture.deps_root(), &synced);
    assert!(checkout.exists());

    fixture
        .store
        .forget_dependency(declared.id, &fixture.deps_root())
        .expect("forget");

    assert!(
        fixture
            .store
            .dependencies(fixture.workspace_id)
            .expect("list")
            .is_empty()
    );
    assert!(!checkout.exists(), "the sources go with the declaration");
    // The deps root is the one thing this feature promises to keep legible.
    // A shell of empty directories named after every project ever removed
    // makes it read as though something is still tracked.
    let left_behind: Vec<_> = std::fs::read_dir(fixture.deps_root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert!(
        left_behind.is_empty(),
        "empty parents were left behind: {left_behind:?}"
    );
}
