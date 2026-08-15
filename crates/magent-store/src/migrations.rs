use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior};

use crate::error::StoreError;

/// Schema version this build writes and understands.
pub const CURRENT_VERSION: i64 = 1;

const MIGRATION_0001: &str = include_str!("../migrations/0001_slice1.sql");

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

    if installed < 1 {
        transaction.execute_batch(MIGRATION_0001)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            (CURRENT_VERSION, Utc::now().to_rfc3339()),
        )?;
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
