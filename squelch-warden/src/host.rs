//! Every side effect this service has, behind two traits.
//!
//! The warden's whole job is effects: it makes directories, writes files, runs
//! `systemctl`, and execs the daemon. That is also exactly what a test cannot
//! be allowed to do, so nothing in this crate calls `std::fs` or
//! `std::process` outside this file. [`Fs`] and [`CommandRunner`] are the seam;
//! the real implementations are here, the mocks are in [`crate::testing`], and
//! the provisioning logic above never learns which one it has.
//!
//! Two rules the rest of the crate depends on:
//!
//! - **Commands run with a cleared environment.** The warden's own environment
//!   holds `SQUELCH_WARDEN_TOKEN`, and the daemon reads a dozen `SQUELCH_*`
//!   vars. A child that inherited either would be a token in another process's
//!   environment and a tenant configured by whatever the warden's unit happened
//!   to set. Every child gets a fixed PATH plus exactly the variables the
//!   caller named.
//! - **Command output is never logged.** `squelchd pair` prints a live pairing
//!   code on stdout. There is no log level at which that is acceptable, so the
//!   rule is blanket rather than per-call: logs carry the program, the
//!   arguments, and the exit status, and nothing else.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The PATH every child gets. Fixed rather than inherited so a unit with a
/// surprising PATH cannot change which `systemctl` runs; the binaries
/// themselves are configured as absolute paths anyway, and this is the belt to
/// that suspenders.
const CHILD_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A host effect that failed. These carry paths and OS error text, so they are
/// for the box's own logs: the wire never sees one (see
/// [`crate::provision::WardenError`]).
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
}

impl HostError {
    fn io(op: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A command to run: program, arguments, and the exact environment it gets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Extra variables for the child, on top of `PATH`. NEVER included in
    /// [`Cmd::display`] — for the pair exec this names the tenant's mailbox.
    pub env: Vec<(String, String)>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Program and arguments, space-joined. This is what logs print and what
    /// the tests assert a provision's command sequence against, which is why it
    /// is deliberately not `Debug`: the environment must not be in it.
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// What a command did. `stdout`/`stderr` are captured because one caller parses
/// them; nothing logs them.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    /// `None` when the child died on a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    /// A successful exit, which for every command the warden runs means 0.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// A canned success, for tests and for the `is-active` fallback.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
}

/// Runs commands. The real one shells out; the mock records.
pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, HostError>;
}

/// Touches the filesystem, plus the one non-file probe that has to be mockable
/// for the same reason the files do: whether a port is free is a fact about the
/// box, and a test must be able to lie about it.
pub trait Fs: Send + Sync {
    /// Create `path` (and its parents) with `mode` on the leaf. Already
    /// existing is success — provisioning is retried by humans.
    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), HostError>;

    /// Write `bytes` to `path` at `mode`, atomically: a temp file in the same
    /// directory, then a rename. Nothing here may leave a half-written
    /// credential or a truncated env file behind a crash.
    fn write_file(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), HostError>;

    /// Read a file, `None` when it does not exist.
    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError>;

    /// Remove a file; a missing one is success (unwind paths call this).
    fn remove_file(&self, path: &Path) -> Result<(), HostError>;

    /// Remove a directory and everything under it; missing is success.
    fn remove_dir_all(&self, path: &Path) -> Result<(), HostError>;

    fn exists(&self, path: &Path) -> bool;

    /// Whether nothing is listening on `127.0.0.1:port` right now.
    fn port_free(&self, port: u16) -> bool;
}

/// The real filesystem.
pub struct RealFs;

impl RealFs {
    /// The sibling temp path a write lands on first. Same directory, so the
    /// rename is atomic (a rename across filesystems is not).
    fn temp_path(path: &Path) -> PathBuf {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "warden".to_string());
        path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
    }
}

