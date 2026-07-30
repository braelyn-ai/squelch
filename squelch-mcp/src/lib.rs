//! squelch-mcp: MCP server exposing squelch's read-mostly tools (agent door).
//!
//! [`streamable_http_service`] is the one construction path for the HTTP
//! service, shared by the MCP bin and `squelchd serve`. Sealed (auth-related)
//! messages are structurally absent from every tool result — see docs/SECURITY.md.

pub mod server;

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use squelch_core::store::SqliteStore;
use tokio_util::sync::CancellationToken;

pub use server::SquelchServer;

/// Path the Streamable HTTP transport is mounted at; clients connect to
/// `http://<addr>/mcp`. Shared so the MCP bin and `squelchd serve` agree.
pub const MCP_PATH: &str = "/mcp";

/// The concrete Streamable HTTP service type.
pub type SquelchHttpService = StreamableHttpService<SquelchServer, LocalSessionManager>;

/// Build the MCP Streamable HTTP tower `Service` for the agent door.
///
/// The one place the service is constructed, so the door is identical whichever
/// binary hosts it. Cancelling `cancellation` terminates active MCP sessions.
pub fn streamable_http_service(
    store: Arc<SqliteStore>,
    account_email: &str,
    cancellation: CancellationToken,
) -> anyhow::Result<SquelchHttpService> {
    let template = SquelchServer::new(store, account_email)?;
    // DNS-rebinding guard: rmcp defaults to loopback-only Host headers, which
    // 403s requests proxied by `tailscale serve` (Host: *.ts.net). Additive —
    // loopback is never dropped.
    let allowed_hosts = squelch_core::config::mcp_allowed_hosts();
    Ok(StreamableHttpService::new(
        move || Ok(template.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(cancellation)
            .with_allowed_hosts(allowed_hosts),
    ))
}
