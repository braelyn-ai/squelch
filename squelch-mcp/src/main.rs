//! squelch-mcp binary. Transport is chosen here and only here: stdio by
//! default, or the MCP Streamable HTTP transport at `/mcp` when given
//! `--http [addr]` / `SQUELCH_MCP_HTTP`. Tool logic lives in [`server`].

use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::stdio;
use squelch_core::store::SqliteStore;
use squelch_mcp::server::SquelchServer;
use squelch_mcp::{MCP_PATH, streamable_http_service};
use tokio_util::sync::CancellationToken;

/// Default bind address for HTTP mode. Loopback only — never default to a
/// non-loopback interface; a reverse proxy is expected to front this listener.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8848";

/// Where the Streamable HTTP transport is mounted; sourced from the lib.
const HTTP_MCP_PATH: &str = MCP_PATH;

/// How the server talks to clients. Selected once, in `main`.
enum Transport {
    /// stdio (the default).
    Stdio,
    /// Streamable HTTP bound to `addr` (loopback by default).
    Http(SocketAddr),
}

/// The operator's shipments LISTING policy, read from the same config file the
/// daemon reads. The standalone bin has no `[carriers]` credentials to care
/// about, but it must still honour `max_failures` / `stale_after_days`: an
/// agent's package list is supposed to match its user's, and defaulting here
/// would reintroduce exactly the drift this value exists to remove.
fn shipment_policy() -> squelch_core::config::ShipmentListPolicy {
    squelch_core::config::Config::load().carriers.list_policy()
}

/// Build the server object, so tests can construct it without binding a transport.
fn build_server() -> anyhow::Result<SquelchServer> {
    let store = Arc::new(SqliteStore::open(squelch_core::config::resolve_db_path())?);
    Ok(
        SquelchServer::new(store, &squelch_core::config::account_email())?
            .with_shipment_policy(shipment_policy()),
    )
}

/// Decide the transport from CLI args and env: `--http [addr]` or
/// `SQUELCH_MCP_HTTP` selects HTTP, otherwise stdio. An explicit `--http <addr>`
/// wins over the env var, which wins over the loopback default — the bind
/// address is never silently widened past loopback.
fn select_transport() -> anyhow::Result<Transport> {
    let mut args = std::env::args().skip(1);
    let mut http_flag = false;
    let mut flag_addr: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" => {
                http_flag = true;
                // Optional inline address: `--http 127.0.0.1:9000`.
                if let Some(next) = args.next() {
                    if next.starts_with('-') {
                        return Err(anyhow::anyhow!("unexpected argument `{next}` after --http"));
                    }
                    flag_addr = Some(next);
                }
            }
            other => {
                return Err(anyhow::anyhow!("unknown argument: {other}"));
            }
        }
    }

    let env_addr = std::env::var("SQUELCH_MCP_HTTP").ok();
    let use_http = http_flag || env_addr.is_some();
    if !use_http {
        return Ok(Transport::Stdio);
    }

    // Flag address wins over env; empty env means "HTTP at the default addr".
    let addr_str = flag_addr
        .or_else(|| env_addr.filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_string());

    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid HTTP bind address `{addr_str}`: {e}"))?;
    Ok(Transport::Http(addr))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match select_transport()? {
        Transport::Stdio => {
            let server = build_server()?;
            let running = server.serve(stdio()).await?;
            running.waiting().await?;
        }
        Transport::Http(addr) => serve_http(addr).await?,
    }
    Ok(())
}

/// Serve the MCP Streamable HTTP transport on `addr` until ctrl-c.
async fn serve_http(addr: SocketAddr) -> anyhow::Result<()> {
    let store = Arc::new(SqliteStore::open(squelch_core::config::resolve_db_path())?);

    let shutdown = CancellationToken::new();
    // Construction errors surface before we bind.
    let service = streamable_http_service(
        store,
        &squelch_core::config::account_email(),
        shipment_policy(),
        shutdown.child_token(),
    )?;

    let router = axum::Router::new().nest_service(HTTP_MCP_PATH, service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr().unwrap_or(addr);

    // No tokens or message content are ever logged.
    eprintln!("squelch-mcp: serving MCP Streamable HTTP on http://{bound}{HTTP_MCP_PATH}");

    // On ctrl-c, cancel the token: that terminates active MCP sessions.
    let shutdown_signal = {
        let shutdown = shutdown.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("squelch-mcp: shutting down");
            shutdown.cancel();
        }
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the server constructs and exposes exactly 7 tools, no transport.
    #[test]
    fn constructs_server_with_seven_tools() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
        let server = SquelchServer::new(store, "me@localhost").expect("server");
        assert_eq!(server.tool_count(), 7, "expected exactly 7 MCP tools");
    }
}
