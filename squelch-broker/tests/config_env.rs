//! Startup-config tests, in their OWN test binary because they mutate
//! process-global environment: `set_var` is unsound while another thread reads
//! the environment, and a separate integration-test file is a separate process.

use squelch_broker::{Config, MAX_SESSIONS_PER_CLIENT};

/// The ONLY test that touches process-global environment: it sets, reads, and
/// clears in one body so nothing else can observe a half-built config.
#[test]
fn config_reads_the_environment() {
    const VARS: &[&str] = &[
        "SQUELCH_BROKER_BIND",
        "SQUELCH_BROKER_PUBLIC_URL",
        "SQUELCH_BROKER_TRUSTED_PROXY_HOPS",
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
    // The public URL has no safe default: guessing it would mean guessing the
    // `redirect_uri` every daemon has to match.
    assert!(Config::from_env().is_err());

    set("SQUELCH_BROKER_PUBLIC_URL", "https://auth.passband.email/");
    let c = Config::from_env().unwrap();
    assert_eq!(c.bind.to_string(), squelch_broker::config::DEFAULT_BIND);
    assert!(c.bind.ip().is_loopback(), "the default must not be public");
    // Stored canonical: no trailing slash, and the callback derived from it.
    assert_eq!(c.public_url, "https://auth.passband.email");
    assert_eq!(c.callback_url, "https://auth.passband.email/callback");
    assert!(!c.is_insecure());

    for bad in [
        "auth.passband.email",
        "ftp://auth.passband.email",
        "https://auth.passband.email/broker",
        "https://user:pw@auth.passband.email",
    ] {
        set("SQUELCH_BROKER_PUBLIC_URL", bad);
        assert!(Config::from_env().is_err(), "{bad} should be refused");
    }
    set("SQUELCH_BROKER_PUBLIC_URL", "http://127.0.0.1:8851");
    assert!(Config::from_env().unwrap().is_insecure());

    set("SQUELCH_BROKER_BIND", "127.0.0.1:9999");
    assert_eq!(
        Config::from_env().unwrap().bind.to_string(),
        "127.0.0.1:9999"
    );
    set("SQUELCH_BROKER_BIND", "not-an-address");
    assert!(Config::from_env().is_err());
    set("SQUELCH_BROKER_BIND", "127.0.0.1:9999");

    // Trusting a forwarded header is the operator's assertion about their own
    // topology, so the default trusts nothing and the per-client session cap
    // stays off until they make it.
    let c = Config::from_env().unwrap();
    assert_eq!(c.trusted_proxy_hops, 0);
    assert_eq!(c.max_sessions_per_client(), None);

    set("SQUELCH_BROKER_TRUSTED_PROXY_HOPS", "1");
    let c = Config::from_env().unwrap();
    assert_eq!(c.trusted_proxy_hops, 1);
    assert_eq!(c.max_sessions_per_client(), Some(MAX_SESSIONS_PER_CLIENT));

    // A typo here would degrade silently to one bucket for the whole internet,
    // so it is a refusal to boot instead.
    for bad in ["-1", "one", "9", "1.5", "999999"] {
        set("SQUELCH_BROKER_TRUSTED_PROXY_HOPS", bad);
        assert!(Config::from_env().is_err(), "{bad} should be refused");
    }

    clear();
}
