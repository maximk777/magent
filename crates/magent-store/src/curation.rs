//! Curating memory by hand.
//!
//! Everything else writes memory automatically, from prompts and transcripts
//! and manifests. This is the half a person does: confirming what is true,
//! withdrawing what is not, correcting wording, and merging the near-duplicates
//! that automation inevitably produces.
//!
//! Nothing here destroys anything. A withdrawal is reversible, a correction
//! supersedes rather than overwrites, and a merge leaves a relation pointing at
//! what was folded in. Curation is where mistakes are made deliberately, and it
//! should never be frightening.

use chrono::Utc;
use magent_core::{
    Cardinality, Evidence, Fact, FactId, FactKind, FactScope, FactStatus, WorkspaceId,
};
use rusqlite::{OptionalExtension, TransactionBehavior};

use crate::{
    error::StoreError,
    facts::FactQuery,
    store::{Store, enum_to_sql},
};

/// How far a confirmed fact is trusted.
///
/// Not 1.0: a person confirming something is better evidence than a model
/// asserting it, and still not certainty.
const VERIFIED_CONFIDENCE: f64 = 0.95;

/// What a console shows at a glance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Overview {
    /// Facts currently retrievable.
    pub facts: usize,
    pub revoked: usize,
    pub superseded: usize,
    pub namespaces: usize,
    pub runs_open: usize,
    pub runs_completed: usize,
    pub repositories: usize,
    pub workspaces: usize,
    /// Background work still owed.
    pub jobs_pending: usize,
    pub jobs_failed: usize,
}

/// Which facts to list.
///
/// Browsing is not searching: someone opening a console wants to see what is
/// there, narrowed, rather than guess a query that reveals it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FactFilter {
    pub namespace: Option<String>,
    pub scope: Option<FactScope>,
    pub kind: Option<FactKind>,
    /// `None` lists everything currently retrievable; a status lists exactly
    /// that status, including the withdrawn ones retrieval hides.
    pub status: Option<FactStatus>,
    pub text: Option<String>,
    pub limit: Option<usize>,
}

impl Store {
    // --- reading -----------------------------------------------------------

