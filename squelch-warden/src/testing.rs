//! Test doubles for the two host traits, plus the fixtures every test module
//! builds on.
//!
//! Compiled only under `cfg(test)`: a provisioner that shipped a fake
//! filesystem in its release binary would be one refactor away from using it.
//! Unit tests must not require systemd, a Caddy, a squelchd, or root, so every
//! test in this crate runs against these.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Config, DEFAULT_BIND, DEFAULT_PORT_BASE};
use crate::host::{Cmd, CmdOutput, CommandRunner, Fs, HostError};
use crate::provision::Warden;

/// The bearer every test presents.
pub const TEST_TOKEN: &str = "warden-test-token-0123456789abcdef";

/// A config with the production defaults and no environment read.
pub fn test_config() -> Config {
    Config {
        bind: DEFAULT_BIND.parse().unwrap(),
        token: TEST_TOKEN.to_string(),
        base_domain: "passband.email".to_string(),
        state_dir: PathBuf::from("/var/lib/squelch/warden"),
        tenants_dir: PathBuf::from("/var/lib/squelch/tenants"),
        env_dir: PathBuf::from("/etc/squelch/tenants"),
        caddy_dir: PathBuf::from("/etc/caddy/tenants"),
        squelchd_bin: PathBuf::from("/usr/local/bin/squelchd"),
        setpriv_bin: PathBuf::from("/usr/bin/setpriv"),
        systemctl_bin: PathBuf::from("/usr/bin/systemctl"),
        chown_bin: PathBuf::from("/usr/bin/chown"),
        age_identity: PathBuf::from("/etc/squelch/age/identity.txt"),
        tenant_user: "squelch".to_string(),
        caddy_unit: "caddy".to_string(),
        port_base: DEFAULT_PORT_BASE,
    }
}

/// An age-armored body of the shape the control plane sends. Not real
/// ciphertext — the warden never decrypts, so nothing in these tests needs it
/// to be.
pub fn armored(marker: &str) -> String {
    format!(
        "-----BEGIN AGE ENCRYPTED FILE-----\nYWdlLWVuY3J5cHRpb24ub3JnL3Yx{marker}\n-----END AGE ENCRYPTED FILE-----\n"
    )
}

/// Exactly what `squelchd pair --url ...` prints on stdout today (see
/// `squelchd/src/bin/squelchd.rs::cmd_pair`). The parser is held to this, so
/// when the daemon's output changes this string is the thing to change first
/// and watch fail.
pub fn pair_stdout(code: &str, url: &str) -> String {
    format!(
        "Pairing code: {code}\n\
         Valid for 10 minutes, until 2026-08-07T12:00:00Z.\n\
         \n\
         On the device you are pairing, open this link:\n\
         \n\
         \x20 passband://pair?url={}&code={code}\n\
         \n\
         Or type the code into Passband, pointed at {url}\n",
        url.replace(':', "%3A").replace('/', "%2F"),
    )
}

#[derive(Default)]
struct MockFsInner {
    files: BTreeMap<PathBuf, (Vec<u8>, u32)>,
    dirs: BTreeMap<PathBuf, u32>,
    busy_ports: BTreeSet<u16>,
    /// Any write whose path contains one of these substrings fails.
    fail_writes: Vec<String>,
}

/// An in-memory filesystem that remembers modes.
#[derive(Default)]
pub struct MockFs {
    inner: Mutex<MockFsInner>,
}

impl MockFs {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockFsInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make `port` look occupied by something outside the warden's bookkeeping.
    pub fn occupy_port(&self, port: u16) {
        self.lock().busy_ports.insert(port);
    }

    /// Make every write to a path containing `needle` fail.
    pub fn fail_writes_containing(&self, needle: &str) {
        self.lock().fail_writes.push(needle.to_string());
    }

    /// The bytes and mode at `path`, if anything is there.
    pub fn file(&self, path: &Path) -> Option<(Vec<u8>, u32)> {
        self.lock().files.get(path).cloned()
    }

    /// The file at `path` as a string.
    pub fn text(&self, path: &Path) -> Option<String> {
        self.file(path)
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The mode a directory was created with.
    pub fn dir_mode(&self, path: &Path) -> Option<u32> {
        self.lock().dirs.get(path).copied()
    }

    /// Every path that currently holds bytes, for "nothing was left behind"
    /// assertions.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.lock().files.keys().cloned().collect()
    }
}

