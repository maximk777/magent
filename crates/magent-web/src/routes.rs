//! What the console does.
//!
//! Reads render a page; curation actions are POSTs that do one thing and
//! redirect back, so the browser's own back button and reload behave the way a
//! person expects. Only the fact list is swapped in place, because filtering a
//! long list on every keystroke is the one interaction where a full page load
//! would be felt.

use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use magent_core::{Evidence, FactId, FactScope, FactStatus};
use magent_store::FactFilter;
use serde::Deserialize;

use crate::{
    Console,
    view::{self, FactView},
};

const STYLESHEET: &str = include_str!("../static/app.css");
const HTMX: &str = include_str!("../static/htmx.min.js");

pub fn router(console: Console) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/facts", get(facts))
        .route("/facts/rows", get(fact_rows))
        .route("/facts/{id}", get(fact_detail))
        .route("/facts/{id}/verify", post(verify))
        .route("/facts/{id}/revoke", post(revoke))
        .route("/facts/{id}/reinstate", post(reinstate))
        .route("/facts/{id}/edit", post(edit))
        .route("/facts/{id}/scope", post(rescope))
        .route("/facts/{id}/merge", post(merge))
        .route("/runs", get(runs))
        .route("/runs/{id}/close", post(close_run))
        .route("/workspaces", get(workspaces))
        .route("/workspaces/promote", post(promote))
        // Served from the binary rather than from disk: the console has to work
        // from any directory, and an install that lost its assets would be a
        // blank page with no explanation.
        .route("/static/app.css", get(stylesheet))
        .route("/static/htmx.min.js", get(htmx))
        .with_state(console)
}

// --- templates -------------------------------------------------------------

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    section: &'static str,
    database: String,
    overview: magent_store::Overview,
    duplicates: Vec<(FactView, FactView)>,
}

#[derive(Template)]
#[template(path = "facts.html")]
struct FactsPage {
    section: &'static str,
    facts: Vec<FactView>,
    namespaces: Vec<String>,
    filter_text: String,
    filter_namespace: String,
    filter_status: String,
    filter_scope: String,
}

#[derive(Template)]
#[template(path = "fact_rows.html")]
struct FactRows {
    facts: Vec<FactView>,
}

#[derive(Template)]
#[template(path = "fact_detail.html")]
struct FactPage {
    section: &'static str,
    fact: FactView,
    history: Vec<FactView>,
    relations: Vec<String>,
    workspaces: Vec<(String, usize)>,
}

struct RunRow {
    run_id: String,
    task: String,
    stage: String,
    status: String,
    open: bool,
    updated_at: String,
}

#[derive(Template)]
#[template(path = "runs.html")]
struct RunsPage {
    section: &'static str,
    runs: Vec<RunRow>,
}

#[derive(Template)]
#[template(path = "workspaces.html")]
struct WorkspacesPage {
    section: &'static str,
    groups: Vec<(String, usize)>,
    namespaces: Vec<(String, usize)>,
}

// --- reads -----------------------------------------------------------------

async fn overview(State(console): State<Console>) -> Result<Html<String>, Failure> {
    let page = OverviewPage {
        section: "overview",
        database: console.database.display().to_string(),
        overview: console.store.overview()?,
        // A handful only: this is a prompt to look, not a work queue.
        duplicates: console
            .store
            .duplicate_candidates(6)?
            .iter()
            .map(|(left, right)| (FactView::of(left), FactView::of(right)))
            .collect(),
    };

    render(&page)
}

#[derive(Debug, Default, Deserialize)]
struct FactsQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    scope: String,
}

impl FactsQuery {
    fn to_filter(&self) -> FactFilter {
        FactFilter {
            namespace: non_empty(&self.namespace),
            scope: parse_scope(&self.scope),
            kind: None,
            status: parse_status(&self.status),
            text: non_empty(&self.q),
            limit: Some(300),
        }
    }
}

async fn facts(
    State(console): State<Console>,
    Query(query): Query<FactsQuery>,
) -> Result<Html<String>, Failure> {
    let page = FactsPage {
        section: "facts",
        facts: views(&console.store.browse_facts(&query.to_filter())?),
        namespaces: {
            let mut all = console.store.known_namespaces()?;
            all.sort_unstable();
            all
        },
        filter_text: query.q.clone(),
        filter_namespace: query.namespace.clone(),
        filter_status: query.status.clone(),
        filter_scope: query.scope.clone(),
    };

    render(&page)
}

