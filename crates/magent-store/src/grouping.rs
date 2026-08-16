//! Gathering repositories that belong together.
//!
//! A repository is rarely the right unit for everything that is known. How a
//! group of services authenticate to each other, or how their deploy pipeline
//! behaves, is true of all of them — and gets filed under whichever one happened
//! to be open when it was learned. Grouping is what lets that reach the rest.
//!
//! Grouping is always explicit. Guessing from directory layout is wrong often
//! enough — vendored checkouts, forks, scratch clones — that a wrong guess would
//! silently merge unrelated projects' memory, which is the failure that makes a
//! memory layer worth switching off.

use std::path::{Path, PathBuf};

use chrono::Utc;
use magent_core::{FactScope, OperationId, RepositoryRole, WorkspaceId};
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};

use crate::{
    error::StoreError,
    git,
    store::{Store, enum_to_sql, parse_id, upsert_repository},
};

/// A grouping asked for once.
///
/// The key is what the whole type exists for: [`Store::execute_operation`]
/// records the request beside it, so a replay carrying the same key and a
/// different name or different roots is refused rather than answered from a
/// record of something else.
#[derive(Clone, Debug, Serialize)]
pub struct GroupingCommand {
    pub operation_id: OperationId,
    /// What to call the group.
    pub name: String,
    /// Every checkout the group is to contain.
    pub roots: Vec<PathBuf>,
}

/// What a grouping call did.
///
/// Serialised as well as returned: it is the recorded answer a retry gets back
/// instead of grouping a second time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceGrouping {
    pub workspace_id: WorkspaceId,
    /// How many repositories now belong to it.
    pub repositories: usize,
    /// Paths that were not grouped, with the reason. Reported rather than
    /// registered: a path that is not there is a mistake in the call, and
    /// taking it at face value leaves a row keyed on nonsense.
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

