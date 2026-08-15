//! Reference checkouts.
//!
//! A dependency is a repository the workspace reads but does not work in: a
//! library whose sources answer questions faster than its documentation, a
//! sibling service whose contract has to be matched.
//!
//! What this deliberately does not build is an index. Once the sources are on
//! disk the agent already has grep and read, and those are faster than a
//! bespoke index, never stale, and cost nothing to maintain. So the whole job
//! here is materialisation: put the right revision at a known path and say
//! where it is.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Utc};
use magent_core::{DependencyId, WorkspaceId};
use rusqlite::{OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::{error::StoreError, git::normalize_origin, store::Store};

/// What the caller asks for.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DependencySpec {
    /// Any form git accepts. Stored as given and normalised for identity.
    pub url: String,
    /// A branch, tag or commit. `None` follows the remote's default branch.
    pub git_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStatus {
    /// Asked for, not yet on disk.
    Declared,
    /// Sources are checked out at [`Dependency::revision`].
    Present,
    /// The last attempt failed; [`Dependency::last_error`] says why.
    Failed,
}

impl DependencyStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Present => "present",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "present" => Self::Present,
            "failed" => Self::Failed,
            _ => Self::Declared,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Dependency {
    pub id: DependencyId,
    pub workspace_id: WorkspaceId,
    /// The URL as typed. Shown back so the answer matches the question.
    pub url: String,
    pub identity_key: String,
    pub git_ref: Option<String>,
    /// The on-disk name, `github.com/acme/thing@v1.2.0`.
    pub slug: String,
    pub status: DependencyStatus,
    pub revision: Option<String>,
    pub synced_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Where a dependency's sources live under `deps_root`.
///
/// A free function rather than a stored column: the path is a function of the
/// root and the slug, and a stored copy could disagree with the filesystem
/// after the state directory moves.
#[must_use]
pub fn dependency_checkout(deps_root: &Path, dependency: &Dependency) -> PathBuf {
    deps_root.join(&dependency.slug)
}

/// Builds the on-disk name for a project and ref.
///
/// A URL decides a path, which makes it attacker-adjacent input: every segment
/// is restricted to characters that cannot traverse, and anything else is
/// replaced rather than rejected, so a legitimate but unusual URL still works.
fn slug_for(identity_key: &str, git_ref: Option<&str>) -> String {
    let project = identity_key
        .split('/')
        .map(sanitise_segment)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");

    match git_ref {
        Some(reference) => format!("{project}@{}", sanitise_segment(reference)),
        None => project,
    }
}

fn sanitise_segment(segment: &str) -> String {
    let mapped: String = segment
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+') {
                character
            } else {
                '-'
            }
        })
        .collect();

    // A single '.' is needed — hostnames and versions have them — but a run of
    // two is what traverses, so runs collapse and leading dots go. Checked on
    // the whole name rather than only on a bare "..", because a name that
    // merely contains ".." can still be misread by anything that resolves the
    // path itself.
    let mut collapsed = String::with_capacity(mapped.len());
    for character in mapped.chars() {
        if character == '.' && collapsed.ends_with('.') {
            continue;
        }
        collapsed.push(character);
    }

    collapsed.trim_start_matches('.').to_owned()
}