/// The rows alone, for the filter bar to swap in.
async fn fact_rows(
    State(console): State<Console>,
    Query(query): Query<FactsQuery>,
) -> Result<Html<String>, Failure> {
    render(&FactRows {
        facts: views(&console.store.browse_facts(&query.to_filter())?),
    })
}

async fn fact_detail(
    State(console): State<Console>,
    Path(id): Path<String>,
) -> Result<Html<String>, Failure> {
    let fact_id = parse_id(&id)?;
    let fact = console.store.fact(fact_id)?.ok_or(Failure::NotFound)?;

    let context = magent_store::FactContext {
        namespace: fact.namespace.clone(),
        ..magent_store::FactContext::default()
    };

    let history: Vec<FactView> = console
        .store
        .fact_history(&fact.name, &context)?
        .iter()
        .filter(|earlier| earlier.fact_id != fact.fact_id)
        .map(FactView::of)
        .collect();

    let page = FactPage {
        section: "facts",
        relations: console.store.relations_of_id(fact.fact_id)?,
        workspaces: console.store.workspaces()?,
        history,
        fact: FactView::of(&fact),
    };

    render(&page)
}

async fn runs(State(console): State<Console>) -> Result<Html<String>, Failure> {
    let rows = console
        .store
        .recent_runs(100)?
        .into_iter()
        .map(|run| RunRow {
            run_id: run.run_id.to_string(),
            task: run.task,
            stage: run.stage,
            open: run.status == "open",
            status: run.status,
            // Elapsed time, as everywhere else: an RFC 3339 instant makes the
            // reader do the subtraction.
            updated_at: chrono::DateTime::parse_from_rfc3339(&run.updated_at)
                .map(|when| view::when(when.with_timezone(&chrono::Utc)))
                .unwrap_or(run.updated_at),
        })
        .collect();

    render(&RunsPage {
        section: "runs",
        runs: rows,
    })
}

async fn workspaces(State(console): State<Console>) -> Result<Html<String>, Failure> {
    render(&WorkspacesPage {
        section: "workspaces",
        groups: console.store.workspaces()?,
        namespaces: console.store.namespace_counts()?,
    })
}

// --- curation --------------------------------------------------------------

#[derive(Deserialize)]
struct VerifyForm {
    locator: String,
}

async fn verify(
    State(console): State<Console>,
    Path(id): Path<String>,
    Form(form): Form<VerifyForm>,
) -> Result<Redirect, Failure> {
    let fact_id = parse_id(&id)?;
    let locator = form.locator.trim();

    // Refused rather than silently ignored: confirming with nothing behind it
    // is exactly what the status is supposed to prevent.
    if locator.is_empty() {
        return Err(Failure::BadRequest(
            "confirming a fact needs something that backs it".into(),
        ));
    }

    console.store.verify_fact(
        fact_id,
        &[Evidence {
            locator: locator.to_owned(),
            excerpt: None,
        }],
    )?;

    Ok(Redirect::to(&format!("/facts/{id}")))
}

#[derive(Deserialize)]
struct RevokeForm {
    #[serde(default)]
    reason: String,
}

async fn revoke(
    State(console): State<Console>,
    Path(id): Path<String>,
    Form(form): Form<RevokeForm>,
) -> Result<Redirect, Failure> {
    let reason = if form.reason.trim().is_empty() {
        "withdrawn from the console"
    } else {
        form.reason.trim()
    };

    console.store.revoke_fact(parse_id(&id)?, reason)?;
    Ok(Redirect::to("/facts"))
}

async fn reinstate(
    State(console): State<Console>,
    Path(id): Path<String>,
) -> Result<Redirect, Failure> {
    console
        .store
        .set_fact_status(parse_id(&id)?, FactStatus::Observed)?;
    Ok(Redirect::to("/facts"))
}

#[derive(Deserialize)]
struct EditForm {
    title: String,
    body: String,
}

