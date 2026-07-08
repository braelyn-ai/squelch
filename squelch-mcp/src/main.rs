//! squelch-mcp: stdio MCP server exposing squelch's 5 read-mostly tools.
//!
//! Transport is chosen HERE and only here. Tool logic lives in [`server`] and
//! is transport-agnostic, so swapping stdio for SSE/streamable-http later is a
//! one-line change in `run`.

mod server;

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::stdio;
use squelch_core::store::SqliteStore;

use crate::server::SquelchServer;

/// Resolve the SQLite path from `SQUELCH_DB`, falling back to the user data dir.
fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("SQUELCH_DB") {
        return PathBuf::from(p);
    }
    // Default: ~/.local/share/squelch/squelch.db (XDG-ish), else CWD.
    if let Ok(home) = std::env::var("HOME") {
        let dir = PathBuf::from(home).join(".local/share/squelch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("squelch.db");
    }
    PathBuf::from("squelch.db")
}

/// The account this server operates on. Multi-account selection is future work;
/// the schema already carries `account_id` everywhere.
fn account_email() -> String {
    std::env::var("SQUELCH_ACCOUNT").unwrap_or_else(|_| "me@localhost".to_string())
}

/// Build the server object. Split out so the smoke test can construct it without
/// binding a transport.
fn build_server() -> anyhow::Result<SquelchServer> {
    let store = Arc::new(SqliteStore::open(db_path())?);
    SquelchServer::new(store, &account_email())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = build_server()?;

    // Transport selection lives ONLY here. To add SSE later, branch on config
    // and call `.serve(sse_transport)` instead — tool code is untouched.
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the server object constructs and exposes exactly the 5 tools
    /// over an in-memory store, without binding any transport.
    #[test]
    fn constructs_server_with_five_tools() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
        let server = SquelchServer::new(store, "me@localhost").expect("server");
        assert_eq!(server.tool_count(), 5, "expected exactly 5 MCP tools");
    }
}