    /// One fact by id, whatever its status.
    ///
    /// Unlike retrieval, this shows withdrawn and superseded facts: a console
    /// that could not open what it had just withdrawn would be useless.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn fact(&self, fact_id: FactId) -> Result<Option<Fact>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let found = crate::facts::load_fact(&tx, fact_id)?;
        drop(tx);
        Ok(found)
    }

    /// Facts matching `filter`, newest first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn browse_facts(&self, filter: &FactFilter) -> Result<Vec<Fact>, StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let found = crate::facts::browse(&tx, filter)?;
        drop(tx);
        Ok(found)
    }

    /// The counts a console leads with.
    ///
    /// Read from the same tables retrieval uses, so what is shown cannot drift
    /// from what an agent actually sees.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn overview(&self) -> Result<Overview, StoreError> {
        let connection = self.lock()?;
        let count = |sql: &str| -> Result<usize, StoreError> {
            let value: i64 = connection.query_row(sql, [], |row| row.get(0))?;
            Ok(usize::try_from(value).unwrap_or(0))
        };

        Ok(Overview {
            facts: count(
                "SELECT COUNT(*) FROM facts
                 WHERE superseded_by IS NULL AND status <> 'revoked'",
            )?,
            revoked: count("SELECT COUNT(*) FROM facts WHERE status = 'revoked'")?,
            superseded: count("SELECT COUNT(*) FROM facts WHERE superseded_by IS NOT NULL")?,
            namespaces: count(
                "SELECT COUNT(DISTINCT namespace) FROM facts WHERE namespace IS NOT NULL",
            )?,
            runs_open: count("SELECT COUNT(*) FROM runs WHERE status = 'open'")?,
            runs_completed: count("SELECT COUNT(*) FROM runs WHERE status = 'completed'")?,
            repositories: count("SELECT COUNT(*) FROM repositories")?,
            workspaces: count("SELECT COUNT(*) FROM workspaces WHERE explicit = 1")?,
            jobs_pending: count("SELECT COUNT(*) FROM jobs WHERE status IN ('pending','running')")?,
            jobs_failed: count("SELECT COUNT(*) FROM jobs WHERE status = 'failed'")?,
        })
    }

    // --- confirming and withdrawing ----------------------------------------

    /// Marks a fact confirmed, attaching what confirms it.
    ///
    /// # Errors
    ///
    /// Refuses without evidence: the strongest status must not be the cheapest
    /// to assert, by hand any more than by tool.
    pub fn verify_fact(&self, fact_id: FactId, evidence: &[Evidence]) -> Result<(), StoreError> {
        if evidence.is_empty() {
            return Err(StoreError::Domain(
                magent_core::DomainError::VerifiedWithoutEvidence,
            ));
        }

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        for item in evidence {
            tx.execute(
                "INSERT INTO fact_evidence (id, fact_id, locator, excerpt, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    uuid::Uuid::new_v4().to_string(),
                    fact_id.to_string(),
                    &item.locator,
                    item.excerpt.as_deref(),
                    &now,
                ),
            )?;
        }

        tx.execute(
            "UPDATE facts SET status = ?1, confidence = MAX(confidence, ?2), updated_at = ?3
             WHERE id = ?4",
            (
                enum_to_sql(&FactStatus::Verified)?,
                VERIFIED_CONFIDENCE,
                &now,
                fact_id.to_string(),
            ),
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Withdraws a fact, recording why.
    ///
    /// Reversible through [`Store::set_fact_status`]: changing one's mind is
    /// ordinary, and a permanent withdrawal would make curation frightening.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn revoke_fact(&self, fact_id: FactId, reason: &str) -> Result<(), StoreError> {
        let connection = self.lock()?;
        let now = Utc::now().to_rfc3339();

        // The reason goes in the body rather than a column: it is the part
        // someone reads when they find the fact later and wonder.
        connection.execute(
            "UPDATE facts
             SET status = ?1,
                 body = body || char(10) || char(10) || 'Withdrawn: ' || ?2,
                 updated_at = ?3
             WHERE id = ?4 AND status <> ?1",
            (
                enum_to_sql(&FactStatus::Revoked)?,
                reason,
                &now,
                fact_id.to_string(),
            ),
        )?;

        Ok(())
    }

    /// Sets a fact's status directly. Used to reinstate a withdrawal.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn set_fact_status(&self, fact_id: FactId, status: FactStatus) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE facts SET status = ?1, updated_at = ?2 WHERE id = ?3",
            (
                enum_to_sql(&status)?,
                Utc::now().to_rfc3339(),
                fact_id.to_string(),
            ),
        )?;
        Ok(())
    }

    // --- correcting --------------------------------------------------------

    /// Rewrites a fact as a new version, superseding the old one.
    ///
    /// Returns the new fact's id. What was believed before stays readable,
    /// because a decision may have been made on it.
    ///
    /// # Errors
    /// Fails on a database error, or if the fact does not exist.
    pub fn edit_fact(
        &self,
        fact_id: FactId,
        title: &str,
        body: &str,
    ) -> Result<FactId, StoreError> {
        let existing = self
            .fact(fact_id)?
            .ok_or(StoreError::Serialization(format!("no fact {fact_id}")))?;

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        let replacement = FactId::new();

        tx.execute(
            "INSERT INTO facts (
                 id, name, title, body, kind, scope, cardinality, status, confidence,
                 workspace_id, run_id, namespace, provenance, created_at, updated_at
             )
             SELECT ?1, name, ?2, ?3, kind, scope, cardinality, status, confidence,
                    workspace_id, run_id, namespace, 'curated', ?4, ?4
             FROM facts WHERE id = ?5",
            (
                replacement.to_string(),
                title,
                body,
                &now,
                fact_id.to_string(),
            ),
        )?;

        tx.execute(
            "UPDATE facts SET superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
            (replacement.to_string(), &now, fact_id.to_string()),
        )?;
        tx.commit()?;

        let _ = existing;
        Ok(replacement)
    }

    /// Moves one fact to another scope.
    ///
    /// Unlike promoting a whole namespace, this is for the single observation
    /// that turns out to be general.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn set_fact_scope(
        &self,
        fact_id: FactId,
        scope: FactScope,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE facts SET scope = ?1, workspace_id = ?2, updated_at = ?3 WHERE id = ?4",
            (
                enum_to_sql(&scope)?,
                workspace_id.map(|id| id.to_string()),
                Utc::now().to_rfc3339(),
                fact_id.to_string(),
            ),
        )?;
        Ok(())
    }

    // --- merging -----------------------------------------------------------

    /// Folds `duplicate` into `keep`.
    ///
    /// The duplicate stops being retrieved and gains a relation from the one
    /// that survived, so a later reader can see that two accounts were merged
    /// rather than that one silently vanished.
    ///
    /// # Errors
    ///
    /// Refuses to merge a fact into itself, which is always a mistake in the
    /// caller rather than an instruction.
    pub fn merge_facts(&self, keep: FactId, duplicate: FactId) -> Result<(), StoreError> {
        if keep == duplicate {
            return Err(StoreError::Serialization(
                "a fact cannot be merged into itself".into(),
            ));
        }

        let duplicate_name: Option<String> = {
            let connection = self.lock()?;
            connection
                .query_row(
                    "SELECT name FROM facts WHERE id = ?1",
                    [duplicate.to_string()],
                    |row| row.get(0),
                )
                .optional()?
        };

        let Some(duplicate_name) = duplicate_name else {
            return Ok(());
        };

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        // Superseded, not contradicted: the two say the same thing twice, and
        // marking a disagreement that does not exist would mislead whoever
        // reads it later. Superseding is also what removes it from retrieval,
        // which is the point of merging.
        tx.execute(
            "UPDATE facts SET superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
            (keep.to_string(), &now, duplicate.to_string()),
        )?;

        tx.execute(
            "INSERT INTO fact_relations (id, from_fact_id, to_name, predicate, created_at)
             VALUES (?1, ?2, ?3, 'supersedes', ?4)",
            (
                uuid::Uuid::new_v4().to_string(),
                keep.to_string(),
                &duplicate_name,
                &now,
            ),
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Facts that look like duplicates of each other.
    ///
    /// Deliberately crude — a shared title word is enough to be worth a human
    /// glance, and anything cleverer would be guessing on the person's behalf.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn duplicate_candidates(&self, limit: usize) -> Result<Vec<(Fact, Fact)>, StoreError> {
        let facts = self.browse_facts(&FactFilter {
            limit: Some(500),
            ..FactFilter::default()
        })?;

        let mut pairs = Vec::new();

        for (index, left) in facts.iter().enumerate() {
            for right in facts.iter().skip(index + 1) {
                if left.namespace != right.namespace {
                    continue;
                }
                if looks_like_the_same_thing(&left.title, &right.title) {
                    pairs.push((left.clone(), right.clone()));
                    if pairs.len() >= limit {
                        return Ok(pairs);
                    }
                }
            }
        }

        Ok(pairs)
    }

    /// Everything currently retrievable, for export and for the console.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn all_live_facts(&self) -> Result<Vec<Fact>, StoreError> {
        self.search(&FactQuery {
            text: None,
            namespaces: self.known_namespaces()?,
            workspace_id: None,
            limit: usize::MAX,
        })
    }
}

