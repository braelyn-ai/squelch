//! The warden's whole memory: one flat JSON file.
//!
//! A box holds tens of tenants, not thousands, and every field here is written
//! once at provision time and read on the next request. SQLite would buy
//! nothing but a schema migration story. The file is mode 0600 and written
//! temp-then-rename by [`crate::host::Fs::write_file`], so a crash mid-write
//! leaves the previous state intact rather than half a JSON document.
//!
//! It holds no secret: labels, mailbox addresses, ports, and stamps. The
//! mailbox address is here because it is what re-rendering an env file needs,
//! and it never reaches a log line or a response body.
//!
//! **A state file that will not parse is a refusal, not an empty state.** An
//! empty state would hand port 9100 to the next signup while a live tenant is
//! already on it, and would report every existing tenant as unknown. So load
//! errors propagate all the way to a 500, and the recovery is the documented
//! rebuild in `deploy/hosted/SETUP.md`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::PORT_RANGE;
use crate::host::{Fs, HostError};

/// Bumped when the shape below changes incompatibly. A file from the future is
/// refused rather than reinterpreted.
pub const STATE_VERSION: u32 = 1;

/// Mode of the state file. It is not secret, but it names every tenant on the
/// box and there is no reason for anything but the warden to read it.
pub const STATE_MODE: u32 = 0o600;

/// Where a tenant is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    /// Written before the first side effect and replaced on success. Seeing it
    /// on a load means the warden died mid-provision: the port is reserved, the
    /// tenant is not serving, and `DELETE` is the way out.
    Provisioning,
    /// Provisioned and enabled.
    Active,
    /// `DELETE`d: unit stopped and disabled, route removed, data dir kept.
    Stopped,
}

/// One tenant, as the warden remembers it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantRecord {
    pub label: String,
    /// PRIVACY: never logged, never in a response body. Here because a rebuild
    /// of an env file needs it.
    pub account_email: String,
    pub port: u16,
    pub status: TenantStatus,
    /// RFC3339, UTC.
    pub created_at: String,
}

/// The file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    /// Keyed by label. `BTreeMap` so the serialized file has a stable order and
    /// a diff between two versions of it is readable.
    pub tenants: BTreeMap<String, TenantRecord>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            tenants: BTreeMap::new(),
        }
    }
}

/// Why the state file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("state file at {path} is not valid JSON: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("state file at {path} is version {found}; this warden speaks version {STATE_VERSION}")]
    Version { path: String, found: u32 },
    #[error("no free port in {base}..{end}; the box is full or the state file has leaked ports")]
    PortsExhausted { base: u16, end: u16 },
}

/// Read the state file. A missing file is an empty state (first boot); an
/// unreadable or unparseable one is an error, deliberately.
pub fn load(fs: &dyn Fs, path: &Path) -> Result<StateFile, StateError> {
    let Some(bytes) = fs.read_file(path)? else {
        return Ok(StateFile::default());
    };
    // An empty file is what a truncating write that died before its first byte
    // would leave. The atomic write makes that impossible for us, but an
    // operator with an editor is not bound by our discipline, and "" is not
    // valid JSON anyway — treat it as the first-boot case rather than a parse
    // error nobody can act on.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(StateFile::default());
    }
    let state: StateFile = serde_json::from_slice(&bytes).map_err(|e| StateError::Parse {
        path: path.to_string_lossy().into_owned(),
        source: e,
    })?;
    if state.version != STATE_VERSION {
        return Err(StateError::Version {
            path: path.to_string_lossy().into_owned(),
            found: state.version,
        });
    }
    Ok(state)
}

/// Write the state file.
pub fn save(fs: &dyn Fs, path: &Path, state: &StateFile) -> Result<(), StateError> {
    // Pretty because a human reads this during an incident, and it is a few
    // dozen records.
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| StateError::Parse {
        path: path.to_string_lossy().into_owned(),
        source: e,
    })?;
    fs.write_file(path, &bytes, STATE_MODE)?;
    Ok(())
}

