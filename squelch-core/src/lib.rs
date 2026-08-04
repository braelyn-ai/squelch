//! squelch-core: domain types, storage, and triage for squelch.
//!
//! Sealed (auth-related) messages are ABSENT from every agent-door query, never
//! redacted — see docs/SECURITY.md §4, [`store`], and [`triage::seal`].

pub mod auth;
pub mod config;
pub mod credentials;
pub mod embed;
pub mod error;
pub mod push;
pub mod store;
pub mod sync;
pub mod tracking;
pub mod triage;
pub mod types;

pub use error::{CoreError, Result};
