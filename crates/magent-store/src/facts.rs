//! Durable memory: storing facts, superseding them, and finding them again.
//!
//! Two properties carry this module. A new value never destroys the old one, so
//! a wrong turn can be diagnosed later. And retrieval is scoped, so working on
//! one project does not drag in everything the user has ever learned elsewhere
//! — memory that returns noise is worse than no memory.

use std::path::Path;

use chrono::Utc;
use magent_core::{
    Cardinality, Evidence, Fact, FactId, FactKind, FactScope, FactStatus, FactSummary,
    RelationKind, RememberCommand, RunId, Validate, WorkspaceId,
};
use rusqlite::{Transaction, TransactionBehavior};

use crate::{
    error::StoreError,
    store::{Store, enum_from_sql, enum_to_sql, parse_id, parse_timestamp},
};

/// Where a fact is being written from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactContext {
    pub workspace_id: Option<WorkspaceId>,
    pub run_id: Option<RunId>,
    /// The name memory is filed under when no workspace is known yet, which is
    /// the case for everything imported from the markdown corpus.
    pub namespace: Option<String>,
    /// How the fact was obtained. Imported memory is marked so it can be
    /// re-imported idempotently and told apart from what was learned here.
    pub provenance: String,
}

impl Default for FactContext {
    fn default() -> Self {
        Self {
            workspace_id: None,
            run_id: None,
            namespace: None,
            provenance: "session".to_owned(),
        }
    }
}

/// What to retrieve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactQuery {
    /// Free text. `None` returns whatever is in scope, most specific first.
    pub text: Option<String>,
    /// Namespaces the caller is working in. See [`namespace_candidates`].
    pub namespaces: Vec<String>,
    pub workspace_id: Option<WorkspaceId>,
    pub limit: usize,
}

impl Default for FactQuery {
    fn default() -> Self {
        Self {
            text: None,
            namespaces: Vec::new(),
            workspace_id: None,
            // Small on purpose: results are read by a model with a context
            // budget, and a long list is skimmed rather than used.
            limit: 10,
        }
    }
}

/// Namespaces a repository at `root` might have its memory filed under.
///
/// The existing corpus files projects both as the bare directory name and as
/// `<parent>-<name>`, so both are offered. This is what lets 100-odd imported
/// facts attach themselves without anyone binding them by hand.
#[must_use]
pub fn namespace_candidates(root: &Path) -> Vec<String> {
    let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };

    let mut candidates = vec![name.clone()];

    if let Some(parent) = root
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
    {
        candidates.push(format!("{parent}-{name}"));
    }

    candidates
}

