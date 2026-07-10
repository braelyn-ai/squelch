//! squelch-mcp: MCP server exposing squelch's 6 read-mostly tools.
//!
//! Transport is chosen HERE and only here. Tool logic lives in [`server`] and
//! is transport-agnostic. Default (no args) is stdio, exactly as before; passing
//! `--http [addr]` (or setting `SQUELCH_MCP_HTTP`) serves the MCP Streamable HTTP
//! transport instead. The endpoint is mounted at `/mcp`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::stdio;
use squelch_core::store::SqliteStore;
use squelch_mcp::server::SquelchServer;
use squelch_mcp::{MCP_PATH, streamable_http_service};
use tokio_util::sync::CancellationToken;

/// Default bind address for HTTP mode. Loopback ONLY: a reverse proxy
/// (e.g. `tailscale serve`) is expected to front this listener. We never
/// default to a non-loopback interface.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8848";

/// Path the Streamable HTTP transport is mounted at. Clients connect to
/// `http://<addr>/mcp`. Sourced from the lib so the bin and `squelchd serve`
/// agree.
const HTTP_MCP_PATH: &str = MCP_PATH;

/// How the server talks to clients. Selected once, in `main`.
enum Transport {
    /// stdio: the default, unchanged behavior.
    Stdio,
    /// Streamable HTTP bound to `addr` (loopback by default).
    Http(SocketAddr),
}

/// Resolve the SQLite path via core config's single source of truth: canonical
/// `SQUELCH_DB_PATH` > legacy `SQUELCH_DB` (deprecated) > the shared XDG default.
fn db_path() -> PathBuf {
    squelch_core::config::resolve_db_path()
}

/// The account this server operates on. Canonical `SQUELCH_ACCOUNT_EMAIL` >
/// legacy `SQUELCH_ACCOUNT` (deprecated) > default. Multi-account selection is
/// future work; the schema already carries `account_id` everywhere.
fn account_email() -> String {
    squelch_core::config::resolve_account_email("me@localhost")
}

/// Build the server object. Split out so the smoke test can construct it without
/// binding a transport.
fn build_server() -> anyhow::Result<SquelchServer> {
    let store = Arc::new(SqliteStore::open(db_path())?);
    SquelchServer::new(store, &account_email())
}

/// Decide the transport from CLI args and env, without touching tool logic.
///
/// Rules:
/// - No `--http` flag and no `SQUELCH_MCP_HTTP` env => stdio (default, unchanged).
/// - `--http` (optionally followed by an address) => HTTP. An explicit address
///   wins over the env var, which wins over the loopback default.
/// - `SQUELCH_MCP_HTTP` set (to an address or empty) => HTTP, unless overridden
///   by an explicit `--http <addr>`.
///
/// The bind address always defaults to loopback; we never silently widen it.
fn select_transport() -> anyhow::Result<Transport> {
    // Skip argv[0].
    let mut args = std::env::args().skip(1);
    let mut http_flag = false;
    let mut flag_addr: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" => {
                http_flag = true;
                // Optional inline address: `--http 127.0.0.1:9000`. Peek the
                // next arg; only consume it if it isn't another flag.
                if let Some(next) = args.next() {
                    if next.starts_with('-') {
                        return Err(anyhow::anyhow!(
                            "unexpected argument `{next}` after --http"
                        ));
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
    // Transport selection lives ONLY here. Tool code is untouched either way.
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
///
/// rmcp exposes the transport as a tower `Service`; axum hosts it at `/mcp`.
/// A fresh [`SquelchServer`] is handed to each session via the service factory
/// (it is cheap to clone — it only wraps an `Arc<SqliteStore>`).
async fn serve_http(addr: SocketAddr) -> anyhow::Result<()> {
    let store = Arc::new(SqliteStore::open(db_path())?);

    let shutdown = CancellationToken::new();
    // Construction errors surface before we bind. The service is built through
    // the shared lib fn so the agent door is identical to `squelchd serve`.
    let service = streamable_http_service(store, &account_email(), shutdown.child_token())?;

    let router = axum::Router::new().nest_service(HTTP_MCP_PATH, service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr().unwrap_or(addr);

    // Single startup line on stderr. No tokens or message content are ever logged.
    eprintln!("squelch-mcp: serving MCP Streamable HTTP on http://{bound}{HTTP_MCP_PATH}");

    // Graceful shutdown: on ctrl-c, cancel the token (terminates active MCP
    // sessions) and stop accepting connections.
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

    /// Smoke test: the server object constructs and exposes exactly the 7 tools
    /// over an in-memory store, without binding any transport.
    #[test]
    fn constructs_server_with_seven_tools() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
        let server = SquelchServer::new(store, "me@localhost").expect("server");
        assert_eq!(server.tool_count(), 7, "expected exactly 7 MCP tools");
    }
}