impl Fs for RealFs {
    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), HostError> {
        std::fs::create_dir_all(path).map_err(|e| HostError::io("creating dir", path, e))?;
        set_mode(path, mode)
    }

    fn write_file(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), HostError> {
        let tmp = Self::temp_path(path);
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Created at the final mode, not created 0644 and fixed after:
                // between those two syscalls a credential file is world
                // readable.
                opts.mode(mode);
            }
            let mut f = opts
                .open(&tmp)
                .map_err(|e| HostError::io("opening temp file", &tmp, e))?;
            f.write_all(bytes)
                .map_err(|e| HostError::io("writing temp file", &tmp, e))?;
            f.flush()
                .map_err(|e| HostError::io("flushing temp file", &tmp, e))?;
            // Durability before the rename: a power cut between them would
            // otherwise leave a correctly named file full of zeroes.
            f.sync_all()
                .map_err(|e| HostError::io("syncing temp file", &tmp, e))?;
        }
        // The destination may have pre-existed at a looser mode; the temp
        // file's mode does NOT carry over an existing inode's on some
        // filesystems, so it is set explicitly on both sides of the rename.
        set_mode(&tmp, mode)?;
        std::fs::rename(&tmp, path).map_err(|e| {
            // Best effort: a leftover temp file is noise, not a failure mode.
            let _ = std::fs::remove_file(&tmp);
            HostError::io("renaming into place", path, e)
        })?;
        set_mode(path, mode)
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HostError::io("reading", path, e)),
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), HostError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HostError::io("removing", path, e)),
        }
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), HostError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HostError::io("removing dir", path, e)),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn port_free(&self, port: u16) -> bool {
        // A plain bind, no SO_REUSEADDR: the question is exactly "would a
        // daemon starting now fail to bind this", and that is what a plain
        // bind answers.
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }
}

/// Force `mode` on unix; a no-op elsewhere (the warden only ever runs on
/// Linux, but the crate must still build on the Mac it is developed on).
fn set_mode(path: &Path, mode: u32) -> Result<(), HostError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| HostError::io("setting mode", path, e))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// The real command runner.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, HostError> {
        let mut child = std::process::Command::new(&cmd.program);
        child.args(&cmd.args);
        // See the module header: nothing is inherited, ever.
        child.env_clear();
        child.env("PATH", CHILD_PATH);
        for (k, v) in &cmd.env {
            child.env(k, v);
        }
        // No stdin: `squelchd pair` does not read one, and a child that blocked
        // on a terminal the warden does not have would hang provisioning.
        child.stdin(Stdio::null());
        let out = child.output().map_err(|e| HostError::Spawn {
            program: cmd.program.clone(),
            source: e,
        })?;
        Ok(CmdOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real Fs against a real temp dir. Everything else in the crate runs
    /// on the mock, so this is the one place the modes and the atomic rename
    /// are checked against an actual filesystem.
    #[test]
    #[cfg(unix)]
    fn real_fs_writes_at_the_mode_it_was_given() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("warden-fs-test-{}", std::process::id()));
        let fs = RealFs;
        let dir = base.join("tenant");
        fs.create_dir(&dir, 0o700).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);

        let file = dir.join("credentials.json");
        fs.write_file(&file, b"armored", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs.read_file(&file).unwrap().unwrap(), b"armored");

        // Overwriting an existing file keeps the mode rather than inheriting
        // whatever it had.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        fs.write_file(&file, b"again", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        assert!(fs.exists(&file));
        fs.remove_file(&file).unwrap();
        assert!(!fs.exists(&file));
        // Missing is success, on both removers.
        fs.remove_file(&file).unwrap();
        fs.remove_dir_all(&base).unwrap();
        fs.remove_dir_all(&base).unwrap();
        assert_eq!(fs.read_file(&base.join("gone")).unwrap(), None);
    }

    #[test]
    fn cmd_display_is_program_and_args_only() {
        let cmd = Cmd::new("/usr/bin/systemctl")
            .arg("enable")
            .arg("--now")
            .arg("squelchd@alice.service")
            .env("SQUELCH_ACCOUNT_EMAIL", "someone@example.com");
        assert_eq!(
            cmd.display(),
            "/usr/bin/systemctl enable --now squelchd@alice.service"
        );
        // The env is the point: it carries a mailbox address and must not be in
        // anything a log line can reach.
        assert!(!cmd.display().contains("example.com"));
    }

    #[test]
    fn real_runner_clears_the_environment() {
        // `env` with no arguments prints the child's environment. If the
        // clear were missing, this test's own marker would show up in it.
        unsafe { std::env::set_var("WARDEN_TEST_MARKER", "leaked") };
        let out = RealCommandRunner
            .run(&Cmd::new("/usr/bin/env").env("KEPT", "yes"))
            .expect("env(1) exists on both Linux and macOS");
        unsafe { std::env::remove_var("WARDEN_TEST_MARKER") };
        assert!(out.ok(), "env exited {:?}", out.code);
        assert!(!out.stdout.contains("WARDEN_TEST_MARKER"));
        assert!(out.stdout.contains("KEPT=yes"));
        assert!(out.stdout.contains("PATH="));
    }

    #[test]
    fn real_runner_reports_a_missing_program_rather_than_pretending() {
        let err = RealCommandRunner
            .run(&Cmd::new("/nonexistent/warden-test-binary"))
            .unwrap_err();
        assert!(matches!(err, HostError::Spawn { .. }));
    }
}
