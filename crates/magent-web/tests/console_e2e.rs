//! The console, driven the way a browser drives it.
//!
//! Requests go through the real router; nothing calls a handler directly. What
//! matters about a console is that the page a person sees reflects the store
//! and that a button changes it, and only a request can show both.
//!
//! Curation is the one place where memory is changed on purpose, so these lean
//! hardest on what must survive: a withdrawal that can be undone, a correction
//! that keeps what it replaced.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use magent_core::{
    Cardinality, FactId, FactKind, FactScope, FactStatus, HarnessKind, OperationId,
    RememberCommand, StartRunCommand,
};
use magent_store::{FactContext, Store};
use magent_web::{Console, router};
use tower::ServiceExt;

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    app: Router,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let database = dir.path().join("magent.db");
        let store = Arc::new(Store::open(&database).expect("open"));

        let app = router(Console {
            store: Arc::clone(&store),
            database,
            deps_root: dir.path().join("deps"),
        });

        Self { dir, store, app }
    }

    fn remember(&self, name: &str, title: &str, body: &str) -> FactId {
        self.store
            .remember(
                &RememberCommand {
                    operation_id: OperationId::new(),
                    name: name.into(),
                    title: title.into(),
                    body: body.into(),
                    kind: FactKind::Project,
                    scope: FactScope::Repository,
                    cardinality: Cardinality::Set,
                    status: FactStatus::Observed,
                    confidence: 0.7,
                    evidence: vec![],
                    relates_to: vec![],
                },
                &FactContext {
                    namespace: Some("service".into()),
                    ..FactContext::default()
                },
            )
            .expect("remember")
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A form submission, as the browser sends one.
    async fn post(&self, path: &str, form: &str) -> StatusCode {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form.to_owned()))
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
    }
}

// --- the pages exist and say what is there ---------------------------------

#[tokio::test]
async fn every_page_answers() {
    let fixture = Fixture::new();

    for path in ["/", "/facts", "/runs", "/workspaces", "/deps"] {
        let (status, body) = fixture.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path} returned {status}");
        assert!(body.contains("magent"), "{path} rendered no page");
    }
}