/// A run as the console lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRow {
    pub run_id: magent_core::RunId,
    pub task: String,
    pub stage: String,
    pub status: String,
    pub updated_at: String,
}

impl Store {
    /// Recent runs, most recently touched first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRow>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, task, stage, status, updated_at FROM runs
             ORDER BY updated_at DESC, rowid DESC LIMIT ?1",
        )?;

        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(100)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(id, task, stage, status, updated_at)| {
                Ok(RunRow {
                    run_id: crate::store::parse_id(&id)?,
                    task,
                    stage,
                    status,
                    updated_at,
                })
            })
            .collect()
    }

    /// How many facts each namespace holds.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn namespace_counts(&self) -> Result<Vec<(String, usize)>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT namespace, COUNT(*) FROM facts
             WHERE namespace IS NOT NULL AND superseded_by IS NULL AND status <> 'revoked'
             GROUP BY namespace ORDER BY COUNT(*) DESC, namespace",
        )?;

        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .map(|(name, count)| (name, usize::try_from(count).unwrap_or(0)))
            .collect())
    }

    /// Closes a run so it stops being restored into new sessions.
    ///
    /// Distinct from `finish_run`, which a model calls with an outcome it is
    /// claiming. This is a person tidying up, and says so.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn close_run_from_console(
        &self,
        run_id: magent_core::RunId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE runs SET status = 'completed', stage = 'completed', outcome = ?1,
                             updated_at = ?2
             WHERE id = ?3 AND status = 'open'",
            (reason, Utc::now().to_rfc3339(), run_id.to_string()),
        )?;
        Ok(())
    }
}

/// Whether two titles look like the same thing said twice.
///
/// Overlap, not a shared word. Everything filed under one project shares its
/// vocabulary, so a single common term pairs facts that merely sit next to each
/// other — and a duplicates list that is mostly noise teaches the reader to
/// skip it, which is worse than not having one.
///
/// Deliberately still crude: this only decides what is worth a person's glance,
/// and anything cleverer would be deciding on their behalf.
fn looks_like_the_same_thing(left: &str, right: &str) -> bool {
    /// Shorter words are grammar and shared by everything.
    const MIN_LENGTH: usize = 5;
    /// Below this the two are about one subject rather than being one claim.
    const MIN_OVERLAP: f64 = 0.5;
    /// One word in common is a topic; two start to be a repetition.
    const MIN_SHARED: usize = 2;

    let words = |text: &str| -> Vec<String> {
        let mut found: Vec<String> = text
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| word.chars().count() >= MIN_LENGTH)
            .map(str::to_lowercase)
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    };

    let left_words = words(left);
    let right_words = words(right);

    if left_words.is_empty() || right_words.is_empty() {
        return false;
    }

    let shared = left_words
        .iter()
        .filter(|word| right_words.contains(word))
        .count();

    if shared < MIN_SHARED {
        return false;
    }

    // Against the smaller side: a short title fully contained in a long one is
    // still a repetition, and dividing by the union would hide it.
    let smaller = left_words.len().min(right_words.len());
    #[expect(
        clippy::cast_precision_loss,
        reason = "title word counts are far below f64's exact range"
    )]
    let overlap = shared as f64 / smaller as f64;

    overlap >= MIN_OVERLAP
}

/// Cardinality is part of what curation may change, so the console can say a
/// value competes with others rather than sitting beside them.
impl Store {
    /// # Errors
    /// Fails on a database error.
    pub fn set_fact_cardinality(
        &self,
        fact_id: FactId,
        cardinality: Cardinality,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE facts SET cardinality = ?1, updated_at = ?2 WHERE id = ?3",
            (
                enum_to_sql(&cardinality)?,
                Utc::now().to_rfc3339(),
                fact_id.to_string(),
            ),
        )?;
        Ok(())
    }
}
