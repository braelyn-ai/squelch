//! Provisioning: the ordered sequence of side effects that turns a label, a
//! mailbox address, and a blob of ciphertext into a running per-tenant daemon
//! behind its own subdomain.
//!
//! The order is the contract (`docs/HOSTED.md`, hosted MVP):
//!
//! 1. allocate a loopback port
//! 2. create `<tenants_dir>/<label>/` at 0700, owned by the tenant user
//! 3. write the credential ciphertext there at 0600, verbatim and unread
//! 4. render the systemd `EnvironmentFile`
//! 5. render the Caddy site file and reload Caddy
//! 6. `systemctl enable --now squelchd@<label>`
//! 7. exec `squelchd pair` as the tenant user and parse the code and deep link
//!
//! **Every failure unwinds.** A provision that stops halfway leaves no
//! half-enabled unit, no route to a port nothing listens on, and no record
//! claiming a port that was never used. The unwind is best effort by
//! definition — it is running because something on the box is already
//! misbehaving — and it is logged as counts rather than paths.
//!
//! PRIVACY, and these are not suggestions:
//!
//! - The credential ciphertext is never logged, never echoed, never inspected
//!   beyond the armor check in [`crate::validate::validate_ciphertext`].
//! - `squelchd pair` prints a LIVE pairing code on stdout. No command's output
//!   is logged at any level, which is the only rule that keeps that true.
//! - The tenant's mailbox address never reaches a log line or a response body.
//!   The label does: it is a public subdomain, it is in the unit name and in
//!   Caddy's access log already, and an operator with no identifier cannot
//!   debug a provisioning failure.

use std::fmt::Display;
use std::sync::{Arc, Mutex};

use crate::config::{Config, path_str};
use crate::host::{Cmd, CommandRunner, Fs};
use crate::state::{self, StateError, StateFile, TenantRecord, TenantStatus};
use crate::validate::{self, CiphertextError, EmailError, LabelError};

/// Mode of a tenant's data directory: the daemon's own, and nobody else's.
pub const TENANT_DIR_MODE: u32 = 0o700;

/// Mode of the credential file. The same 0600 discipline as
/// `squelch_core::credentials::write_private`, which is the only other writer
/// of a file with token material in it.
pub const CREDENTIAL_MODE: u32 = 0o600;

/// Mode of a tenant's env file. Read by systemd as root before it drops to the
/// tenant user, so nothing else needs it. It carries no secret today, and this
/// mode is what keeps that from being load-bearing.
pub const ENV_MODE: u32 = 0o600;

/// Mode of a Caddy site file. Caddy re-reads its config as its own user, so
/// unlike everything else here this one has to be world readable. It contains a
/// hostname and a loopback port.
pub const CADDY_MODE: u32 = 0o644;

/// What the control plane sends to create a tenant.
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub label: String,
    pub account_email: String,
    pub cred_read_ciphertext: String,
}

/// A pairing handoff, exactly as `squelchd pair` minted it.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub pair_code: String,
    pub pair_url: String,
    pub deep_link: String,
}

/// A provisioned tenant.
#[derive(Debug, Clone)]
pub struct Provisioned {
    pub port: u16,
    pub pairing: Pairing,
}

/// A tenant's current state, as `GET /v1/tenants/{label}` reports it.
#[derive(Debug, Clone)]
pub struct TenantView {
    /// `active` | `failed` | `stopped`.
    pub status: &'static str,
    pub port: u16,
}