impl Store {
    /// Writes a fact, superseding the previous value when cardinality demands.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Domain`] for an invalid command, or a database
    /// error.
    pub fn remember(
        &self,
        command: &RememberCommand,
        context: &FactContext,
    ) -> Result<FactId, StoreError> {
        command.validate()?;

        self.execute_operation("remember", command.operation_id, command, |tx| {
            let now = Utc::now().to_rfc3339();
            let fact_id = FactId::new();

            tx.execute(
                "INSERT INTO facts (
                     id, name, title, body, kind, scope, cardinality, status, confidence,
                     workspace_id, run_id, namespace, provenance, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                rusqlite::params![
                    fact_id.to_string(),
                    &command.name,
                    &command.title,
                    &command.body,
                    enum_to_sql(&command.kind)?,
                    enum_to_sql(&command.scope)?,
                    enum_to_sql(&command.cardinality)?,
                    enum_to_sql(&command.status)?,
                    command.confidence,
                    context.workspace_id.map(|id| id.to_string()),
                    context.run_id.map(|id| id.to_string()),
                    context.namespace.as_deref(),
                    &context.provenance,
                    &now,
                ],
            )?;

            if command.cardinality.conflicts_within_scope() {
                // After the insert, because superseded_by references the new
                // row; and excluding it, because it also has a null
                // superseded_by and would otherwise supersede itself.
                //
                // The predecessor is marked, never deleted: what was believed
                // earlier is how a later wrong turn gets diagnosed.
                tx.execute(
                    "UPDATE facts SET superseded_by = ?1, updated_at = ?2
                     WHERE name = ?3 AND superseded_by IS NULL AND id <> ?1
                       AND namespace IS ?4 AND scope = ?5",
                    (
                        fact_id.to_string(),
                        &now,
                        &command.name,
                        context.namespace.as_deref(),
                        enum_to_sql(&command.scope)?,
                    ),
                )?;
            }

            for evidence in &command.evidence {
                tx.execute(
                    "INSERT INTO fact_evidence (id, fact_id, locator, excerpt, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        uuid::Uuid::new_v4().to_string(),
                        fact_id.to_string(),
                        &evidence.locator,
                        evidence.excerpt.as_deref(),
                        &now,
                    ),
                )?;
            }

            for (target, predicate) in &command.relates_to {
                tx.execute(
                    "INSERT INTO fact_relations (id, from_fact_id, to_name, predicate, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        uuid::Uuid::new_v4().to_string(),
                        fact_id.to_string(),
                        target,
                        enum_to_sql(predicate)?,
                        &now,
                    ),
                )?;
            }

            Ok(fact_id)
        })
    }

    /// The current value of a named fact, if it is visible from `context`.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn recall(&self, name: &str, context: &FactContext) -> Result<Option<Fact>, StoreError> {
        let namespaces: Vec<String> = context.namespace.clone().into_iter().collect();
        let found = self.search(&FactQuery {
            text: None,
            namespaces,
            workspace_id: context.workspace_id,
            limit: 1024,
        })?;

        Ok(found.into_iter().find(|fact| fact.name == name))
    }

    /// Every value a name has held, newest first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn fact_history(&self, name: &str, context: &FactContext) -> Result<Vec<Fact>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;

        let mut statement = tx.prepare(
            "SELECT id, name, title, body, kind, scope, cardinality, status, confidence,
                    namespace, updated_at
             FROM facts
             WHERE name = ?1 AND namespace IS ?2
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = statement
            .query_map((name, context.namespace.as_deref()), |row| {
                Ok(row_to_parts(row))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut facts = Vec::new();
        for parts in rows {
            facts.push(build_fact(&tx, parts?)?);
        }
        drop(tx);

        Ok(facts)
    }

    /// Facts visible from `query`, most specific and most trusted first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn search(&self, query: &FactQuery) -> Result<Vec<Fact>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let parts = select_visible(&tx, query)?;

        let mut facts = Vec::new();
        for part in parts {
            facts.push(build_fact(&tx, part)?);
        }
        drop(tx);

        Ok(facts)
    }

    /// The same selection as [`Store::search`], without bodies.
    ///
    /// This is what gets injected on every prompt, so it tells the model what
    /// exists and lets it ask for the rest. Carrying bodies here would cost as
    /// much as the search it is meant to avoid.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn fact_index(&self, query: &FactQuery) -> Result<Vec<FactSummary>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let parts = select_visible(&tx, query)?;
        drop(tx);

        Ok(parts
            .into_iter()
            .map(|part| FactSummary {
                fact_id: part.fact_id,
                name: part.name,
                title: part.title,
                kind: part.kind,
                scope: part.scope,
                status: part.status,
            })
            .collect())
    }

    /// Facts worth pushing into `session`'s context that it has not seen yet.
    ///
    /// Recording what was pushed is what keeps a long session from paying for
    /// the same handful of facts on every single turn.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn unpushed_index(
        &self,
        session: &str,
        query: &FactQuery,
    ) -> Result<Vec<FactSummary>, StoreError> {
        let candidates = self.fact_index(query)?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        let mut fresh = Vec::new();
        for candidate in candidates {
            let inserted = tx.execute(
                "INSERT INTO retrieval_events (external_session_hint, fact_id, pushed_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (external_session_hint, fact_id) DO NOTHING",
                (session, candidate.fact_id.to_string(), &now),
            )?;

            // A row that was already there means this session has seen it.
            if inserted == 1 {
                fresh.push(candidate);
            }
        }
        tx.commit()?;

        Ok(fresh)
    }

    /// Records what `root` declares about its toolchain, once.
    ///
    /// Detection is cheap but not free, and its facts only change when a
    /// manifest does, so it runs when a repository is first seen rather than on
    /// every session. Returns how many facts were written; zero means either
    /// the repository declares nothing or it has already been read.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn detect_toolchain_once(
        &self,
        root: &Path,
        context: &FactContext,
    ) -> Result<usize, StoreError> {
        if self.has_detected_facts(context)? {
            return Ok(0);
        }

        let detected = crate::toolchain::detect_toolchain(root);
        let mut written = 0;

        for command in &detected {
            self.remember(
                command,
                &FactContext {
                    provenance: "detected".to_owned(),
                    ..context.clone()
                },
            )?;
            written += 1;
        }

        Ok(written)
    }

    /// Whether this repository's manifests have already been read.
    fn has_detected_facts(&self, context: &FactContext) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM facts
                 WHERE provenance = 'detected' AND namespace IS ?1 AND superseded_by IS NULL
             )",
            [context.namespace.as_deref()],
            |row| row.get(0),
        )?)
    }

    /// Every namespace memory has been filed under.
    ///
    /// Used by the exporter, which must reach all of them rather than only the
    /// project it happens to be standing in.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn known_namespaces(&self) -> Result<Vec<String>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT DISTINCT namespace FROM facts WHERE namespace IS NOT NULL")?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Names one fact links to, addressed by id.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn relations_of_id(&self, fact_id: FactId) -> Result<Vec<String>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT to_name FROM fact_relations WHERE from_fact_id = ?1 ORDER BY created_at",
        )?;
        let rows = statement
            .query_map([fact_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Names this fact links to, with the kind of link.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn relations_of(
        &self,
        name: &str,
        context: &FactContext,
    ) -> Result<Vec<(String, RelationKind)>, StoreError> {
        let Some(fact) = self.recall(name, context)? else {
            return Ok(Vec::new());
        };

        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT to_name, predicate FROM fact_relations WHERE from_fact_id = ?1
             ORDER BY created_at",
        )?;
        let rows = statement
            .query_map([fact.fact_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(target, predicate)| Ok((target, enum_from_sql(&predicate)?)))
            .collect()
    }
}

// --- selection -------------------------------------------------------------

struct FactParts {
    fact_id: FactId,
    name: String,
    title: String,
    body: String,
    kind: FactKind,
    scope: FactScope,
    cardinality: Cardinality,
    status: FactStatus,
    confidence: f64,
    namespace: Option<String>,
    updated_at: String,
}

/// Facts in scope for `query`.
///
/// Visibility is the whole point of the memory layer: user-level facts are
/// about the person and travel everywhere, while anything narrower must belong
/// to a namespace or workspace the caller is actually in.
fn select_visible(tx: &Transaction<'_>, query: &FactQuery) -> Result<Vec<FactParts>, StoreError> {
    let namespace_list = json_array(&query.namespaces);
    let workspace = query.workspace_id.map(|id| id.to_string());
    let limit = i64::try_from(query.limit.max(1)).unwrap_or(i64::MAX);

    let visibility = "
        f.superseded_by IS NULL
        AND f.status <> 'revoked'
        AND (
            f.scope = 'user'
            OR (f.namespace IS NOT NULL AND f.namespace IN (SELECT value FROM json_each(?1)))
            OR (?2 IS NOT NULL AND f.workspace_id = ?2)
        )";

    // Without a text query, specificity decides: a repository-level fact should
    // outrank a general one, since it was learned here.
    let ordering = "
        ORDER BY CASE f.scope
                     WHEN 'run' THEN 0 WHEN 'repository' THEN 1
                     WHEN 'workspace' THEN 2 ELSE 3 END,
                 f.confidence DESC,
                 f.updated_at DESC
        LIMIT ?3";

    // With one, relevance decides. OR semantics mean weak matches are in the
    // result set, so bm25 is what keeps them out of the top of it. Lower is a
    // better match.
    let ranked_ordering = "
        ORDER BY bm25(facts_fts), f.confidence DESC, f.updated_at DESC
        LIMIT ?3";

    let mut rows: Vec<FactParts> = Vec::new();

    if let Some(expression) = fts_expression(query.text.as_deref()) {
        let sql = format!(
            "SELECT f.id, f.name, f.title, f.body, f.kind, f.scope, f.cardinality,
                        f.status, f.confidence, f.namespace, f.updated_at, bm25(facts_fts)
                 FROM facts_fts
                 JOIN facts f ON f.rowid = facts_fts.rowid
                 WHERE facts_fts MATCH ?4 AND {visibility} {ranked_ordering}"
        );
        let mut statement = tx.prepare(&sql)?;
        let mapped = statement.query_map(
            rusqlite::params![&namespace_list, &workspace, limit, &expression],
            |row| Ok((row.get::<_, f64>(11)?, row_to_parts(row))),
        )?;

        let mut scored = Vec::new();
        for entry in mapped {
            let (score, part) = entry?;
            scored.push((score, part?));
        }

        // bm25 is negative and more negative is better, so the best score is
        // the smallest. Anything less than half as good is noise, and with only
        // a handful of slots it would displace something that is on topic.
        if let Some(best) = scored.first().map(|(score, _)| *score) {
            let cutoff = best * RELEVANCE_FLOOR;
            scored.retain(|(score, _)| *score <= cutoff);
        }

        rows.extend(scored.into_iter().map(|(_, part)| part));
    } else {
        let sql = format!(
            "SELECT f.id, f.name, f.title, f.body, f.kind, f.scope, f.cardinality,
                        f.status, f.confidence, f.namespace, f.updated_at
                 FROM facts f WHERE {visibility} {ordering}"
        );
        let mut statement = tx.prepare(&sql)?;
        let mapped = statement.query_map(
            rusqlite::params![&namespace_list, &workspace, limit],
            |row| Ok(row_to_parts(row)),
        )?;
        for part in mapped {
            // Two layers: rusqlite's row error, then our own conversion error.
            rows.push(part??);
        }
    }

    Ok(rows)
}

/// How much worse than the best match a result may be and still be shown.
///
/// Relative rather than absolute: bm25 scores depend on corpus size and term
/// distribution, so a fixed threshold would be meaningless on a small store and
/// wrong on a large one. Halfway to the best match is the cut.
const RELEVANCE_FLOOR: f64 = 0.5;

/// Shortest token worth matching on.
///
/// Below this the words are almost all grammar — "the", "on", "a" — and with
/// OR semantics they would match everything and rank nothing.
const MIN_TOKEN: usize = 3;

/// Builds an FTS5 expression from free text.
///
/// Tokens are joined with OR, not AND. The text is often a whole prompt rather
/// than chosen keywords, and requiring every word to appear in one fact means
/// retrieval that almost never fires. Precision comes from ranking instead: see
/// the bm25 ordering in [`select_visible`].
///
/// Tokens are extracted and quoted rather than passed through, because the text
/// comes from a model or a raw prompt and an unbalanced quote or a bare `*`
/// would turn a search into a syntax error.
fn fts_expression(text: Option<&str>) -> Option<String> {
    let tokens: Vec<String> = text?
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.chars().count() >= MIN_TOKEN)
        .map(|token| format!("\"{token}\""))
        .collect();

    (!tokens.is_empty()).then(|| tokens.join(" OR "))
}