impl Store {
    /// Declares a reference checkout for `workspace_id`.
    ///
    /// Idempotent on the project and ref rather than on an operation id:
    /// declaring the same dependency twice is the same request, whoever asks,
    /// and inventing a key for it would only give callers a way to get it wrong.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn declare_dependency(
        &self,
        workspace_id: WorkspaceId,
        spec: &DependencySpec,
    ) -> Result<Dependency, StoreError> {
        let identity_key = normalize_origin(&spec.url);
        let git_ref = spec
            .git_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let slug = slug_for(&identity_key, git_ref);

        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO dependencies
                 (id, workspace_id, url, identity_key, git_ref, slug, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'declared', ?7)
             ON CONFLICT (workspace_id, identity_key, IFNULL(git_ref, ''))
             -- The URL is refreshed rather than ignored: declaring the same
             -- project by its HTTPS form after its SSH form is a correction.
             DO UPDATE SET url = excluded.url",
            (
                DependencyId::new().to_string(),
                workspace_id.to_string(),
                &spec.url,
                &identity_key,
                git_ref,
                &slug,
                Utc::now().to_rfc3339(),
            ),
        )?;

        connection
            .query_row(
                "SELECT id, workspace_id, url, identity_key, git_ref, slug, status,
                        revision, synced_at, last_error
                 FROM dependencies
                 WHERE workspace_id = ?1 AND identity_key = ?2 AND IFNULL(git_ref, '') = ?3",
                (
                    workspace_id.to_string(),
                    &identity_key,
                    git_ref.unwrap_or_default(),
                ),
                read_dependency,
            )
            .map_err(Into::into)
    }

    /// Everything declared for `workspace_id`, oldest first.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn dependencies(&self, workspace_id: WorkspaceId) -> Result<Vec<Dependency>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, workspace_id, url, identity_key, git_ref, slug, status,
                    revision, synced_at, last_error
             FROM dependencies
             WHERE workspace_id = ?1
             ORDER BY created_at, rowid",
        )?;

        let rows = statement.query_map([workspace_id.to_string()], read_dependency)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// # Errors
    /// Fails on a database error, or if no such dependency exists.
    pub fn dependency(&self, id: DependencyId) -> Result<Dependency, StoreError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, workspace_id, url, identity_key, git_ref, slug, status,
                        revision, synced_at, last_error
                 FROM dependencies WHERE id = ?1",
                [id.to_string()],
                read_dependency,
            )
            .optional()?
            .ok_or(StoreError::DependencyNotFound(id))
    }

    /// Brings the checkout up to date, cloning it if it is not there yet.
    ///
    /// A failure to fetch is recorded and returned as a `failed` dependency
    /// rather than an error: a library that cannot be reached right now is a
    /// fact about the workspace, not a reason for the caller to stop.
    ///
    /// # Errors
    /// Fails on a database error. Git failures are recorded, not raised.
    pub fn sync_dependency(
        &self,
        id: DependencyId,
        deps_root: &Path,
    ) -> Result<Dependency, StoreError> {
        let dependency = self.dependency(id)?;
        let checkout = dependency_checkout(deps_root, &dependency);

        match materialise(&dependency, &checkout) {
            Ok(revision) => {
                self.record_sync(id, &revision)?;
                self.dependency(id)
            }
            Err(reason) => {
                // A half-clone would be read as sources on the next question,
                // and answering from a truncated tree is worse than answering
                // "not available".
                let _ = std::fs::remove_dir_all(&checkout);
                self.record_failure(id, &reason)?;
                self.dependency(id)
            }
        }
    }

    fn record_sync(&self, id: DependencyId, revision: &str) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE dependencies
             SET status = ?1, revision = ?2, synced_at = ?3, last_error = NULL
             WHERE id = ?4",
            (
                DependencyStatus::Present.as_str(),
                revision,
                Utc::now().to_rfc3339(),
                id.to_string(),
            ),
        )?;
        Ok(())
    }

    fn record_failure(&self, id: DependencyId, reason: &str) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection.execute(
            // The revision is cleared with the checkout: reporting a commit for
            // sources that are no longer there invites citing them.
            "UPDATE dependencies
             SET status = ?1, revision = NULL, last_error = ?2
             WHERE id = ?3",
            (DependencyStatus::Failed.as_str(), reason, id.to_string()),
        )?;
        Ok(())
    }

    /// Drops the declaration and the sources it materialised.
    ///
    /// # Errors
    /// Fails on a database error.
    pub fn forget_dependency(&self, id: DependencyId, deps_root: &Path) -> Result<(), StoreError> {
        let checkout = self
            .dependency(id)
            .map(|dependency| dependency_checkout(deps_root, &dependency))
            .ok();

        {
            let connection = self.lock()?;
            connection.execute("DELETE FROM dependencies WHERE id = ?1", [id.to_string()])?;
        }

        if let Some(path) = checkout {
            let _ = std::fs::remove_dir_all(&path);
            prune_empty_parents(&path, deps_root);
        }
        Ok(())
    }
}