async fn edit(
    State(console): State<Console>,
    Path(id): Path<String>,
    Form(form): Form<EditForm>,
) -> Result<Redirect, Failure> {
    let replacement =
        console
            .store
            .edit_fact(parse_id(&id)?, form.title.trim(), form.body.trim())?;

    // To the new version, not the old one: the old is now history, and landing
    // there after saving would look like the edit had not taken.
    Ok(Redirect::to(&format!("/facts/{replacement}")))
}

#[derive(Deserialize)]
struct ScopeForm {
    scope: String,
    #[serde(default)]
    workspace: String,
}

async fn rescope(
    State(console): State<Console>,
    Path(id): Path<String>,
    Form(form): Form<ScopeForm>,
) -> Result<Redirect, Failure> {
    let scope = parse_scope(&form.scope)
        .ok_or_else(|| Failure::BadRequest(format!("no such scope: {}", form.scope)))?;

    let workspace = match non_empty(&form.workspace) {
        Some(name) => console.store.workspace_id_by_name(&name)?,
        None => None,
    };

    console
        .store
        .set_fact_scope(parse_id(&id)?, scope, workspace)?;
    Ok(Redirect::to(&format!("/facts/{id}")))
}

#[derive(Deserialize)]
struct MergeForm {
    duplicate: String,
}

async fn merge(
    State(console): State<Console>,
    Path(id): Path<String>,
    Form(form): Form<MergeForm>,
) -> Result<Redirect, Failure> {
    console
        .store
        .merge_facts(parse_id(&id)?, parse_id(&form.duplicate)?)?;
    Ok(Redirect::to("/"))
}

async fn close_run(
    State(console): State<Console>,
    Path(id): Path<String>,
) -> Result<Redirect, Failure> {
    let run_id = id
        .parse()
        .map_err(|_| Failure::BadRequest(format!("not an id: {id}")))?;

    console
        .store
        .close_run_from_console(run_id, "closed from the console")?;
    Ok(Redirect::to("/runs"))
}

#[derive(Deserialize)]
struct PromoteForm {
    namespace: String,
    workspace: String,
}

async fn promote(
    State(console): State<Console>,
    Form(form): Form<PromoteForm>,
) -> Result<Redirect, Failure> {
    let workspace = console
        .store
        .workspace_id_by_name(&form.workspace)?
        .ok_or_else(|| Failure::BadRequest(format!("no workspace called {}", form.workspace)))?;

    console
        .store
        .promote_namespace(&form.namespace, workspace)?;
    Ok(Redirect::to("/workspaces"))
}

// --- assets ----------------------------------------------------------------

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLESHEET,
    )
}

async fn htmx() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        HTMX,
    )
}

// --- plumbing --------------------------------------------------------------

fn views(facts: &[magent_core::Fact]) -> Vec<FactView> {
    facts.iter().map(FactView::of).collect()
}

fn render<T: Template>(page: &T) -> Result<Html<String>, Failure> {
    page.render()
        .map(Html)
        .map_err(|error| Failure::Render(error.to_string()))
}

fn parse_id(raw: &str) -> Result<FactId, Failure> {
    raw.parse()
        .map_err(|_| Failure::BadRequest(format!("not an id: {raw}")))
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn parse_scope(raw: &str) -> Option<FactScope> {
    match raw {
        "user" => Some(FactScope::User),
        "workspace" => Some(FactScope::Workspace),
        "repository" => Some(FactScope::Repository),
        "run" => Some(FactScope::Run),
        _ => None,
    }
}

fn parse_status(raw: &str) -> Option<FactStatus> {
    match raw {
        "observed" => Some(FactStatus::Observed),
        "inferred" => Some(FactStatus::Inferred),
        "verified" => Some(FactStatus::Verified),
        "contradicted" => Some(FactStatus::Contradicted),
        "stale" => Some(FactStatus::Stale),
        "revoked" => Some(FactStatus::Revoked),
        _ => None,
    }
}

/// What can go wrong, and what the person sees.
///
/// Plain text rather than a styled page: these are all either a broken link or
/// a bug, and a reader is better served by the reason than by chrome.
enum Failure {
    NotFound,
    BadRequest(String),
    Store(magent_store::StoreError),
    Render(String),
}

impl From<magent_store::StoreError> for Failure {
    fn from(error: magent_store::StoreError) -> Self {
        Self::Store(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "no such fact".to_owned()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Store(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            Self::Render(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };

        (status, message).into_response()
    }
}
