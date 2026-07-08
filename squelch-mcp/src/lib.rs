//! squelch-mcp: MCP server exposing squelch's 5 read-mostly tools (AGENT DOOR).
//!
//! Tool logic lives in [`server`] and is transport-agnostic. The MCP bin
//! (`main.rs`) picks stdio vs Streamable HTTP; the unified `squelchd serve`
//! process mounts the SAME Streamable HTTP service at `/mcp` alongside the
//! human door. To keep exactly one construction path for the HTTP service, both
//! callers go through [`streamable_http_service`].
//!
//! SECURITY: sealed (auth-related) messages are structurally ABSENT from every
//! tool result (SQL exclusion in `squelch-core` + a re-filter in [`server`]).
//! This crate adds NO write capability; the agent door stays narrow.

pub mod server;

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use squelch_core::store::SqliteStore;
use tokio_util::sync::CancellationToken;

pub use server::SquelchServer;

/// Path the Streamable HTTP transport is mounted at. Clients connect to
/// `http://<addr>/mcp`. Shared so both the MCP bin and `squelchd serve` mount
/// the door at the identical location.
pub const MCP_PATH: &str = "/mcp";

/// The concrete Streamable HTTP service type, spelled once so callers do not
/// have to name rmcp's generic soup.
pub type SquelchHttpService = StreamableHttpService<SquelchServer, LocalSessionManager>;

/// Build the MCP Streamable HTTP tower `Service` for the agent door.
///
/// This is the ONE place the service is constructed. The MCP bin's `--http`
/// mode and `squelchd serve` both call it, so the agent door is byte-for-byte
/// identical no matter which binary hosts it: same tools, same sealed-absent
/// guarantees, zero write capability.
///
/// A fresh [`SquelchServer`] is handed to each session via the factory (it only
/// wraps an `Arc<SqliteStore>`, so cloning is cheap). The provided
/// `cancellation` token, when cancelled, terminates active MCP sessions — wire
/// it to your shutdown signal for graceful teardown.
pub fn streamable_http_service(
    store: Arc<SqliteStore>,
    account_email: &str,
    cancellation: CancellationToken,
) -> anyhow::Result<SquelchHttpService> {
    let template = SquelchServer::new(store, account_email)?;
    Ok(StreamableHttpService::new(
        move || Ok(template.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_cancellation_token(cancellation),
    ))
}