/// The lowest port from `base` that no record claims and nothing is listening
/// on.
///
/// Both halves matter and they fail differently. A port another RECORD claims
/// is a port that will come back when that tenant is restarted, so it stays
/// reserved even while the tenant is stopped. A port something else on the box
/// is LISTENING on is a port whose tenant would fail to start ten seconds from
/// now with an error nobody would connect back to this decision.
pub fn allocate_port(state: &StateFile, fs: &dyn Fs, base: u16) -> Result<u16, StateError> {
    let claimed: std::collections::BTreeSet<u16> =
        state.tenants.values().map(|t| t.port).collect();
    let end = base.saturating_add(PORT_RANGE);
    for port in base..end {
        if !claimed.contains(&port) && fs.port_free(port) {
            return Ok(port);
        }
    }
    Err(StateError::PortsExhausted { base, end })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockFs, test_config};

    fn record(label: &str, port: u16) -> TenantRecord {
        TenantRecord {
            label: label.to_string(),
            account_email: format!("{label}@example.com"),
            port,
            status: TenantStatus::Active,
            created_at: "2026-08-07T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn missing_state_file_is_an_empty_state() {
        let fs = MockFs::new();
        let state = load(&fs, Path::new("/var/lib/squelch/warden/state.json")).unwrap();
        assert!(state.tenants.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn round_trips_at_mode_0600() {
        let fs = MockFs::new();
        let path = test_config().state_file();
        let mut state = StateFile::default();
        state
            .tenants
            .insert("alice".to_string(), record("alice", 9100));
        save(&fs, &path, &state).unwrap();

        assert_eq!(fs.file(&path).unwrap().1, 0o600);
        let reloaded = load(&fs, &path).unwrap();
        assert_eq!(reloaded.tenants["alice"].port, 9100);
        assert_eq!(reloaded.tenants["alice"].status, TenantStatus::Active);
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_not_an_empty_state() {
        // The whole reason this is not a silent default: an empty state would
        // hand alice's port to the next signup.
        let fs = MockFs::new();
        let path = test_config().state_file();
        fs.write_file(&path, b"{not json", 0o600).unwrap();
        assert!(matches!(
            load(&fs, &path).unwrap_err(),
            StateError::Parse { .. }
        ));

        fs.write_file(&path, br#"{"version":99,"tenants":{}}"#, 0o600)
            .unwrap();
        assert!(matches!(
            load(&fs, &path).unwrap_err(),
            StateError::Version { found: 99, .. }
        ));

        // Whitespace only is the first-boot case, not a corruption.
        fs.write_file(&path, b"\n\n", 0o600).unwrap();
        assert!(load(&fs, &path).unwrap().tenants.is_empty());
    }

    #[test]
    fn allocation_skips_claimed_and_occupied_ports() {
        let fs = MockFs::new();
        let mut state = StateFile::default();
        assert_eq!(allocate_port(&state, &fs, 9100).unwrap(), 9100);

        state
            .tenants
            .insert("alice".to_string(), record("alice", 9100));
        assert_eq!(allocate_port(&state, &fs, 9100).unwrap(), 9101);

        // Something unrelated on the box is on 9101.
        fs.occupy_port(9101);
        assert_eq!(allocate_port(&state, &fs, 9100).unwrap(), 9102);

        // A stopped tenant keeps its port: restarting it must land where its
        // Caddy route and its data dir already point.
        let mut stopped = record("bob", 9102);
        stopped.status = TenantStatus::Stopped;
        state.tenants.insert("bob".to_string(), stopped);
        assert_eq!(allocate_port(&state, &fs, 9100).unwrap(), 9103);
    }

    #[test]
    fn allocation_survives_a_reload() {
        let fs = MockFs::new();
        let path = test_config().state_file();
        let mut state = StateFile::default();
        for (i, label) in ["alice", "bob", "carol"].iter().enumerate() {
            let port = 9100 + i as u16;
            state
                .tenants
                .insert((*label).to_string(), record(label, port));
        }
        save(&fs, &path, &state).unwrap();

        // A restarted warden reads the file and keeps counting from where the
        // old one stopped, rather than from the base.
        let reloaded = load(&fs, &path).unwrap();
        assert_eq!(allocate_port(&reloaded, &fs, 9100).unwrap(), 9103);
    }

    #[test]
    fn exhaustion_is_an_error_rather_than_a_wrap() {
        let fs = MockFs::new();
        for port in 9100..9100 + PORT_RANGE {
            fs.occupy_port(port);
        }
        assert!(matches!(
            allocate_port(&StateFile::default(), &fs, 9100).unwrap_err(),
            StateError::PortsExhausted { .. }
        ));
    }
}
