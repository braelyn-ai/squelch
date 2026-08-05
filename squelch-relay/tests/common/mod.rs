//! Shared scaffolding for the relay integration tests: the APNs signing fixture,
//! the configured topics, and the `Config` both binaries build on.
//!
//! Each test binary compiles its own copy of this module and uses only part of
//! it, so unused helpers here are expected rather than rot.
#![allow(dead_code)]

use squelch_relay::{Config, Environment};

/// The PKCS#8 fixture every test signs provider tokens with. Test-only material:
/// it is not, and must never be, a real APNs key.
pub const KEY: &str = include_str!("../fixture_test_key.p8");
/// The default topic — first in the configured list.
pub const TOPIC: &str = "dev.squelch.ios";
/// A second configured topic, so "not the default" is testable.
pub const BETA_TOPIC: &str = "dev.squelch.ios.beta";

/// A relay config on an ephemeral port with both topics allowed. `auth_token`
/// left `None` is the anonymous relay; `apns_url_override` points APNs at a
/// mock instead of Apple.
pub fn config(apns_url_override: Option<String>, auth_token: Option<String>) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        apns_key_pem: KEY.to_string(),
        apns_key_id: "KEYID12345".into(),
        apns_team_id: "TEAMID6789".into(),
        apns_topics: vec![TOPIC.into(), BETA_TOPIC.into()],
        apns_env: Environment::Production,
        auth_token,
        apns_url_override,
        // In-memory open buffer: it lives as long as the `RelayState` the test
        // holds, and never leaves a file behind.
        db_path: None,
        // Trust nothing by default, as production does; the rate-limit tests
        // raise it on their own copy.
        trusted_proxy_hops: 0,
        trusted_proxy_cidrs: None,
    }
}

/// Serve `config` on an ephemeral port and return its base URL. Connect info is
/// attached because the rate limiters key on the peer address.
pub async fn spawn_relay(config: Config) -> String {
    let app = squelch_relay::router(squelch_relay::RelayState::new(config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}