impl Fs for MockFs {
    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), HostError> {
        self.lock().dirs.insert(path.to_path_buf(), mode);
        Ok(())
    }

    fn write_file(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), HostError> {
        let mut inner = self.lock();
        let display = path.to_string_lossy().into_owned();
        if inner.fail_writes.iter().any(|n| display.contains(n)) {
            return Err(HostError::Io {
                op: "writing",
                path: path.to_path_buf(),
                source: std::io::Error::other("mock write failure"),
            });
        }
        inner
            .files
            .insert(path.to_path_buf(), (bytes.to_vec(), mode));
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self.lock().files.get(path).map(|(b, _)| b.clone()))
    }

    fn remove_file(&self, path: &Path) -> Result<(), HostError> {
        self.lock().files.remove(path);
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), HostError> {
        let mut inner = self.lock();
        inner.dirs.retain(|p, _| !p.starts_with(path));
        inner.files.retain(|p, _| !p.starts_with(path));
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        let inner = self.lock();
        inner.files.contains_key(path) || inner.dirs.contains_key(path)
    }

    fn port_free(&self, port: u16) -> bool {
        !self.lock().busy_ports.contains(&port)
    }
}

#[derive(Default)]
struct MockRunnerInner {
    calls: Vec<Cmd>,
    /// First matching substring wins, so a test can script the pair exec
    /// without naming its whole command line.
    scripted: Vec<(String, CmdOutput)>,
    /// Commands containing one of these exit non-zero.
    fail: Vec<String>,
    /// Commands containing one of these fail to spawn at all.
    unspawnable: Vec<String>,
}

/// A command runner that records instead of running.
#[derive(Default)]
pub struct MockRunner {
    inner: Mutex<MockRunnerInner>,
}

impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockRunnerInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Answer any command containing `needle` with `out`.
    pub fn script(&self, needle: &str, out: CmdOutput) {
        self.lock().scripted.push((needle.to_string(), out));
    }

    /// Script the pair exec with the daemon's real output shape.
    pub fn script_pair(&self, code: &str, url: &str) {
        self.script("squelchd pair", CmdOutput::success(pair_stdout(code, url)));
    }

    /// Make any command containing `needle` exit 1.
    pub fn fail_on(&self, needle: &str) {
        self.lock().fail.push(needle.to_string());
    }

    /// Stop failing anything: for a test that breaks the box, watches the
    /// unwind, and then wants to prove the tenant can be provisioned again.
    pub fn clear_failures(&self) {
        let mut inner = self.lock();
        inner.fail.clear();
        inner.unspawnable.clear();
    }

    /// Make any command containing `needle` fail to spawn.
    pub fn unspawnable(&self, needle: &str) {
        self.lock().unspawnable.push(needle.to_string());
    }

    /// Every command run so far, as `program arg arg`.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.iter().map(Cmd::display).collect()
    }

    /// The full recorded commands, for a test that needs to see the child
    /// environment the provisioner built.
    pub fn raw_calls(&self) -> Vec<Cmd> {
        self.lock().calls.clone()
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, HostError> {
        let mut inner = self.lock();
        let display = cmd.display();
        inner.calls.push(cmd.clone());
        if inner.unspawnable.iter().any(|n| display.contains(n)) {
            return Err(HostError::Spawn {
                program: cmd.program.clone(),
                source: std::io::Error::other("mock spawn failure"),
            });
        }
        if inner.fail.iter().any(|n| display.contains(n)) {
            return Ok(CmdOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: "mock failure".to_string(),
            });
        }
        // Newest script wins, so a test can re-script a command it already
        // scripted (a second pairing, say) without clearing the first.
        if let Some((_, out)) = inner.scripted.iter().rev().find(|(n, _)| display.contains(n)) {
            return Ok(out.clone());
        }
        Ok(CmdOutput::success(""))
    }
}

/// A warden wired to mocks, with the mocks kept for assertions.
pub struct Harness {
    pub warden: Arc<Warden>,
    pub fs: Arc<MockFs>,
    pub runner: Arc<MockRunner>,
    pub config: Arc<Config>,
}

impl Harness {
    /// A harness whose pair exec answers with `code`.
    pub fn new() -> Self {
        Self::with_config(test_config())
    }

    pub fn with_config(config: Config) -> Self {
        let fs = Arc::new(MockFs::new());
        let runner = Arc::new(MockRunner::new());
        runner.script_pair("ABCD-1234", "https://alice.passband.email");
        // A box where everything provisioned is running, which is what a test
        // that is not about systemd wants to assume.
        runner.script("is-active", CmdOutput::success("active\n"));
        let config = Arc::new(config);
        let warden = Arc::new(Warden::new(
            config.clone(),
            fs.clone() as Arc<dyn Fs>,
            runner.clone() as Arc<dyn CommandRunner>,
        ));
        Self {
            warden,
            fs,
            runner,
            config,
        }
    }

    /// Router state over this harness's warden, for the wire tests.
    pub fn state(&self) -> crate::WardenState {
        crate::WardenState::new(self.warden.clone())
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
