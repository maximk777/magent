//! The local console.
//!
//! A console rather than a dashboard: the point is to curate memory by hand —
//! confirm what is true, withdraw what is not, correct wording, fold duplicates
//! together — because that is the one job automation cannot do and the one that
//! keeps a memory layer from rotting.
//!
//! It is not the daemon the architecture deliberately does without. It opens
//! the same `SQLite` file every hook and the MCP server open, holds nothing,
//! and can be closed at any moment without anything else noticing.
//!
//! Bound to loopback only. It is a read-write view of a personal memory with no
//! authentication, so it must never be reachable from anywhere else.

mod routes;
mod view;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use magent_store::Store;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct Console {
    pub store: Arc<Store>,
    /// Shown on the overview, so it is obvious which profile is open.
    pub database: PathBuf,
}

/// Builds the console's router.
///
/// Exposed so tests can drive it without binding a port.
pub fn router(console: Console) -> Router {
    routes::router(console)
}

/// Serves the console on loopback.
///
/// # Errors
///
/// Fails if the port is taken or the listener cannot be bound.
pub async fn serve(console: Console, port: u16) -> anyhow::Result<()> {
    // Loopback only, never 0.0.0.0: this is an unauthenticated read-write view
    // of a personal memory.
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(address).await?;

    let bound = listener.local_addr()?;
    println!("magent console on http://{bound}");

    axum::serve(listener, router(console)).await?;
    Ok(())
}
