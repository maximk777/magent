use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior};

use crate::error::StoreError;

/// Schema version this build writes and understands.
pub const CURRENT_VERSION: i64 = 3;

const MIGRATION_0001: &str = include_str!("../migrations/0001_slice1.sql");
const MIGRATION_0002: &str = include_str!("../migrations/0002_facts.sql");
const MIGRATION_0003: &str = include_str!("../migrations/0003_retrieval.sql");

/// Brings `connection` up to [`CURRENT_VERSION`].
///
/// Safe to call concurrently from several processes: the work happens inside an
/// immediate transaction, so the loser waits and then observes the applied
/// schema instead of applying it twice.
pub fn apply(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let installed = read_version(&transaction)?;
    if installed > CURRENT_VERSION {
        return Err(StoreError::UnsupportedSchema(installed));
    }

    for (version, sql) in [
        (1, MIGRATION_0001),
        (2, MIGRATION_0002),
        (3, MIGRATION_0003),
    ] {
        if installed < version {
            transaction.execute_batch(sql)?;
            transaction.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                (version, Utc::now().to_rfc3339()),
            )?;
        }
    }

    transaction.commit()?;
    Ok(())
}

/// Reads the installed version, treating "no `schema_migrations` table" as 0.
pub fn read_version(connection: &Connection) -> Result<i64, StoreError> {
    let table_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        return Ok(0);
    }

    let version: Option<i64> =
        connection.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;

    Ok(version.unwrap_or(0))
}