fn json_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

fn row_to_parts(row: &rusqlite::Row<'_>) -> Result<FactParts, StoreError> {
    Ok(FactParts {
        fact_id: parse_id(&row.get::<_, String>(0)?)?,
        name: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        kind: enum_from_sql(&row.get::<_, String>(4)?)?,
        scope: enum_from_sql(&row.get::<_, String>(5)?)?,
        cardinality: enum_from_sql(&row.get::<_, String>(6)?)?,
        status: enum_from_sql(&row.get::<_, String>(7)?)?,
        confidence: row.get(8)?,
        namespace: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn build_fact(tx: &Transaction<'_>, parts: FactParts) -> Result<Fact, StoreError> {
    let mut statement = tx.prepare(
        "SELECT locator, excerpt FROM fact_evidence WHERE fact_id = ?1 ORDER BY created_at",
    )?;
    let evidence = statement
        .query_map([parts.fact_id.to_string()], |row| {
            Ok(Evidence {
                locator: row.get(0)?,
                excerpt: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Fact {
        fact_id: parts.fact_id,
        name: parts.name,
        title: parts.title,
        body: parts.body,
        kind: parts.kind,
        scope: parts.scope,
        cardinality: parts.cardinality,
        status: parts.status,
        confidence: parts.confidence,
        namespace: parts.namespace,
        evidence,
        updated_at: parse_timestamp(&parts.updated_at)?,
    })
}