/// A console is unusable offline if its assets come from a CDN, and this one is
/// for a local database.
#[tokio::test]
async fn the_assets_are_served_by_the_binary_itself() {
    let fixture = Fixture::new();

    let (status, css) = fixture.get("/static/app.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(css.contains("--navy-900"), "the stylesheet is not there");

    let (status, js) = fixture.get("/static/htmx.min.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(js.contains("htmx"), "htmx is not there");
}

#[tokio::test]
async fn the_overview_counts_what_the_store_holds() {
    let fixture = Fixture::new();
    fixture.remember("first", "the first thing", "body");
    fixture.remember("second", "the second thing", "body");

    let (_, body) = fixture.get("/").await;

    assert!(
        body.contains(">2</div>"),
        "the fact count is missing: {body}"
    );
    assert!(body.contains("live facts"));
}

#[tokio::test]
async fn a_fact_can_be_opened_from_the_list() {
    let fixture = Fixture::new();
    let id = fixture.remember(
        "goose-locking",
        "goose locks with a table locker",
        "goose_lock needs DDL rights",
    );

    let (_, list) = fixture.get("/facts").await;
    assert!(list.contains("goose-locking"), "the fact is not listed");
    assert!(
        list.contains(&format!("/facts/{id}")),
        "the row does not link to the fact"
    );

    let (status, detail) = fixture.get(&format!("/facts/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("DDL rights"), "the body is not shown");
}

/// The list is filtered on every keystroke, so it is the one thing swapped in
/// place rather than reloaded.
#[tokio::test]
async fn the_row_fragment_is_filtered_and_carries_no_page_chrome() {
    let fixture = Fixture::new();
    fixture.remember("goose-locking", "migration locking", "about locking");
    fixture.remember("unrelated", "something else", "about other things");

    let (status, rows) = fixture.get("/facts/rows?q=locking").await;

    assert_eq!(status, StatusCode::OK);
    assert!(rows.contains("goose-locking"));
    assert!(
        !rows.contains("unrelated"),
        "the filter was ignored: {rows}"
    );
    assert!(
        !rows.contains("<html"),
        "the fragment carries a whole page, which would nest one inside the other"
    );
}

#[tokio::test]
async fn an_unknown_fact_is_a_not_found_rather_than_a_crash() {
    let fixture = Fixture::new();

    let (status, _) = fixture
        .get("/facts/00000000-0000-4000-8000-000000000000")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = fixture.get("/facts/not-an-id").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- curation through the buttons ------------------------------------------

#[tokio::test]
async fn withdrawing_a_fact_hides_it_and_reinstating_brings_it_back() {
    let fixture = Fixture::new();
    let id = fixture.remember("old-advice", "do it the old way", "no longer true");

    let status = fixture
        .post(&format!("/facts/{id}/revoke"), "reason=the+API+changed")
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "the button did not redirect");

    let (_, listed) = fixture.get("/facts").await;
    assert!(
        !listed.contains("old-advice"),
        "a withdrawn fact is still listed"
    );

    // Still reachable: a console that could not open what it just withdrew
    // would be useless.
    let (status, detail) = fixture.get(&format!("/facts/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        detail.contains("the API changed"),
        "the reason was not kept"
    );

    fixture.post(&format!("/facts/{id}/reinstate"), "").await;
    let (_, listed) = fixture.get("/facts").await;
    assert!(listed.contains("old-advice"), "reinstating did nothing");
}

/// The strongest status must not be the cheapest to assert, from a form any
/// more than from a tool.
#[tokio::test]
async fn confirming_needs_something_behind_it() {
    let fixture = Fixture::new();
    let id = fixture.remember("unchecked", "asserted", "body");

    let status = fixture
        .post(&format!("/facts/{id}/verify"), "locator=")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let status = fixture
        .post(
            &format!("/facts/{id}/verify"),
            "locator=internal%2Fmigrate.go%3A41",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, detail) = fixture.get(&format!("/facts/{id}")).await;
    assert!(detail.contains("verified"), "the fact was not confirmed");
    assert!(
        detail.contains("internal/migrate.go:41"),
        "the evidence is not shown"
    );
}

#[tokio::test]
async fn correcting_a_fact_keeps_the_previous_wording() {
    let fixture = Fixture::new();
    let id = fixture.remember("retry-budget", "the retry budget is 3", "hardcoded");

    let status = fixture
        .post(
            &format!("/facts/{id}/edit"),
            "title=the+retry+budget+is+configurable&body=read+from+config",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, listed) = fixture.get("/facts").await;
    assert!(listed.contains("the retry budget is configurable"));
    assert!(
        !listed.contains("the retry budget is 3"),
        "both versions are being listed as current"
    );

    // The old version is still openable, because a decision may rest on it.
    let (status, old) = fixture.get(&format!("/facts/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(old.contains("the retry budget is 3"));
}

#[tokio::test]
async fn a_fact_can_be_moved_to_a_workspace_from_the_page() {
    let fixture = Fixture::new();
    let sibling = fixture.dir.path().join("sibling");
    std::fs::create_dir_all(&sibling).expect("mkdir");
    fixture
        .store
        .group_into_workspace("bank", &[sibling])
        .expect("group");

    let id = fixture.remember("service-auth", "HMAC for clients", "everywhere");

    let status = fixture
        .post(
            &format!("/facts/{id}/scope"),
            "scope=workspace&workspace=bank",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, detail) = fixture.get(&format!("/facts/{id}")).await;
    assert!(detail.contains("workspace"), "the scope did not change");
}

#[tokio::test]
async fn merging_from_the_overview_folds_one_into_the_other() {
    let fixture = Fixture::new();
    let keep = fixture.remember(
        "lock-timeout",
        "the goose lock timeout is ninety seconds",
        "body",
    );
    let duplicate = fixture.remember(
        "locking-timeout",
        "the goose locking timeout is ninety seconds",
        "body",
    );

    let status = fixture
        .post(
            &format!("/facts/{keep}/merge"),
            &format!("duplicate={duplicate}"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, listed) = fixture.get("/facts").await;
    assert!(listed.contains("lock-timeout"));
    assert!(
        !listed.contains("locking-timeout"),
        "the folded-in fact is still listed"
    );
}

// --- runs and workspaces ---------------------------------------------------

#[tokio::test]
async fn an_open_run_can_be_closed_from_the_console() {
    let fixture = Fixture::new();
    let started = fixture
        .store
        .start_run(
            &StartRunCommand {
                operation_id: OperationId::new(),
                task: "something left open".into(),
                resume_run_id: None,
                external_session_hint: None,
                workspace_roots: vec![fixture.dir.path().to_path_buf()],
            },
            HarnessKind::ClaudeCode,
        )
        .expect("start");

    let (_, runs) = fixture.get("/runs").await;
    assert!(runs.contains("something left open"));

    let status = fixture
        .post(&format!("/runs/{}/close", started.run_id), "")
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    assert_eq!(
        fixture.store.get_run(started.run_id).expect("run").status,
        magent_core::RunStatus::Completed
    );
}

#[tokio::test]
async fn a_namespace_can_be_promoted_from_the_workspaces_page() {
    let fixture = Fixture::new();
    let sibling = fixture.dir.path().join("sibling");
    std::fs::create_dir_all(&sibling).expect("mkdir");
    fixture
        .store
        .group_into_workspace("bank", &[sibling])
        .expect("group");
    fixture.remember("shared-thing", "true of the whole group", "body");

    let (_, page) = fixture.get("/workspaces").await;
    assert!(page.contains("service"), "the namespace is not listed");
    assert!(page.contains("bank"), "the workspace is not offered");

    let status = fixture
        .post("/workspaces/promote", "namespace=service&workspace=bank")
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let promoted = fixture
        .store
        .fact(
            fixture
                .store
                .browse_facts(&magent_store::FactFilter::default())
                .expect("browse")[0]
                .fact_id,
        )
        .expect("read")
        .expect("fact");
    assert_eq!(promoted.scope, FactScope::Workspace);
}

/// Promoting into a workspace that does not exist is a stale form, not a crash.
#[tokio::test]
async fn promoting_into_an_unknown_workspace_is_refused() {
    let fixture = Fixture::new();

    let status = fixture
        .post("/workspaces/promote", "namespace=service&workspace=nope")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- reference checkouts ----------------------------------------------------

/// The console is where a dependency is added, because naming a git URL is a
/// decision a person makes. What it must show afterwards is the path, since
/// that is what the agent will be told to read.
#[tokio::test]
async fn a_dependency_can_be_declared_and_shows_its_path() {
    let fixture = Fixture::new();
    let upstream = seed_upstream(fixture.dir.path(), "package retry\n");
    let workspace_id = fixture
        .store
        .resolve_workspace_for(fixture.dir.path())
        .expect("resolve")
        .workspace_id;

    let status = fixture
        .post(
            "/deps",
            &format!(
                "workspace_id={workspace_id}&url={}",
                urlencode(&format!("file://{}", upstream.display()))
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a POST redirects back");

    let (status, body) = fixture.get("/deps").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("present"), "{body}");
    assert!(
        body.contains("/deps/") || body.contains("deps"),
        "the path has to be on the page"
    );
}

/// Removing sources is destructive in the only way this console is, so it must
/// be the sources and nothing else.
#[tokio::test]
async fn removing_a_dependency_leaves_the_rest_alone() {
    let fixture = Fixture::new();
    let first = seed_upstream(fixture.dir.path(), "one\n");
    let second = seed_upstream_named(fixture.dir.path(), "other", "two\n");
    let workspace_id = fixture
        .store
        .resolve_workspace_for(fixture.dir.path())
        .expect("resolve")
        .workspace_id;

    for upstream in [&first, &second] {
        fixture
            .post(
                "/deps",
                &format!(
                    "workspace_id={workspace_id}&url={}",
                    urlencode(&format!("file://{}", upstream.display()))
                ),
            )
            .await;
    }

    let declared = fixture.store.dependencies(workspace_id).expect("list");
    assert_eq!(declared.len(), 2);

    let status = fixture
        .post(&format!("/deps/{}/remove", declared[0].id), "")
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let left = fixture.store.dependencies(workspace_id).expect("list");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, declared[1].id);
}

/// A checkout that could not be fetched must read as broken rather than as
/// sources that happen to be empty.
#[tokio::test]
async fn a_failed_checkout_says_why_on_the_page() {
    let fixture = Fixture::new();
    let workspace_id = fixture
        .store
        .resolve_workspace_for(fixture.dir.path())
        .expect("resolve")
        .workspace_id;

    fixture
        .post(
            "/deps",
            &format!(
                "workspace_id={workspace_id}&url={}",
                urlencode(&format!("file://{}/nowhere", fixture.dir.path().display()))
            ),
        )
        .await;

    let (_, body) = fixture.get("/deps").await;
    assert!(body.contains("failed"), "{body}");
    assert!(
        body.to_lowercase().contains("repository")
            || body.to_lowercase().contains("not")
            || body.contains("does not exist"),
        "the reason has to be visible: {body}"
    );
}

fn seed_upstream(root: &std::path::Path, contents: &str) -> std::path::PathBuf {
    seed_upstream_named(root, "retry", contents)
}

fn seed_upstream_named(root: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = root.join("upstream").join(name);
    std::fs::create_dir_all(&path).expect("mkdir");

    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}");
    };

    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "T"]);
    std::fs::write(path.join("retry.go"), contents).expect("write");
    git(&["add", "."]);
    git(&["commit", "-m", "seed"]);
    path
}

/// Enough of one for a file:// URL in a form body.
fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' | ':' => {
                character.to_string()
            }
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}