impl Store {
    /// Gathers `roots` into the workspace called `name`, creating it if needed.
    ///
    /// Idempotent: calling it again with the same or additional roots extends
    /// the same workspace rather than making a second one of the same name.
    ///
    /// Unkeyed, so every call is its own operation. That is what a person
    /// typing the command means; a caller that can be retried on its behalf
    /// wants [`Store::apply_grouping`] instead.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn group_into_workspace(
        &self,
        name: &str,
        roots: &[PathBuf],
    ) -> Result<WorkspaceGrouping, StoreError> {
        self.apply_grouping(&GroupingCommand {
            operation_id: OperationId::new(),
            name: name.to_owned(),
            roots: roots.to_vec(),
        })
    }

    /// As [`Store::group_into_workspace`], but recorded against a key.
    ///
    /// A retry carrying the same `operation_id` gets the first call's answer
    /// back without touching anything. Grouping is idempotent on the workspace
    /// name only while nothing else moved — a checkout taken out of the group
    /// in between would be dragged back in — so the key is what makes a retry
    /// safe, not the shape of the SQL.
    ///
    /// # Errors
    /// Returns [`StoreError::IdempotencyConflict`] when the key was already
    /// used for a different request, or a database error.
    pub fn apply_grouping(
        &self,
        command: &GroupingCommand,
    ) -> Result<WorkspaceGrouping, StoreError> {
        let mut skipped = Vec::new();
        let mut probes = Vec::new();

        // Probed before the transaction, as everywhere else: git subprocesses
        // must not hold the single write lock. A replay pays for the probes and
        // then throws them away, which is the cheaper half of the trade.
        for root in &command.roots {
            if root.is_dir() {
                probes.push(git::discover(root));
            } else {
                skipped.push((root.clone(), "not a directory".to_owned()));
            }
        }

        self.execute_operation("group", command.operation_id, command, move |tx| {
            let now = Utc::now().to_rfc3339();

            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM workspaces WHERE name = ?1 AND explicit = 1",
                    [&command.name],
                    |row| row.get(0),
                )
                .ok();

            let workspace_id = if let Some(id) = existing {
                parse_id(&id)?
            } else {
                let id = WorkspaceId::new();
                tx.execute(
                    "INSERT INTO workspaces (id, name, created_at, explicit)
                     VALUES (?1, ?2, ?3, 1)",
                    (id.to_string(), &command.name, &now),
                )?;
                id
            };

            for probe in &probes {
                // Ensures the repository exists, then moves it. Each root may be
                // seen for the first time here.
                let resolved = upsert_repository(tx, probe, &now)?;
                tx.execute(
                    "UPDATE repositories SET workspace_id = ?1 WHERE id = ?2",
                    (workspace_id.to_string(), resolved.repository_id.to_string()),
                )?;
            }

            // Workspaces left with no repositories are the remains of the
            // single-repository defaults these roots used to sit in.
            tx.execute(
                "DELETE FROM workspaces
                 WHERE id NOT IN (SELECT DISTINCT workspace_id FROM repositories)
                   AND id NOT IN (SELECT DISTINCT workspace_id FROM runs)
                   AND id <> ?1",
                [workspace_id.to_string()],
            )?;

            let repositories: i64 = tx.query_row(
                "SELECT COUNT(*) FROM repositories WHERE workspace_id = ?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )?;

            Ok(WorkspaceGrouping {
                workspace_id,
                repositories: usize::try_from(repositories).unwrap_or(0),
                skipped,
            })
        })
    }

    /// Records how freely a repository may be touched.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn set_repository_role(&self, root: &Path, role: RepositoryRole) -> Result<(), StoreError> {
        let probe = git::discover(root);

        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();

        let resolved = upsert_repository(&tx, &probe, &now)?;
        tx.execute(
            "UPDATE repositories SET role = ?1 WHERE id = ?2",
            (enum_to_sql(&role)?, resolved.repository_id.to_string()),
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Moves a namespace's facts up to workspace scope.
    ///
    /// Imported memory is filed under whichever directory it was written in. A
    /// namespace that turns out to describe how a whole group fits together is
    /// only useful once it can be seen from the other repositories in it.
    ///
    /// Returns how many facts moved.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn promote_namespace(
        &self,
        namespace: &str,
        workspace_id: WorkspaceId,
    ) -> Result<usize, StoreError> {
        let connection = self.lock()?;
        let moved = connection.execute(
            "UPDATE facts SET scope = ?1, workspace_id = ?2, updated_at = ?3
             WHERE namespace = ?4 AND superseded_by IS NULL AND scope <> 'user'",
            (
                enum_to_sql(&FactScope::Workspace)?,
                workspace_id.to_string(),
                Utc::now().to_rfc3339(),
                namespace,
            ),
        )?;

        Ok(moved)
    }

    /// A workspace's id, by the name it was grouped under.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn workspace_id_by_name(&self, name: &str) -> Result<Option<WorkspaceId>, StoreError> {
        let connection = self.lock()?;
        let found: Option<String> = connection
            .query_row(
                "SELECT id FROM workspaces WHERE name = ?1 AND explicit = 1",
                [name],
                |row| row.get(0),
            )
            .ok();

        found.map(|id| parse_id(&id)).transpose()
    }

    /// # Errors
    /// Fails on a database error.
    pub fn workspace_count(&self) -> Result<usize, StoreError> {
        let connection = self.lock()?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Every workspace with its id, named groups first.
    ///
    /// Unlike [`Store::workspaces`] this includes the implicit ones — a
    /// workspace created for a single repository is still somewhere a
    /// dependency can be declared, and hiding it would make the console
    /// unable to offer the one that a terminal already resolved.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn workspace_names(&self) -> Result<Vec<(String, WorkspaceId)>, StoreError> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare("SELECT name, id FROM workspaces ORDER BY explicit DESC, name")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .filter_map(|(name, id)| id.parse().ok().map(|id| (name, id)))
            .collect())
    }

    /// Every workspace, with how many repositories it holds.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn workspaces(&self) -> Result<Vec<(String, usize)>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT w.name, COUNT(r.id) FROM workspaces w
             LEFT JOIN repositories r ON r.workspace_id = w.id
             WHERE w.explicit = 1
             GROUP BY w.id ORDER BY w.name",
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
}
