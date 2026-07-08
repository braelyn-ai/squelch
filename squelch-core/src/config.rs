//! Config stub. Everything becomes config-driven later; nothing magic hardcoded.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the SQLite store.
    pub db_path: PathBuf,
    /// Default minimum importance for surfacing updates.
    pub default_min_importance: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("squelch.db"),
            default_min_importance: 0,
        }
    }
}