/// Removes directories left empty by a deletion, up to but never including
/// `deps_root`.
///
/// A slug is a path, so removing `github.com/acme/thing@v1` leaves `acme` and
/// `github.com` behind. A root full of empty shells named after every project
/// ever removed reads as though those are still tracked, and legibility of that
/// one directory is what the whole feature promises.
fn prune_empty_parents(removed: &Path, deps_root: &Path) {
    let mut current = removed.parent();

    while let Some(directory) = current {
        if directory == deps_root || !directory.starts_with(deps_root) {
            return;
        }
        // remove_dir refuses a non-empty directory, which is exactly the guard
        // wanted here: a sibling checkout stops the walk.
        if std::fs::remove_dir(directory).is_err() {
            return;
        }
        current = directory.parent();
    }
}

/// Clones or updates the checkout, returning the revision it now holds.
fn materialise(dependency: &Dependency, checkout: &Path) -> Result<String, String> {
    if checkout.join(".git").is_dir() {
        update(dependency, checkout)?;
    } else {
        clone(dependency, checkout)?;
    }

    head_revision(checkout)
}

fn clone(dependency: &Dependency, checkout: &Path) -> Result<(), String> {
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let mut command = Command::new("git");
    command.arg("clone").arg("--depth").arg("1");
    if let Some(reference) = &dependency.git_ref {
        command.arg("--branch").arg(reference);
    }
    command.arg("--").arg(&dependency.url).arg(checkout);

    if run(&mut command).is_ok() {
        return Ok(());
    }

    // `--branch` takes a branch or a tag and refuses a commit, which is the
    // one form worth pinning to: a commit is the only ref that cannot move
    // under a checkout someone is citing. So the shallow clone is retried
    // without it and the ref fetched directly.
    let _ = std::fs::remove_dir_all(checkout);
    run(Command::new("git")
        .args(["clone", "--depth", "1", "--"])
        .arg(&dependency.url)
        .arg(checkout))?;

    match &dependency.git_ref {
        Some(reference) => update(dependency, checkout)
            .map_err(|error| format!("cloned, but {reference} could not be resolved: {error}")),
        None => Ok(()),
    }
}

fn update(dependency: &Dependency, checkout: &Path) -> Result<(), String> {
    let reference = dependency.git_ref.as_deref().unwrap_or("HEAD");

    run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["fetch", "--depth", "1", "origin"])
        .arg(reference))?;

    // Reset rather than merge: a reference checkout has no local work to
    // preserve, and a merge could leave it in a conflicted state nobody is
    // watching.
    run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["reset", "--hard", "FETCH_HEAD"]))
}

fn head_revision(checkout: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| error.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(stderr_of(&output.stderr))
    }
}

fn run(command: &mut Command) -> Result<(), String> {
    // A reference checkout must never prompt. Without this a private URL hangs
    // a hook or an MCP call on a credential prompt nobody can see or answer.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "0");

    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(stderr_of(&output.stderr))
    }
}

/// Git's diagnostics are verbose and the useful line is usually the last one.
fn stderr_of(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_owned()
}

fn read_dependency(row: &Row<'_>) -> rusqlite::Result<Dependency> {
    let parse_time = |value: Option<String>| {
        value.and_then(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .ok()
                .map(|stamp| stamp.with_timezone(&Utc))
        })
    };

    Ok(Dependency {
        id: row
            .get::<_, String>(0)?
            .parse()
            .unwrap_or_else(|_| DependencyId::new()),
        workspace_id: row
            .get::<_, String>(1)?
            .parse()
            .unwrap_or_else(|_| WorkspaceId::new()),
        url: row.get(2)?,
        identity_key: row.get(3)?,
        git_ref: row.get(4)?,
        slug: row.get(5)?,
        status: DependencyStatus::parse(&row.get::<_, String>(6)?),
        revision: row.get(7)?,
        synced_at: parse_time(row.get(8)?),
        last_error: row.get(9)?,
    })
}
