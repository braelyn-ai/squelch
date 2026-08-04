//! Startup-config tests, in their OWN test binary because they mutate
//! process-global environment: `set_var` is unsound while another thread reads
//! the environment, and a separate integration-test file is a separate process.

use squelch_relay::{Config, Environment, RelayState};

mod common;
use common::{BETA_TOPIC, KEY, TOPIC};

/// The ONLY test that touches process-global environment: it sets, reads, and
/// clears in one body so nothing else can observe a half-built config.
#[test]
fn config_reads_the_environment() {
    const VARS: &[&str] = &[
        "SQUELCH_RELAY_BIND",
        "SQUELCH_RELAY_APNS_KEY",
        "SQUELCH_RELAY_APNS_KEY_PATH",
        "SQUELCH_RELAY_APNS_KEY_ID",
        "SQUELCH_RELAY_APNS_TEAM_ID",
        "SQUELCH_RELAY_APNS_TOPICS",
        "SQUELCH_RELAY_APNS_ENV",
        "SQUELCH_RELAY_AUTH_TOKEN",
        "SQUELCH_RELAY_ALLOW_ANONYMOUS",
        "SQUELCH_RELAY_APNS_URL_OVERRIDE",
        "SQUELCH_RELAY_DB_PATH",
    ];
    fn clear() {
        for v in VARS {
            unsafe { std::env::remove_var(v) };
        }
    }
    fn set(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }

    clear();
    // No key source at all.
    assert!(Config::from_env().is_err());

    set("SQUELCH_RELAY_APNS_KEY", KEY);
    assert!(Config::from_env().is_err(), "key id still missing");
    set("SQUELCH_RELAY_APNS_KEY_ID", "KEYID12345");
    assert!(Config::from_env().is_err(), "team id still missing");
    set("SQUELCH_RELAY_APNS_TEAM_ID", "TEAMID6789");
    assert!(Config::from_env().is_err(), "topics still missing");
    set(
        "SQUELCH_RELAY_APNS_TOPICS",
        " dev.squelch.ios , dev.squelch.ios.beta ",
    );

    // A forgotten bearer must be a refusal to boot, not a silently open relay.
    assert!(
        Config::from_env().is_err(),
        "auth token still missing and anonymous not opted into"
    );
    set("SQUELCH_RELAY_AUTH_TOKEN", "short");
    assert!(
        Config::from_env().is_err(),
        "a token too short to be worth a constant-time compare"
    );
    // A secret pasted across two lines: long enough, but it can never ride in
    // an HTTP header, so booting would 401 every client forever with nothing in
    // the logs. The ends are trimmed, so only an INTERIOR break survives to here.
    set(
        "SQUELCH_RELAY_AUTH_TOKEN",
        "0123456789abcdef0123456789abcdef0123456789\nabcdef0123456789abcdef",
    );
    assert!(
        Config::from_env().is_err(),
        "a token with an interior newline is unpresentable, not merely ugly"
    );
    // Surrounding whitespace is still fine: var() trims the ends.
    set(
        "SQUELCH_RELAY_AUTH_TOKEN",
        "  0123456789abcdef0123456789abcdef0123456789ab  ",
    );
    assert!(
        Config::from_env().is_ok(),
        "trimmable padding must not be confused with an interior break"
    );
    unsafe { std::env::remove_var("SQUELCH_RELAY_AUTH_TOKEN") };
    set("SQUELCH_RELAY_ALLOW_ANONYMOUS", "nope");
    assert!(Config::from_env().is_err(), "unparseable opt-out");
    set("SQUELCH_RELAY_ALLOW_ANONYMOUS", "1");

    let cfg = Config::from_env().unwrap();
    assert_eq!(cfg.bind.to_string(), squelch_relay::config::DEFAULT_BIND);
    assert_eq!(cfg.apns_topics, vec![TOPIC, BETA_TOPIC]);
    assert_eq!(cfg.resolve_topic(None), Some(TOPIC));
    assert_eq!(cfg.apns_env, Environment::Production);
    assert!(cfg.auth_token.is_none());
    assert!(cfg.apns_url_override.is_none());
    // Unset means the open buffer lives in memory.
    assert!(cfg.db_path.is_none());
    // The key parses into a usable signer.
    RelayState::new(cfg).unwrap();

    // Two key sources is ambiguous, not a preference order.
    set("SQUELCH_RELAY_APNS_KEY_PATH", "tests/fixture_test_key.p8");
    assert!(Config::from_env().is_err());
    unsafe { std::env::remove_var("SQUELCH_RELAY_APNS_KEY") };
    let cfg = Config::from_env().unwrap();
    assert!(cfg.apns_key_pem.contains("BEGIN PRIVATE KEY"));

    // A PEM flattened onto one line with literal `\n` sequences is unmangled,
    // and still signs.
    unsafe { std::env::remove_var("SQUELCH_RELAY_APNS_KEY_PATH") };
    set(
        "SQUELCH_RELAY_APNS_KEY",
        &KEY.trim_end().replace('\n', "\\n"),
    );
    let cfg = Config::from_env().unwrap();
    assert!(
        cfg.apns_key_pem.contains('\n'),
        "sequences became real newlines"
    );
    assert!(!cfg.apns_key_pem.contains('\\'), "no backslash survives");
    RelayState::new(cfg).unwrap();
    unsafe { std::env::remove_var("SQUELCH_RELAY_APNS_KEY") };
    set("SQUELCH_RELAY_APNS_KEY_PATH", "tests/fixture_test_key.p8");

    const BEARER: &str = "0123456789abcdef0123456789abcdef";
    set("SQUELCH_RELAY_APNS_ENV", "sandbox");
    set("SQUELCH_RELAY_AUTH_TOKEN", BEARER);
    set("SQUELCH_RELAY_BIND", "127.0.0.1:9999");
    // A bearer AND the anonymous opt-out is a contradiction, not a preference.
    assert!(Config::from_env().is_err());
    unsafe { std::env::remove_var("SQUELCH_RELAY_ALLOW_ANONYMOUS") };
    let cfg = Config::from_env().unwrap();
    assert_eq!(cfg.apns_env, Environment::Sandbox);
    assert_eq!(cfg.apns_base_url(), "https://api.sandbox.push.apple.com");
    assert_eq!(cfg.auth_token.as_deref(), Some(BEARER));
    assert_eq!(cfg.bind.to_string(), "127.0.0.1:9999");
    // The secrets never reach a log through `{:?}`.
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains(BEARER));
    assert!(!dbg.contains("PRIVATE KEY"));

    set("SQUELCH_RELAY_APNS_ENV", "staging");
    assert!(Config::from_env().is_err());
    set("SQUELCH_RELAY_APNS_ENV", "sandbox");

    set("SQUELCH_RELAY_BIND", "not-an-address");
    assert!(Config::from_env().is_err());
    set("SQUELCH_RELAY_BIND", "127.0.0.1:9999");

    set("SQUELCH_RELAY_APNS_URL_OVERRIDE", "ftp://nope");
    assert!(Config::from_env().is_err());
    set("SQUELCH_RELAY_APNS_URL_OVERRIDE", "http://127.0.0.1:1234/");
    let cfg = Config::from_env().unwrap();
    // Trailing slash is trimmed so the joined path is never doubled.
    assert_eq!(cfg.apns_base_url(), "http://127.0.0.1:1234");

    set("SQUELCH_RELAY_APNS_KEY_PATH", "tests/does-not-exist.p8");
    assert!(Config::from_env().is_err());
    set("SQUELCH_RELAY_APNS_KEY_PATH", "tests/fixture_test_key.p8");

    // The open buffer opens (and creates) the configured file.
    let db = std::env::temp_dir().join(format!(
        "squelch-relay-config-{}.sqlite3",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);
    set("SQUELCH_RELAY_DB_PATH", db.to_str().unwrap());
    let cfg = Config::from_env().unwrap();
    assert_eq!(cfg.db_path.as_deref(), Some(db.as_path()));
    RelayState::new(cfg).unwrap();
    assert!(db.exists(), "the buffer file is created at startup");
    let _ = std::fs::remove_file(&db);

    clear();
}