/// Why an operation failed.
///
/// The `Host` variant carries a MACHINE REASON and nothing else: it goes on the
/// wire to the control plane, which logs it, and a path or an OS error string
/// there would be this box's internals in someone else's log aggregator. The
/// detail is logged here instead, where it belongs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WardenError {
    #[error("invalid label: {0}")]
    InvalidLabel(#[from] LabelError),
    #[error("invalid account_email: {0}")]
    InvalidEmail(#[from] EmailError),
    #[error("invalid cred_read_ciphertext: {0}")]
    InvalidCiphertext(#[from] CiphertextError),
    #[error("label already exists")]
    Conflict,
    #[error("no such tenant")]
    NotFound,
    #[error("{reason}")]
    Host { reason: &'static str },
}

impl WardenError {
    pub(crate) fn host(reason: &'static str) -> Self {
        Self::Host { reason }
    }
}

/// Log the detail on the box and return the machine reason for the wire.
fn fail(label: &str, reason: &'static str, detail: impl Display) -> WardenError {
    tracing::error!(tenant = label, reason, error = %detail, "warden step failed");
    WardenError::host(reason)
}

/// What a provision has created so far, so a failure can take it back down.
#[derive(Debug, Default, Clone, Copy)]
struct Created {
    record: bool,
    dir: bool,
    env: bool,
    caddy: bool,
    unit: bool,
}

/// The provisioner.
pub struct Warden {
    config: Arc<Config>,
    fs: Arc<dyn Fs>,
    cmd: Arc<dyn CommandRunner>,
    /// Serializes every state mutation. Two signups landing together would
    /// otherwise read the same state file, allocate the same port, and write
    /// each other's record away. A `std::sync::Mutex` and not a tokio one on
    /// purpose: every operation under it is blocking work (fork/exec, fsync)
    /// and runs on a blocking thread, so nothing awaits while holding it.
    lock: Mutex<()>,
}

impl Warden {
    pub fn new(config: Arc<Config>, fs: Arc<dyn Fs>, cmd: Arc<dyn CommandRunner>) -> Self {
        Self {
            config,
            fs,
            cmd,
            lock: Mutex::new(()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Create the state directory if it is not there. Called once at startup so
    /// a fresh box does not fail its first provision on a missing directory.
    pub fn ensure_state_dir(&self) -> Result<(), WardenError> {
        self.fs
            .create_dir(&self.config.state_dir, 0o700)
            .map_err(|e| fail("-", "state_dir_failed", e))
    }

    /// Poisoning is recovered rather than propagated: the guarded value is `()`
    /// and the invariant lives in the state file, so a panicked provision must
    /// not brick every future one.
    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn load_state(&self, label: &str) -> Result<StateFile, WardenError> {
        state::load(self.fs.as_ref(), &self.config.state_file())
            .map_err(|e| fail(label, "state_unreadable", e))
    }

    fn save_state(&self, label: &str, s: &StateFile) -> Result<(), WardenError> {
        state::save(self.fs.as_ref(), &self.config.state_file(), s)
            .map_err(|e| fail(label, "state_write_failed", e))
    }

    /// Run a command, treating a non-zero exit as a failure.
    ///
    /// The command line is logged; its OUTPUT never is. See the module header.
    fn run_checked(
        &self,
        label: &str,
        cmd: &Cmd,
        reason: &'static str,
    ) -> Result<crate::host::CmdOutput, WardenError> {
        let out = self.cmd.run(cmd).map_err(|e| fail(label, reason, e))?;
        if !out.ok() {
            tracing::error!(
                tenant = label,
                reason,
                command = %cmd.display(),
                code = ?out.code,
                "command exited non-zero"
            );
            return Err(WardenError::host(reason));
        }
        Ok(out)
    }

    /// `systemctl <args...>`.
    fn systemctl(&self, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::new(path_str(&self.config.systemctl_bin));
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        cmd
    }

    /// Create a tenant. See the module header for the ordering and the unwind
    /// guarantee.
    pub fn provision(&self, req: ProvisionRequest) -> Result<Provisioned, WardenError> {
        // Validation first, before the lock and before anything is touched: a
        // bad request must cost the box nothing.
        let label = validate::validate_label(&req.label)?;
        let account_email = validate::validate_account_email(&req.account_email)?;
        let ciphertext = validate::validate_ciphertext(&req.cred_read_ciphertext)?;

        let _guard = self.guard();

        let mut st = self.load_state(&label)?;
        if st.tenants.contains_key(&label) {
            return Err(WardenError::Conflict);
        }
        let port = state::allocate_port(&st, self.fs.as_ref(), self.config.port_base)
            .map_err(|e| match e {
                StateError::PortsExhausted { .. } => fail(&label, "ports_exhausted", e),
                other => fail(&label, "state_unreadable", other),
            })?;

        // The record goes in BEFORE the side effects, marked `provisioning`.
        // It reserves the port against a concurrent request that arrives while
        // this one is forking systemctl, and it is the only trace a warden that
        // is killed mid-provision leaves behind — without it, the orphaned
        // files would have nothing pointing at them.
        let record = TenantRecord {
            label: label.clone(),
            account_email: account_email.clone(),
            port,
            status: TenantStatus::Provisioning,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        st.tenants.insert(label.clone(), record.clone());
        self.save_state(&label, &st)?;

        let mut created = Created {
            record: true,
            ..Default::default()
        };
        match self.provision_steps(&label, &account_email, &ciphertext, port, &mut created) {
            Ok(pairing) => {
                let mut st = self.load_state(&label)?;
                if let Some(rec) = st.tenants.get_mut(&label) {
                    rec.status = TenantStatus::Active;
                }
                self.save_state(&label, &st)?;
                tracing::info!(tenant = %label, port, "tenant provisioned");
                Ok(Provisioned { port, pairing })
            }
            Err(e) => {
                self.unwind(&label, created);
                Err(e)
            }
        }
    }

    /// The side effects, in order. Split out so the caller can unwind whatever
    /// this managed to do before it failed.
    fn provision_steps(
        &self,
        label: &str,
        account_email: &str,
        ciphertext: &str,
        port: u16,
        created: &mut Created,
    ) -> Result<Pairing, WardenError> {
        let cfg = &self.config;

        let dir = cfg.tenant_dir(label);
        // Whether the directory was already there decides whether the unwind
        // may delete it. A directory this provision made is ours to remove; one
        // that was already on disk holds somebody's mail — a leaked dir from a
        // failed unwind, or a restore that ran ahead of the state file — and
        // deleting it on an unrelated failure would be the single most
        // destructive thing this service could do.
        let adopted = self.fs.exists(&dir);
        if adopted {
            tracing::warn!(
                tenant = label,
                "tenant directory already exists; adopting it and leaving it in place if this provision fails"
            );
        }
        self.fs
            .create_dir(&dir, TENANT_DIR_MODE)
            .map_err(|e| fail(label, "tenant_dir_failed", e))?;
        created.dir = !adopted;

        // Verbatim. The warden does not parse, re-serialize, or pretty-print
        // this: it is somebody else's ciphertext and the only correct thing to
        // do with it is put it where the daemon will look.
        self.fs
            .write_file(
                &cfg.credentials_path(label),
                ciphertext.as_bytes(),
                CREDENTIAL_MODE,
            )
            .map_err(|e| fail(label, "credential_write_failed", e))?;

        // The daemon runs as the tenant user and the warden runs as root, so
        // everything just written is root-owned. One recursive chown covers the
        // directory and the credential file; it must happen before the unit
        // starts, or the daemon's first write fails.
        self.run_checked(
            label,
            &Cmd::new(path_str(&cfg.chown_bin))
                .arg("-R")
                .arg(cfg.tenant_owner())
                .arg(path_str(&dir)),
            "chown_failed",
        )?;

        self.fs
            .write_file(
                &cfg.env_file(label),
                render_env_file(cfg, label, account_email, port).as_bytes(),
                ENV_MODE,
            )
            .map_err(|e| fail(label, "env_write_failed", e))?;
        created.env = true;

        self.fs
            .write_file(
                &cfg.caddy_file(label),
                render_caddy_site(cfg, label, port).as_bytes(),
                CADDY_MODE,
            )
            .map_err(|e| fail(label, "caddy_write_failed", e))?;
        created.caddy = true;

        self.run_checked(
            label,
            &self.systemctl(&["reload", &cfg.caddy_unit]),
            "caddy_reload_failed",
        )?;

        self.run_checked(
            label,
            &self.systemctl(&["enable", "--now", &cfg.unit_name(label)]),
            "unit_start_failed",
        )?;
        created.unit = true;

        self.mint_pairing(label, account_email)
    }

    /// Best-effort teardown of whatever a failed provision created.
    ///
    /// Errors here are counted, not propagated: the caller already has a
    /// failure to report, and a second one would replace a useful reason with a
    /// less useful one. PRIVACY: counts, not paths.
    fn unwind(&self, label: &str, created: Created) {
        let cfg = &self.config;
        let mut undone = 0usize;
        let mut failures = 0usize;
        let mut step = |ok: bool| {
            if ok {
                undone += 1;
            } else {
                failures += 1;
            }
        };

        if created.unit {
            // `disable --now` is stop + disable in one, which is exactly the
            // "no half-enabled unit" guarantee.
            step(
                self.cmd
                    .run(&self.systemctl(&["disable", "--now", &cfg.unit_name(label)]))
                    .is_ok_and(|o| o.ok()),
            );
        }
        if created.caddy {
            step(self.fs.remove_file(&cfg.caddy_file(label)).is_ok());
            // The route has to go before anything else can take this hostname,
            // so the reload is part of the unwind rather than a later cleanup.
            step(
                self.cmd
                    .run(&self.systemctl(&["reload", &cfg.caddy_unit]))
                    .is_ok_and(|o| o.ok()),
            );
        }
        if created.env {
            step(self.fs.remove_file(&cfg.env_file(label)).is_ok());
        }
        if created.dir {
            // Set only when THIS provision created the directory (see
            // `adopted` in provision_steps). An existing tenant's data dir is
            // never removed here, and never by DELETE either.
            step(self.fs.remove_dir_all(&cfg.tenant_dir(label)).is_ok());
        }
        if created.record {
            let removed = self
                .load_state(label)
                .and_then(|mut st| {
                    st.tenants.remove(label);
                    self.save_state(label, &st)
                })
                .is_ok();
            step(removed);
        }

        tracing::warn!(
            tenant = %label,
            steps_undone = undone,
            step_failures = failures,
            "provision failed; unwound"
        );
    }

    /// Mint a pairing code for an existing tenant by running `squelchd pair`
    /// against its store.
    fn mint_pairing(&self, label: &str, account_email: &str) -> Result<Pairing, WardenError> {
        let cfg = &self.config;
        let url = cfg.tenant_url(label);
        // Dropped to the tenant user with setpriv (the same tool the docker
        // entrypoints use) because this touches the tenant's SQLite file. Run
        // as root it would leave root-owned `-wal` and `-shm` files next to the
        // db, and the daemon — which is not root — would fail its next write
        // with a permissions error nobody would trace back to a pairing.
        let cmd = Cmd::new(path_str(&cfg.setpriv_bin))
            .arg(format!("--reuid={}", cfg.tenant_user))
            .arg(format!("--regid={}", cfg.tenant_user))
            .arg("--init-groups")
            .arg(path_str(&cfg.squelchd_bin))
            .arg("pair")
            .arg("--url")
            .arg(&url)
            // Exactly the three variables `squelchd pair` reads, and no more:
            // the store to mint in, the account to mint for, and a HOME that
            // keeps it from discovering some other user's config.toml. The
            // runner clears everything else.
            .env("HOME", path_str(&cfg.tenant_dir(label)))
            .env("SQUELCH_DB_PATH", path_str(&cfg.db_path(label)))
            .env("SQUELCH_ACCOUNT_EMAIL", account_email);

        let out = self.run_checked(label, &cmd, "pair_failed")?;
        // The output is a live credential from here to the end of this
        // function. It is parsed, moved into the response, and dropped.
        //
        // Both streams are scanned. As shipped, the daemon prints the code and
        // the link on stdout and its warnings on stderr; scanning both means a
        // future version that moves one of them is a warning in a diff rather
        // than a signup that dies at the last step.
        let output = format!("{}\n{}", out.stdout, out.stderr);
        parse_pair_output(&output, &url).ok_or_else(|| {
            tracing::error!(
                tenant = label,
                reason = "pair_output_unparsed",
                "squelchd pair succeeded but its output did not contain a code and a link"
            );
            WardenError::host("pair_output_unparsed")
        })
    }

    /// Re-mint a pairing code for a later device. Nothing else about the tenant
    /// is touched.
    pub fn repair(&self, raw_label: &str) -> Result<Pairing, WardenError> {
        let label = validate::validate_label(raw_label)?;
        let _guard = self.guard();
        let st = self.load_state(&label)?;
        let record = st.tenants.get(&label).ok_or(WardenError::NotFound)?;
        // Minting supersedes the previous code (the daemon keeps one live code
        // per account), which is the documented behaviour of `squelchd pair`.
        self.mint_pairing(&label, &record.account_email)
    }

    /// Report a tenant's status and port.
    pub fn status(&self, raw_label: &str) -> Result<TenantView, WardenError> {
        let label = validate::validate_label(raw_label)?;
        let _guard = self.guard();
        let st = self.load_state(&label)?;
        let record = st.tenants.get(&label).ok_or(WardenError::NotFound)?;

        // A record still marked `provisioning` means a warden died mid-flight;
        // whatever systemd says about a unit that may never have been enabled,
        // the honest answer is failed.
        if record.status == TenantStatus::Provisioning {
            return Ok(TenantView {
                status: "failed",
                port: record.port,
            });
        }

        // `systemctl is-active` exits non-zero for anything but active, so the
        // exit code is not the answer here — the word on stdout is.
        let status = match self
            .cmd
            .run(&self.systemctl(&["is-active", &self.config.unit_name(&label)]))
        {
            Ok(out) => map_is_active(out.stdout.trim()),
            Err(e) => {
                // systemd unreachable is an operator problem, not a verdict on
                // the tenant. Fall back to what the warden last recorded rather
                // than claiming a failure that may not exist.
                tracing::warn!(tenant = %label, error = %e, "could not ask systemd; reporting the recorded status");
                match record.status {
                    TenantStatus::Active => "active",
                    TenantStatus::Stopped => "stopped",
                    TenantStatus::Provisioning => "failed",
                }
            }
        };
        Ok(TenantView {
            status,
            port: record.port,
        })
    }

    /// Stop and disable a tenant, and take its route away. The DATA DIR STAYS:
    /// this is the "cancel my account" path, not the "destroy my mail" path,
    /// and the second one is a later flag with a lot more ceremony.
    ///
    /// Idempotent: an unknown label is success. The control plane calls this on
    /// its own unwind paths, where a retry after a partial failure must not
    /// turn into a 404 it has to special-case.
    pub fn deprovision(&self, raw_label: &str) -> Result<(), WardenError> {
        let label = validate::validate_label(raw_label)?;
        let _guard = self.guard();
        let mut st = self.load_state(&label)?;
        let Some(record) = st.tenants.get_mut(&label) else {
            tracing::info!(tenant = %label, "delete of an unknown tenant; nothing to do");
            return Ok(());
        };

        self.run_checked(
            &label,
            &self.systemctl(&["disable", "--now", &self.config.unit_name(&label)]),
            "unit_stop_failed",
        )?;

        // The route goes with the daemon. Leaving it would point the hostname
        // at a dead port (a 502 for anyone who kept the link) and would keep
        // renewing a certificate for a tenant that is not serving.
        self.fs
            .remove_file(&self.config.caddy_file(&label))
            .map_err(|e| fail(&label, "caddy_write_failed", e))?;
        self.run_checked(
            &label,
            &self.systemctl(&["reload", &self.config.caddy_unit]),
            "caddy_reload_failed",
        )?;

        // The env file stays, deliberately: it holds no secret, and it is what
        // a manual `systemctl start squelchd@<label>` needs to bring the tenant
        // back without a re-provision.
        record.status = TenantStatus::Stopped;
        self.save_state(&label, &st)?;
        tracing::info!(tenant = %label, "tenant stopped; data dir kept");
        Ok(())
    }
}

/// Map `systemctl is-active` output onto the three states the wire has.
fn map_is_active(word: &str) -> &'static str {
    match word {
        "active" | "activating" | "reloading" => "active",
        "failed" => "failed",
        // inactive, deactivating, unknown, and anything systemd invents later.
        _ => "stopped",
    }
}

/// The systemd `EnvironmentFile` for a tenant.
///
/// Every value here is either a path this crate built from validated
/// components or the account address, which
/// [`crate::validate::validate_account_email`] has already refused newlines,
/// quotes, backslashes and `$` in. That refusal is what makes this plain
/// `format!` safe: systemd would otherwise happily read an injected second
/// assignment.
pub fn render_env_file(config: &Config, label: &str, account_email: &str, port: u16) -> String {
    let dir = config.tenant_dir(label);
    format!(
        "# Managed by squelch-warden. Regenerated whenever this tenant is\n\
         # provisioned; hand edits are lost. One tenant, one file, one systemd\n\
         # instance of squelchd@.service.\n\
         SQUELCH_BIND=127.0.0.1:{port}\n\
         SQUELCH_ACCOUNT_EMAIL={account_email}\n\
         SQUELCH_DB_PATH={db}\n\
         SQUELCH_CRED_BACKEND=file\n\
         SQUELCH_CREDENTIALS_PATH={creds}\n\
         # The box's age identity. The DAEMON decrypts with it; neither the\n\
         # warden nor the control plane ever reads this file.\n\
         SQUELCH_CRED_AGE_IDENTITY={identity}\n\
         # Per-tenant HOME: keeps config.toml discovery and the embedding-model\n\
         # cache inside this tenant's own directory.\n\
         HOME={home}\n",
        db = path_str(&config.db_path(label)),
        creds = path_str(&config.credentials_path(label)),
        identity = path_str(&config.age_identity),
        home = path_str(&dir),
    )
}

/// The Caddy site file for a tenant.
///
/// The `/mcp*` refusal is the load-bearing line. Hosted ships the HUMAN door
/// only: the agent door is not internet-facing in this MVP, and a tenant site
/// that reverse-proxied everything would publish it. The main Caddyfile carries
/// the same refusal as a second copy, because this one is generated and the
/// generated thing is what a future edit could get wrong.
pub fn render_caddy_site(config: &Config, label: &str, port: u16) -> String {
    format!(
        "# Managed by squelch-warden: tenant `{label}`. Regenerated on provision\n\
         # and removed when the tenant is stopped. Hand edits are lost.\n\
         {host} {{\n\
         \tencode zstd gzip\n\
         \n\
         \t# Hosted is the human door only. The agent door (/mcp) is NOT routed\n\
         \t# to any tenant daemon; see deploy/hosted/Caddyfile for the global\n\
         \t# copy of this refusal.\n\
         \thandle /mcp* {{\n\
         \t\trespond 404\n\
         \t}}\n\
         \n\
         \thandle {{\n\
         \t\treverse_proxy 127.0.0.1:{port}\n\
         \t}}\n\
         }}\n",
        host = config.hostname(label),
    )
}

/// Pull the pairing code and the deep link out of `squelchd pair`'s stdout.
///
/// Held to the daemon's real output (`squelchd/src/bin/squelchd.rs::cmd_pair`):
/// a `Pairing code: XXXX-XXXX` line, then an indented `passband://pair?...`
/// line, both on stdout, with the warnings on stderr. Parsed rather than
/// re-derived because the code exists only in the daemon's store and only that
/// process ever sees the plaintext.
///
/// Returns `None` unless BOTH are found and the code has the shape the daemon
/// mints. A partial parse would hand the control plane a success it would show
/// a user, with half a handoff in it.
fn parse_pair_output(stdout: &str, url: &str) -> Option<Pairing> {
    let mut code = None;
    let mut deep_link = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Pairing code:") {
            let candidate = rest.trim();
            if validate::is_pairing_code(candidate) {
                code = Some(candidate.to_string());
            }
        } else if line.starts_with("passband://pair?") && line.contains("code=") {
            deep_link = Some(line.to_string());
        }
    }
    Some(Pairing {
        pair_code: code?,
        pair_url: url.to_string(),
        deep_link: deep_link?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Harness, armored, pair_stdout};

    fn request(label: &str) -> ProvisionRequest {
        ProvisionRequest {
            label: label.to_string(),
            account_email: format!("{label}@example.com"),
            cred_read_ciphertext: armored(label),
        }
    }

    /// The whole happy path, asserted as an exact command sequence and exact
    /// file contents. If provisioning grows a step, this test is the thing that
    /// notices.
    #[test]
    fn provisions_a_tenant_end_to_end() {
        let h = Harness::new();
        let out = h.warden.provision(request("alice")).unwrap();

        assert_eq!(out.port, 9100);
        assert_eq!(out.pairing.pair_code, "ABCD-1234");
        assert_eq!(out.pairing.pair_url, "https://alice.passband.email");
        assert!(out.pairing.deep_link.starts_with("passband://pair?url="));
        assert!(out.pairing.deep_link.contains("code=ABCD-1234"));

        assert_eq!(
            h.runner.calls(),
            vec![
                "/usr/bin/chown -R squelch:squelch /var/lib/squelch/tenants/alice",
                "/usr/bin/systemctl reload caddy",
                "/usr/bin/systemctl enable --now squelchd@alice.service",
                "/usr/bin/setpriv --reuid=squelch --regid=squelch --init-groups \
                 /usr/local/bin/squelchd pair --url https://alice.passband.email",
            ]
        );

        // The data dir is the daemon's alone.
        assert_eq!(
            h.fs.dir_mode(&h.config.tenant_dir("alice")),
            Some(TENANT_DIR_MODE)
        );

        // The ciphertext lands verbatim, at 0600.
        let (bytes, mode) = h.fs.file(&h.config.credentials_path("alice")).unwrap();
        assert_eq!(mode, CREDENTIAL_MODE);
        assert_eq!(String::from_utf8(bytes).unwrap(), armored("alice"));

        // The env file is asserted EXACTLY, not line by line. A missing
        // SQUELCH_CRED_AGE_IDENTITY here would mean a daemon that cannot
        // decrypt what the warden just wrote and writes its next credential in
        // plaintext; a stray line is a tenant configured by accident.
        let (env, env_mode) = h.fs.file(&h.config.env_file("alice")).unwrap();
        assert_eq!(env_mode, ENV_MODE);
        assert_eq!(
            String::from_utf8(env).unwrap(),
            concat!(
                "# Managed by squelch-warden. Regenerated whenever this tenant is\n",
                "# provisioned; hand edits are lost. One tenant, one file, one systemd\n",
                "# instance of squelchd@.service.\n",
                "SQUELCH_BIND=127.0.0.1:9100\n",
                "SQUELCH_ACCOUNT_EMAIL=alice@example.com\n",
                "SQUELCH_DB_PATH=/var/lib/squelch/tenants/alice/squelch.db\n",
                "SQUELCH_CRED_BACKEND=file\n",
                "SQUELCH_CREDENTIALS_PATH=/var/lib/squelch/tenants/alice/credentials.json\n",
                "# The box's age identity. The DAEMON decrypts with it; neither the\n",
                "# warden nor the control plane ever reads this file.\n",
                "SQUELCH_CRED_AGE_IDENTITY=/etc/squelch/age/identity.txt\n",
                "# Per-tenant HOME: keeps config.toml discovery and the embedding-model\n",
                "# cache inside this tenant's own directory.\n",
                "HOME=/var/lib/squelch/tenants/alice\n",
            )
        );

        let (caddy, caddy_mode) = h.fs.file(&h.config.caddy_file("alice")).unwrap();
        assert_eq!(caddy_mode, CADDY_MODE);
        let caddy = String::from_utf8(caddy).unwrap();
        assert!(caddy.contains("alice.passband.email {"), "{caddy}");
        assert!(caddy.contains("reverse_proxy 127.0.0.1:9100"), "{caddy}");
        assert!(caddy.contains("handle /mcp* {"), "{caddy}");
        assert!(caddy.contains("respond 404"), "{caddy}");

        // And the tenant is recorded as active on the port it got.
        let view = h.warden.status("alice").unwrap();
        assert_eq!(view.port, 9100);
    }

    #[test]
    fn the_pair_exec_gets_only_the_three_variables_it_needs() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        let pair = h
            .runner
            .raw_calls()
            .into_iter()
            .find(|c| c.args.iter().any(|a| a == "pair"))
            .expect("the pair exec ran");
        let mut keys: Vec<_> = pair.env.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["HOME", "SQUELCH_ACCOUNT_EMAIL", "SQUELCH_DB_PATH"]
        );
    }

    #[test]
    fn a_duplicate_label_is_a_conflict_and_changes_nothing() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        let before = h.runner.calls().len();

        assert_eq!(
            h.warden.provision(request("alice")).unwrap_err(),
            WardenError::Conflict
        );
        // A conflict must not have run a single command: no reload, no restart
        // of a healthy tenant.
        assert_eq!(h.runner.calls().len(), before);
        // ...and the live tenant's credential file is untouched.
        let (bytes, _) = h.fs.file(&h.config.credentials_path("alice")).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), armored("alice"));
    }

    #[test]
    fn case_folding_makes_alice_and_alice_the_same_tenant() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        let mut upper = request("alice");
        upper.label = "ALICE".to_string();
        assert_eq!(h.warden.provision(upper).unwrap_err(), WardenError::Conflict);
    }

    #[test]
    fn invalid_input_never_reaches_the_box() {
        let h = Harness::new();

        let mut bad_label = request("alice");
        bad_label.label = "-nope-".to_string();
        assert!(matches!(
            h.warden.provision(bad_label).unwrap_err(),
            WardenError::InvalidLabel(_)
        ));

        let mut reserved = request("alice");
        reserved.label = "mcp".to_string();
        assert_eq!(
            h.warden.provision(reserved).unwrap_err(),
            WardenError::InvalidLabel(LabelError::Reserved)
        );

        let mut bad_email = request("alice");
        bad_email.account_email = "alice@example.com\nSQUELCH_API_TOKEN=hunter2".to_string();
        assert!(matches!(
            h.warden.provision(bad_email).unwrap_err(),
            WardenError::InvalidEmail(_)
        ));

        // The one that matters most: a plaintext token must not land on disk.
        let mut plaintext = request("alice");
        plaintext.cred_read_ciphertext = r#"{"refresh_token":"1//0gREAL"}"#.to_string();
        assert!(matches!(
            h.warden.provision(plaintext).unwrap_err(),
            WardenError::InvalidCiphertext(_)
        ));

        assert!(h.runner.calls().is_empty());
        assert!(h.fs.paths().is_empty());
    }

    #[test]
    fn a_failure_mid_sequence_unwinds_everything_it_made() {
        // The unit starts, then the pair exec fails: the worst case, because it
        // is the one that leaves a daemon running on a live mailbox.
        let h = Harness::new();
        h.runner.fail_on("squelchd pair");

        let err = h.warden.provision(request("alice")).unwrap_err();
        assert_eq!(err, WardenError::host("pair_failed"));

        let calls = h.runner.calls();
        assert!(
            calls.contains(&"/usr/bin/systemctl disable --now squelchd@alice.service".to_string()),
            "{calls:?}"
        );
        // Nothing is left on disk...
        assert!(h.fs.paths().iter().all(|p| p.ends_with("state.json")), "{:?}", h.fs.paths());
        assert!(h.fs.file(&h.config.env_file("alice")).is_none());
        assert!(h.fs.file(&h.config.caddy_file("alice")).is_none());
        assert!(h.fs.file(&h.config.credentials_path("alice")).is_none());
        // ...and the label is free again, on the same port.
        assert_eq!(
            h.warden.status("alice").unwrap_err(),
            WardenError::NotFound
        );
        h.runner.clear_failures();
        h.runner.script_pair("EFGH-5678", "https://alice.passband.email");
        assert_eq!(h.warden.provision(request("alice")).unwrap().port, 9100);
    }

    #[test]
    fn a_failed_credential_write_stops_before_systemd() {
        let h = Harness::new();
        h.fs.fail_writes_containing("credentials.json");

        let err = h.warden.provision(request("alice")).unwrap_err();
        assert_eq!(err, WardenError::host("credential_write_failed"));
        // Nothing was enabled, so there is nothing to disable: the unwind only
        // removes the directory it made.
        assert!(h.runner.calls().iter().all(|c| !c.contains("systemctl")), "{:?}", h.runner.calls());
        assert!(h.fs.file(&h.config.env_file("alice")).is_none());
    }

    /// The most destructive thing this service could do, and the guard that
    /// stops it: a data directory that was already on disk survives an unwind.
    #[test]
    fn an_unwind_never_deletes_a_data_dir_it_did_not_create() {
        let h = Harness::new();
        // A leaked directory: mail on disk, but no record in the state file.
        h.fs.create_dir(&h.config.tenant_dir("alice"), TENANT_DIR_MODE)
            .unwrap();
        h.fs.write_file(
            &h.config.tenant_dir("alice").join("squelch.db"),
            b"someone's mailbox",
            0o600,
        )
        .unwrap();
        h.runner.fail_on("systemctl enable");

        assert_eq!(
            h.warden.provision(request("alice")).unwrap_err(),
            WardenError::host("unit_start_failed")
        );
        assert_eq!(
            h.fs.text(&h.config.tenant_dir("alice").join("squelch.db"))
                .as_deref(),
            Some("someone's mailbox")
        );
        // Everything the provision itself made is still gone.
        assert!(h.fs.file(&h.config.env_file("alice")).is_none());
        assert!(h.fs.file(&h.config.caddy_file("alice")).is_none());
    }

    #[test]
    fn a_caddy_reload_failure_takes_the_site_file_back_out() {
        let h = Harness::new();
        h.runner.fail_on("systemctl reload caddy");

        assert_eq!(
            h.warden.provision(request("alice")).unwrap_err(),
            WardenError::host("caddy_reload_failed")
        );
        assert!(h.fs.file(&h.config.caddy_file("alice")).is_none());
        // The unit was never enabled, so it was never disabled either.
        let calls = h.runner.calls();
        assert!(!calls.iter().any(|c| c.contains("enable")), "{calls:?}");
    }

    #[test]
    fn ports_climb_and_survive_a_restart() {
        let h = Harness::new();
        assert_eq!(h.warden.provision(request("alice")).unwrap().port, 9100);
        assert_eq!(h.warden.provision(request("bob")).unwrap().port, 9101);

        // A brand new Warden over the same filesystem: this is a restart.
        let restarted = Warden::new(h.config.clone(), h.fs.clone(), h.runner.clone());
        assert_eq!(restarted.provision(request("carol")).unwrap().port, 9102);
    }

    #[test]
    fn allocation_skips_a_port_something_else_holds() {
        let h = Harness::new();
        h.fs.occupy_port(9100);
        h.fs.occupy_port(9101);
        assert_eq!(h.warden.provision(request("alice")).unwrap().port, 9102);
    }

    #[test]
    fn repair_mints_a_second_code_without_touching_anything_else() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        let after_provision = h.runner.calls().len();

        h.runner
            .script("squelchd pair", crate::host::CmdOutput::success(pair_stdout(
                "WXYZ-9876",
                "https://alice.passband.email",
            )));
        let pairing = h.warden.repair("ALICE").unwrap();
        assert_eq!(pairing.pair_code, "WXYZ-9876");
        assert_eq!(pairing.pair_url, "https://alice.passband.email");
        assert!(pairing.deep_link.contains("code=WXYZ-9876"));

        // Exactly one new command: the pair exec. No restart, no reload.
        assert_eq!(h.runner.calls().len(), after_provision + 1);
        assert!(h.runner.calls().last().unwrap().contains("squelchd pair"));
    }

    #[test]
    fn repair_of_an_unknown_tenant_is_a_404() {
        let h = Harness::new();
        assert_eq!(h.warden.repair("nobody").unwrap_err(), WardenError::NotFound);
        assert!(h.runner.calls().is_empty());
    }

    #[test]
    fn delete_stops_and_disables_but_keeps_the_data() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        let before = h.runner.calls().len();

        h.warden.deprovision("alice").unwrap();

        let new_calls = &h.runner.calls()[before..];
        assert_eq!(
            new_calls,
            [
                "/usr/bin/systemctl disable --now squelchd@alice.service",
                "/usr/bin/systemctl reload caddy",
            ]
        );
        // The mail and the credential stay put.
        assert!(h.fs.file(&h.config.credentials_path("alice")).is_some());
        assert_eq!(
            h.fs.dir_mode(&h.config.tenant_dir("alice")),
            Some(TENANT_DIR_MODE)
        );
        // The env file stays so a manual restart works; the route does not.
        assert!(h.fs.file(&h.config.env_file("alice")).is_some());
        assert!(h.fs.file(&h.config.caddy_file("alice")).is_none());

        // The label stays taken and the port stays reserved.
        assert_eq!(
            h.warden.provision(request("alice")).unwrap_err(),
            WardenError::Conflict
        );
        h.runner.script("is-active", crate::host::CmdOutput::success("inactive"));
        assert_eq!(h.warden.status("alice").unwrap().status, "stopped");
    }

    #[test]
    fn delete_of_an_unknown_tenant_is_a_no_op() {
        let h = Harness::new();
        h.warden.deprovision("nobody").unwrap();
        assert!(h.runner.calls().is_empty());
    }

    #[test]
    fn status_reads_systemd_not_the_record() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();

        h.runner
            .script("is-active", crate::host::CmdOutput::success("failed\n"));
        assert_eq!(h.warden.status("alice").unwrap().status, "failed");
        assert_eq!(h.warden.status("alice").unwrap().port, 9100);
    }

    #[test]
    fn status_falls_back_to_the_record_when_systemd_cannot_be_reached() {
        let h = Harness::new();
        h.warden.provision(request("alice")).unwrap();
        h.runner.unspawnable("is-active");
        assert_eq!(h.warden.status("alice").unwrap().status, "active");
    }

    #[test]
    fn maps_every_systemd_word() {
        assert_eq!(map_is_active("active"), "active");
        assert_eq!(map_is_active("activating"), "active");
        assert_eq!(map_is_active("reloading"), "active");
        assert_eq!(map_is_active("failed"), "failed");
        assert_eq!(map_is_active("inactive"), "stopped");
        assert_eq!(map_is_active("deactivating"), "stopped");
        assert_eq!(map_is_active("unknown"), "stopped");
        assert_eq!(map_is_active(""), "stopped");
    }

    #[test]
    fn parses_the_daemons_real_pair_output() {
        let url = "https://alice.passband.email";
        let out = pair_stdout("ABCD-1234", url);
        let parsed = parse_pair_output(&out, url).unwrap();
        assert_eq!(parsed.pair_code, "ABCD-1234");
        assert_eq!(parsed.pair_url, url);
        assert!(parsed.deep_link.starts_with("passband://pair?url="));
        assert!(parsed.deep_link.contains("code=ABCD-1234"));
    }

    /// The daemon prints both lines on stdout today. If a future version moved
    /// the link to stderr, provisioning must still work.
    #[test]
    fn a_link_on_stderr_still_parses() {
        let h = Harness::new();
        h.runner.script(
            "squelchd pair",
            crate::host::CmdOutput {
                code: Some(0),
                stdout: "Pairing code: JKMN-6789\n".to_string(),
                stderr: "  passband://pair?url=https%3A%2F%2Falice.passband.email&code=JKMN-6789\n"
                    .to_string(),
            },
        );
        let out = h.warden.provision(request("alice")).unwrap();
        assert_eq!(out.pairing.pair_code, "JKMN-6789");
        assert!(out.pairing.deep_link.contains("code=JKMN-6789"));
    }

    #[test]
    fn refuses_a_half_parse() {
        let url = "https://alice.passband.email";
        // A code with no link, and a link with no code, are both nothing.
        assert!(parse_pair_output("Pairing code: ABCD-1234\n", url).is_none());
        assert!(parse_pair_output("  passband://pair?url=x&code=ABCD-1234\n", url).is_none());
        assert!(parse_pair_output("", url).is_none());
        // A code that is not the shape the daemon mints is not a code.
        assert!(
            parse_pair_output(
                "Pairing code: nonsense\n  passband://pair?url=x&code=nonsense\n",
                url
            )
            .is_none()
        );
    }
}
