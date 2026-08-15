//! Grouping repositories, and moving memory up to where it belongs.
//!
//! A repository is rarely the right unit for everything that is known. How a
//! bank's services authenticate to each other, or how its deploy pipeline
//! works, is true of all of them — and filed under whichever one happened to be
//! open when it was learned. Grouping is what lets that knowledge reach the
//! other fifty.

use std::path::{Path, PathBuf};

use magent_core::{
    Cardinality, FactKind, FactScope, FactStatus, OperationId, RememberCommand, RepositoryRole,
};
use magent_store::{FactContext, FactQuery, Store};

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

fn init_repo(root: &Path, origin: &str) {
    std::fs::create_dir_all(root).expect("mkdir");
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("README.md"), "seed\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "seed"]);
    git(root, &["remote", "add", "origin", origin]);
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    repos: Vec<PathBuf>,
}

impl Fixture {
    /// Three services under one parent, as a bank's repositories sit on disk.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("magent.db")).expect("open");

        let repos: Vec<PathBuf> = ["clients", "accounts", "deploy-bank"]
            .iter()
            .map(|name| {
                let path = dir.path().join("wbbank").join(name);
                init_repo(&path, &format!("git@example.invalid:wbbank/{name}.git"));
                path
            })
            .collect();

        Self {
            _dir: dir,
            store,
            repos,
        }
    }
}

fn fact(name: &str, title: &str, scope: FactScope) -> RememberCommand {
    RememberCommand {
        operation_id: OperationId::new(),
        name: name.into(),
        title: title.into(),
        body: format!("{title}, in detail."),
        kind: FactKind::Project,
        scope,
        cardinality: Cardinality::Set,
        status: FactStatus::Observed,
        confidence: 0.8,
        evidence: vec![],
        relates_to: vec![],
    }
}

// --- grouping --------------------------------------------------------------

#[test]
fn repositories_can_be_gathered_into_one_workspace() {
    let fixture = Fixture::new();

    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    assert_eq!(grouped.repositories, 3);

    let ids: Vec<_> = fixture
        .repos
        .iter()
        .map(|root| {
            fixture
                .store
                .resolve_workspace_for(root)
                .expect("resolve")
                .workspace_id
        })
        .collect();

    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "the repositories are still in separate workspaces: {ids:?}"
    );
}

/// Grouping the same repositories again must not create a second workspace of
/// the same name, or the group quietly splits in two.
#[test]
fn grouping_twice_converges() {
    let fixture = Fixture::new();

    let first = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("first");
    let second = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("second");

    assert_eq!(first.workspace_id, second.workspace_id);
    assert_eq!(fixture.store.workspace_count().expect("count"), 1);
}

#[test]
fn a_repository_added_later_joins_the_existing_workspace() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos[..2])
        .expect("first");

    let second = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos[2..])
        .expect("second");

    assert_eq!(first.workspace_id, second.workspace_id);
}

/// A path that is not there is a mistake in the call, not a repository. Taking
/// it at face value creates a row keyed on nonsense that then has to be found
/// and removed by hand.
#[test]
fn a_path_that_does_not_exist_is_refused_rather_than_registered() {
    let fixture = Fixture::new();

    let mut roots = fixture.repos.clone();
    roots.push(PathBuf::from("/definitely/not/a/real/path"));

    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &roots)
        .expect("group");

    assert_eq!(
        grouped.repositories, 3,
        "the missing path was registered as a repository"
    );
    assert_eq!(grouped.skipped.len(), 1, "{:?}", grouped.skipped);
}

// --- roles -----------------------------------------------------------------

