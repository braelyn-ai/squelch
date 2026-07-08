//! squelch-core: domain types, storage, and triage for the squelch local-first
//! email intelligence tool.
//!
//! SECURITY INVARIANT: sealed (auth-related) messages are INVISIBLE to any
//! MCP-facing query — absent, not redacted. See [`store`] and [`triage::seal`].

pub mod auth;
pub mod config;
pub mod credentials;
pub mod error;
pub mod store;
pub mod sync;
pub mod triage;
pub mod types;

pub use error::{CoreError, Result};