/// Not every repository in a group is equally safe to touch. Infrastructure
/// that deploys a dozen services deserves a different posture from the service
/// being worked on, and the agent can only respect that if it is recorded.
#[test]
fn a_repository_carries_a_role() {
    let fixture = Fixture::new();
    fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    fixture
        .store
        .set_repository_role(&fixture.repos[2], RepositoryRole::Infrastructure)
        .expect("set role");

    let deploy = fixture
        .store
        .resolve_workspace_for(&fixture.repos[2])
        .expect("resolve");
    assert_eq!(deploy.role, RepositoryRole::Infrastructure);

    let service = fixture
        .store
        .resolve_workspace_for(&fixture.repos[0])
        .expect("resolve");
    assert_eq!(
        service.role,
        RepositoryRole::Primary,
        "an ordinary repository defaults to primary"
    );
}

// --- what grouping is for --------------------------------------------------

/// The payoff. A fact learned while working on one service, recorded at
/// workspace scope, must reach the others.
#[test]
fn workspace_facts_reach_every_repository_in_the_group() {
    let fixture = Fixture::new();
    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    fixture
        .store
        .remember(
            &fact(
                "service-auth",
                "HMAC for clients, Bearer for user-balance",
                FactScope::Workspace,
            ),
            &FactContext {
                workspace_id: Some(grouped.workspace_id),
                ..FactContext::default()
            },
        )
        .expect("remember");

    // Asked from a different repository in the same group.
    let found = fixture
        .store
        .search(&FactQuery {
            text: Some("HMAC Bearer auth".into()),
            namespaces: vec!["accounts".into()],
            workspace_id: Some(grouped.workspace_id),
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
}

/// Grouping must not turn every repository's own notes into everyone's.
#[test]
fn grouping_does_not_leak_repository_facts_sideways() {
    let fixture = Fixture::new();
    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    fixture
        .store
        .remember(
            &fact(
                "clients-quirk",
                "a quirk of clients only",
                FactScope::Repository,
            ),
            &FactContext {
                namespace: Some("clients".into()),
                ..FactContext::default()
            },
        )
        .expect("remember");

    let from_accounts = fixture
        .store
        .search(&FactQuery {
            text: Some("quirk".into()),
            namespaces: vec!["accounts".into()],
            workspace_id: Some(grouped.workspace_id),
            ..FactQuery::default()
        })
        .expect("search");

    assert!(
        from_accounts.is_empty(),
        "a repository's own notes leaked to a sibling: {from_accounts:?}"
    );
}

// --- promoting imported memory ---------------------------------------------

/// The concrete case this exists for. A hundred facts about how a group of
/// services fit together were filed under whichever repository was open at the
/// time; promoting that namespace is what makes them reach the rest.
#[test]
fn a_namespace_can_be_promoted_to_the_whole_workspace() {
    let fixture = Fixture::new();
    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    for name in ["ecosystem-map", "deploy-freeze"] {
        fixture
            .store
            .remember(
                &fact(name, "how the services fit together", FactScope::Repository),
                &FactContext {
                    namespace: Some("wbbank-project-expert".into()),
                    ..FactContext::default()
                },
            )
            .expect("remember");
    }

    let promoted = fixture
        .store
        .promote_namespace("wbbank-project-expert", grouped.workspace_id)
        .expect("promote");
    assert_eq!(promoted, 2);

    let from_a_sibling = fixture
        .store
        .search(&FactQuery {
            text: Some("services fit together".into()),
            namespaces: vec!["accounts".into()],
            workspace_id: Some(grouped.workspace_id),
            ..FactQuery::default()
        })
        .expect("search");

    assert_eq!(
        from_a_sibling.len(),
        2,
        "promoted memory did not reach the group: {from_a_sibling:?}"
    );
}

#[test]
fn promoting_a_namespace_that_does_not_exist_changes_nothing() {
    let fixture = Fixture::new();
    let grouped = fixture
        .store
        .group_into_workspace("wbbank", &fixture.repos)
        .expect("group");

    assert_eq!(
        fixture
            .store
            .promote_namespace("nothing-here", grouped.workspace_id)
            .expect("promote"),
        0
    );
}
