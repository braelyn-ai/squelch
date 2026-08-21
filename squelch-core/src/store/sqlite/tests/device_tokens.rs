//! Device-token and pairing-code store tests.
//!
//! The assertions that matter here are negative ones: that a revoked token is
//! indistinguishable from an unknown one, that every pairing failure is the same
//! failure, and that no plaintext ever reaches a listing or the audit log.

use super::super::device_tokens::{
    CONSOLE_DEVICE_NAME, PAIRING_CODE_LEN, PAIRING_TTL_SECS, TOKEN_PREFIX,
};
use super::super::*;
use super::support::*;
use chrono::{Duration, TimeZone};

fn ttl() -> Duration {
    Duration::seconds(PAIRING_TTL_SECS)
}

/// Read a token row's stored hash and `last_used_at` straight off the table —
/// the two columns no public method will hand back.
fn raw_token_row(store: &SqliteStore, id: i64) -> (String, Option<String>) {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT token_hash, last_used_at FROM device_tokens WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Backdate a token's `last_used_at` so the throttle's stale branch is reachable
/// without sleeping through it.
fn backdate_last_used(store: &SqliteStore, id: i64, ago: Duration) {
    let conn = store.lock().unwrap();
    conn.execute(
        "UPDATE device_tokens SET last_used_at = ?2 WHERE id = ?1",
        params![id, (Utc::now() - ago).to_rfc3339()],
    )
    .unwrap();
}

fn live_code_count(store: &SqliteStore, acct: AccountId) -> i64 {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM pairing_codes WHERE account_id = ?1 AND claimed_at IS NULL",
        params![acct],
        |r| r.get(0),
    )
    .unwrap()
}

fn failed_attempts(store: &SqliteStore, acct: AccountId) -> Option<i64> {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT failed_attempts FROM pairing_codes WHERE account_id = ?1",
        params![acct],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

// ---- device tokens -----------------------------------------------------

#[test]
fn issued_token_round_trips_through_verify() {
    let (store, acct) = store();
    let issued = store.issue_device_token(acct, "Braelyn's iPhone").unwrap();

    assert!(issued.token.starts_with(TOKEN_PREFIX));
    // 32 bytes is exactly 43 unpadded base64url chars; nothing truncated, no '='.
    assert_eq!(issued.token.len(), TOKEN_PREFIX.len() + 43);
    assert!(
        issued.token[TOKEN_PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "the token must survive an Authorization header unescaped"
    );

    let verified = store
        .verify_device_token(&issued.token)
        .unwrap()
        .expect("a freshly issued token authenticates");
    assert_eq!(verified.id, issued.id);
    assert_eq!(verified.account_id, acct);
    assert_eq!(verified.name, "Braelyn's iPhone");
    assert!(verified.revoked_at.is_none());

    // The PLAINTEXT is nowhere on disk; the stored hash is not the token.
    let (hash, _) = raw_token_row(&store, issued.id);
    assert_eq!(hash.len(), 64, "lowercase hex sha256");
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    );
    assert_ne!(hash, issued.token);
}

#[test]
fn minted_tokens_never_repeat() {
    let (store, acct) = store();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let issued = store.issue_device_token(acct, "device").unwrap();
        assert!(seen.insert(issued.token), "minted the same token twice");
    }
}

#[test]
fn an_unknown_or_malformed_token_is_a_plain_no() {
    let (store, acct) = store();
    let issued = store.issue_device_token(acct, "phone").unwrap();

    assert!(
        store
            .verify_device_token("sqd_not-a-real-token")
            .unwrap()
            .is_none()
    );
    // No prefix at all: rejected before it ever reaches the store.
    assert!(store.verify_device_token("bare-secret").unwrap().is_none());
    assert!(store.verify_device_token("").unwrap().is_none());
    // The stored HASH is not itself a credential.
    let (hash, _) = raw_token_row(&store, issued.id);
    assert!(store.verify_device_token(&hash).unwrap().is_none());
    // A one-character mutation is as unknown as anything else: near misses buy
    // nothing, which is the whole point of matching on a digest.
    let flip = if issued.token.ends_with('A') {
        'B'
    } else {
        'A'
    };
    let mutated = format!("{}{flip}", &issued.token[..issued.token.len() - 1]);
    assert!(store.verify_device_token(&mutated).unwrap().is_none());
}

#[test]
fn surrounding_whitespace_on_a_presented_token_is_ignored() {
    let (store, acct) = store();
    let issued = store.issue_device_token(acct, "phone").unwrap();
    assert!(
        store
            .verify_device_token(&format!("  {}\n", issued.token))
            .unwrap()
            .is_some()
    );
}

#[test]
fn revoking_kills_verify_on_the_very_next_call() {
    let (store, acct) = store();
    let issued = store.issue_device_token(acct, "old laptop").unwrap();
    assert!(store.verify_device_token(&issued.token).unwrap().is_some());

    assert!(store.revoke_device_token(acct, issued.id).unwrap());
    // No cache anywhere in the verify path: the next call already refuses.
    assert!(
        store.verify_device_token(&issued.token).unwrap().is_none(),
        "a revoked token is as absent as an unknown one"
    );

    // Revoking twice is not an error, and does not re-audit.
    assert!(!store.revoke_device_token(acct, issued.id).unwrap());
    // Nor is revoking something that never existed.
    assert!(!store.revoke_device_token(acct, 999_999).unwrap());

    // The tombstone stays visible to the operator.
    let listed = store.list_device_tokens(acct).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked_at.is_some());
}

#[test]
fn a_revoke_cannot_reach_another_accounts_token() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    let issued = store.issue_device_token(acct, "phone").unwrap();

    assert!(
        !store.revoke_device_token(other, issued.id).unwrap(),
        "the id is real, but it is not this account's"
    );
    assert!(store.verify_device_token(&issued.token).unwrap().is_some());
    assert!(store.list_device_tokens(other).unwrap().is_empty());
}

#[test]
fn listing_is_account_scoped_oldest_first_and_carries_nothing_secret() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    let a = store.issue_device_token(acct, "  iPhone  ").unwrap();
    let b = store.issue_device_token(acct, "iPad").unwrap();
    store.issue_device_token(other, "not mine").unwrap();

    let listed = store.list_device_tokens(acct).unwrap();
    assert_eq!(
        listed.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![a.id, b.id]
    );
    // The name is trimmed on the way in.
    assert_eq!(listed[0].name, "iPhone");
    assert!(listed[0].last_used_at.is_none(), "never used yet");

    // Nothing in the serialized listing resembles the credential.
    let json = serde_json::to_string(&listed).unwrap();
    assert!(!json.contains(&a.token));
    assert!(!json.contains(&b.token));
    assert!(!json.contains(&raw_token_row(&store, a.id).0));
}

#[test]
fn an_empty_device_name_is_refused_loudly_at_the_cli_and_a_long_one_is_capped() {
    let (store, acct) = store();
    assert!(matches!(
        store.issue_device_token(acct, "   "),
        Err(CoreError::InvalidInput(_))
    ));
    let long = "n".repeat(500);
    let issued = store.issue_device_token(acct, &long).unwrap();
    assert_eq!(issued.name.chars().count(), 100);
}

#[test]
fn last_used_at_is_touched_once_then_throttled() {
    let (store, acct) = store();
    let issued = store.issue_device_token(acct, "phone").unwrap();
    assert_eq!(raw_token_row(&store, issued.id).1, None);

    // First use stamps it.
    store.verify_device_token(&issued.token).unwrap().unwrap();
    let first = raw_token_row(&store, issued.id)
        .1
        .expect("stamped on first use");

    // A burst of further requests inside the window writes nothing — this is
    // what keeps an SSE-driven client from turning reads into a write storm.
    for _ in 0..5 {
        store.verify_device_token(&issued.token).unwrap().unwrap();
    }
    assert_eq!(
        raw_token_row(&store, issued.id).1.as_deref(),
        Some(first.as_str())
    );

    // Past the throttle window it refreshes again.
    backdate_last_used(&store, issued.id, Duration::seconds(120));
    let stale = raw_token_row(&store, issued.id).1.unwrap();
    let seen = store.verify_device_token(&issued.token).unwrap().unwrap();
    let refreshed = raw_token_row(&store, issued.id).1.unwrap();
    assert_ne!(refreshed, stale, "a stale stamp is refreshed");
    assert!(seen.last_used_at.is_some());
}

// ---- activation --------------------------------------------------------

/// Rewrite a token's `created_at`, so a pairing order can be built without
/// waiting for the clock. The column is the one the activation reader folds a
/// minimum over.
fn backdate_created(store: &SqliteStore, id: i64, at: DateTime<Utc>) {
    let conn = store.lock().unwrap();
    conn.execute(
        "UPDATE device_tokens SET created_at = ?2 WHERE id = ?1",
        params![id, at.to_rfc3339()],
    )
    .unwrap();
}

#[test]
fn a_mailbox_nobody_paired_with_has_no_activation_stamp() {
    let (store, acct) = store();
    assert_eq!(store.first_client_pairing_at(acct).unwrap(), None);
    assert_eq!(store.count_client_devices(acct).unwrap(), 0);
}

/// The console signs in by claiming a pairing code under a RESERVED name, which
/// on a hosted tenant is part of the signup flow itself. Counting it would
/// report every signup as an activation.
#[test]
fn a_console_session_is_not_a_paired_device() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    store
        .claim_pairing_code(&minted.code, CONSOLE_DEVICE_NAME)
        .unwrap();

    assert_eq!(store.first_client_pairing_at(acct).unwrap(), None);
    assert_eq!(store.count_client_devices(acct).unwrap(), 0);
    // The row is really there; it is the READERS that skip it.
    assert_eq!(store.list_device_tokens(acct).unwrap().len(), 1);

    // One real device, and the answer appears.
    store.issue_device_token(acct, "iPhone").unwrap();
    assert!(store.first_client_pairing_at(acct).unwrap().is_some());
    assert_eq!(store.count_client_devices(acct).unwrap(), 1);
    assert_eq!(store.list_device_tokens(acct).unwrap().len(), 2);
}

/// The EARLIEST pairing wins whatever order the rows went in, because the answer
/// is folded over parsed timestamps rather than taken off the id order or off a
/// string comparison of the column.
#[test]
fn the_earliest_pairing_wins_regardless_of_insert_order() {
    let (store, acct) = store();
    let march = Utc
        .with_ymd_and_hms(2026, 3, 1, 9, 30, 0)
        .single()
        .unwrap();
    let april = Utc
        .with_ymd_and_hms(2026, 4, 1, 9, 30, 0)
        .single()
        .unwrap();

    // Inserted newest-first, so an implementation that trusted id order would
    // answer April.
    let later = store.issue_device_token(acct, "iPad").unwrap();
    backdate_created(&store, later.id, april);
    let earlier = store.issue_device_token(acct, "iPhone").unwrap();
    backdate_created(&store, earlier.id, march);
    // And a console row in between, older than either, which must not win.
    let console = store.issue_device_token(acct, CONSOLE_DEVICE_NAME).unwrap();
    backdate_created(
        &store,
        console.id,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap(),
    );

    assert_eq!(store.first_client_pairing_at(acct).unwrap(), Some(march));
    assert_eq!(store.count_client_devices(acct).unwrap(), 2);

    // Another account's devices are nobody else's activation.
    let other = store.ensure_account("other@example.com").unwrap();
    assert_eq!(store.first_client_pairing_at(other).unwrap(), None);
    assert_eq!(store.count_client_devices(other).unwrap(), 0);
}

/// REVOKING A DEVICE DOES NOT UN-HAPPEN THE PAIRING. The stamp is a fact about
/// the past; the gauge is a statement about right now, and this is the one case
/// where the two readers deliberately disagree.
#[test]
fn a_revoked_first_device_still_counts_as_the_activation() {
    let (store, acct) = store();
    let march = Utc
        .with_ymd_and_hms(2026, 3, 1, 9, 30, 0)
        .single()
        .unwrap();
    let phone = store.issue_device_token(acct, "iPhone").unwrap();
    backdate_created(&store, phone.id, march);
    assert!(store.revoke_device_token(acct, phone.id).unwrap());

    assert_eq!(store.first_client_pairing_at(acct).unwrap(), Some(march));
    assert_eq!(
        store.count_client_devices(acct).unwrap(),
        0,
        "nothing can connect right now, which is a different question"
    );
}

// ---- pairing -----------------------------------------------------------

#[test]
fn a_minted_code_is_eight_unambiguous_symbols_shown_dashed() {
    let (store, acct) = store();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        assert_eq!(minted.code.len(), PAIRING_CODE_LEN + 1, "XXXX-XXXX");
        assert_eq!(&minted.code[4..5], "-");
        let bare = minted.code.replace('-', "");
        assert!(
            bare.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "Crockford base32 is uppercase alphanumeric: {bare}"
        );
        assert!(
            !bare.contains(['I', 'L', 'O', 'U']),
            "the ambiguous glyphs are excluded by construction: {bare}"
        );
        assert!(minted.expires_at > Utc::now());
        assert!(seen.insert(bare), "minted the same code twice");
    }
}

#[test]
fn claiming_a_code_yields_a_working_token() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();

    let issued = store
        .claim_pairing_code(&minted.code, "  Braelyn's iPhone  ")
        .unwrap();
    assert_eq!(issued.account_id, acct, "the code resolved its own account");
    assert_eq!(issued.name, "Braelyn's iPhone");

    // The token the claim handed back is a real human-door credential.
    let verified = store.verify_device_token(&issued.token).unwrap().unwrap();
    assert_eq!(verified.id, issued.id);
    assert_eq!(store.list_device_tokens(acct).unwrap().len(), 1);
}

#[test]
fn a_code_is_accepted_however_the_user_retypes_it() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    let bare = minted.code.replace('-', "");

    // Lowercased, undashed, and padded with the whitespace a paste drags along.
    let typed = format!("  {}  ", bare.to_lowercase());
    assert!(store.claim_pairing_code(&typed, "iPhone").is_ok());
}

#[test]
fn a_claim_is_one_shot() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    store.claim_pairing_code(&minted.code, "iPhone").unwrap();

    // The replay is refused with the module's single, detail-free answer.
    assert!(matches!(
        store.claim_pairing_code(&minted.code, "iPhone again"),
        Err(CoreError::NotFound)
    ));
    assert_eq!(
        store.list_device_tokens(acct).unwrap().len(),
        1,
        "a burned code mints nothing"
    );
}

#[test]
fn an_expired_code_is_refused_exactly_like_a_wrong_one() {
    let (store, acct) = store();
    let minted = store
        .mint_pairing_code(acct, Duration::seconds(-1))
        .unwrap();

    assert!(matches!(
        store.claim_pairing_code(&minted.code, "iPhone"),
        Err(CoreError::NotFound)
    ));
    assert!(matches!(
        store.claim_pairing_code("ZZZZ-ZZZZ", "iPhone"),
        Err(CoreError::NotFound)
    ));
    // An expired code is not "live", so a miss does not charge it an attempt.
    assert_eq!(failed_attempts(&store, acct), Some(0));
    assert!(store.list_device_tokens(acct).unwrap().is_empty());
}

#[test]
fn a_nameless_claim_is_refused_without_spending_the_code() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();

    assert!(matches!(
        store.claim_pairing_code(&minted.code, "   "),
        Err(CoreError::NotFound)
    ));
    assert_eq!(
        failed_attempts(&store, acct),
        Some(0),
        "the caller's own empty field must not burn their code"
    );
    // The code still works once a name is supplied.
    assert!(store.claim_pairing_code(&minted.code, "iPhone").is_ok());
}

#[test]
fn five_misses_burn_the_live_code() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();

    for expected in 1..5 {
        assert!(store.claim_pairing_code("2345-6789", "attacker").is_err());
        assert_eq!(failed_attempts(&store, acct), Some(expected));
    }
    // The fifth miss takes the row with it.
    assert!(store.claim_pairing_code("2345-6789", "attacker").is_err());
    assert_eq!(failed_attempts(&store, acct), None, "the code is gone");

    // The RIGHT code now fails too, and identically.
    assert!(matches!(
        store.claim_pairing_code(&minted.code, "iPhone"),
        Err(CoreError::NotFound)
    ));
    assert!(store.list_device_tokens(acct).unwrap().is_empty());

    let burns = store
        .list_audit(acct, 50)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "pairing.burn")
        .count();
    assert_eq!(burns, 1, "the burn is recorded exactly once");
}

#[test]
fn a_garbage_code_still_costs_an_attempt() {
    let (store, acct) = store();
    store.mint_pairing_code(acct, ttl()).unwrap();
    // Wrong length, wrong alphabet, empty: brute force is never free.
    assert!(store.claim_pairing_code("!!!", "attacker").is_err());
    assert!(store.claim_pairing_code("", "attacker").is_err());
    assert_eq!(failed_attempts(&store, acct), Some(2));
}

#[test]
fn minting_supersedes_the_previous_code() {
    let (store, acct) = store();
    let first = store.mint_pairing_code(acct, ttl()).unwrap();
    let second = store.mint_pairing_code(acct, ttl()).unwrap();
    assert_eq!(
        live_code_count(&store, acct),
        1,
        "one live code per account, always"
    );

    // The superseded code is dead, and spends an attempt against the new one.
    assert!(store.claim_pairing_code(&first.code, "iPhone").is_err());
    assert_eq!(failed_attempts(&store, acct), Some(1));
    assert!(store.claim_pairing_code(&second.code, "iPhone").is_ok());
}

#[test]
fn a_code_only_ever_pairs_its_own_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    let mine = store.mint_pairing_code(acct, ttl()).unwrap();
    store.mint_pairing_code(other, ttl()).unwrap();

    let issued = store.claim_pairing_code(&mine.code, "iPhone").unwrap();
    assert_eq!(issued.account_id, acct);
    assert_eq!(store.list_device_tokens(other).unwrap().len(), 0);
}

// ---- audit + secrecy ---------------------------------------------------

#[test]
fn the_audit_trail_records_every_step_and_no_secret_material() {
    let (store, acct) = store();
    // Real mail sitting on the low message ids, so the enrichment join in
    // `list_audit` has something to WRONGLY attach to a credential row if the
    // targets ever go back to bare decimals.
    for i in 1..=4 {
        triaged(acct, &format!("g{i}"), &format!("t{i}")).seed(&store);
    }
    let issued = store.issue_device_token(acct, "laptop").unwrap();
    store.revoke_device_token(acct, issued.id).unwrap();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    let paired = store.claim_pairing_code(&minted.code, "iPhone").unwrap();

    let log = store.list_audit(acct, 50).unwrap();
    let actions: Vec<&str> = log.iter().map(|e| e.action.as_str()).collect();
    for expected in [
        "token.issue",
        "token.revoke",
        "pairing.mint",
        "pairing.claim",
    ] {
        assert!(
            actions.contains(&expected),
            "missing audit action {expected}"
        );
    }

    // The claim's audit row names the token it minted, so a reader can tie the
    // device back to the pairing that created it. NAMESPACED, never a bare id:
    // `list_audit` joins messages on `CAST(target AS INTEGER)`, so a decimal here
    // would dress this row up as somebody's email.
    let claim = log.iter().find(|e| e.action == "pairing.claim").unwrap();
    assert_eq!(
        claim.target.as_deref(),
        Some(format!("token:{}", paired.id).as_str())
    );
    assert_eq!(claim.detail.as_deref(), Some("iPhone"));
    for entry in log.iter().filter(|e| {
        matches!(
            e.action.as_str(),
            "token.issue" | "token.revoke" | "pairing.mint" | "pairing.claim" | "pairing.burn"
        )
    }) {
        let target = entry.target.as_deref().unwrap_or_default();
        assert!(
            target.starts_with("token:") || target.starts_with("code:"),
            "{} targets a bare id: {target}",
            entry.action
        );
        assert!(
            entry.target_sender.is_none() && entry.target_subject.is_none(),
            "{} borrowed a message's identity in the audit view",
            entry.action
        );
    }

    // Nothing anywhere in the log is, or hashes to, a live credential.
    let bare_code = minted.code.replace('-', "");
    let hashes = [
        raw_token_row(&store, issued.id).0,
        raw_token_row(&store, paired.id).0,
    ];
    for entry in &log {
        let row = format!("{:?}{:?}", entry.target, entry.detail);
        assert!(!row.contains(&issued.token));
        assert!(!row.contains(&paired.token));
        assert!(!row.contains(&minted.code));
        assert!(!row.contains(&bare_code));
        for hash in &hashes {
            assert!(
                !row.contains(hash),
                "a hash in the audit log is a crack target"
            );
        }
    }
}

#[test]
fn debug_formatting_redacts_the_plaintext() {
    let (store, acct) = store();
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    let issued = store.claim_pairing_code(&minted.code, "iPhone").unwrap();

    // `{:?}` in a log line is the classic accidental leak; both carriers refuse.
    let printed = format!("{issued:?} {minted:?}");
    assert!(!printed.contains(&issued.token));
    assert!(!printed.contains(&minted.code));
    assert!(printed.contains("<redacted>"));
    // The non-secret fields still print, or the redaction is useless for debugging.
    assert!(printed.contains("iPhone"));
}

// ---- two processes on one file -----------------------------------------

/// `squelchd token issue` against a live `squelchd serve` is TWO PROCESSES on
/// one DB file, which is what the CLI subcommands introduced. Two handles here
/// stand in for them: the "daemon" holds a read transaction open the way a
/// serving connection does, the "CLI" mints through its own handle, and the
/// daemon's next verify accepts the result.
///
/// Without `journal_mode=WAL` the mint below waits out `busy_timeout` behind the
/// reader's lock and then fails with "database is locked", which is exactly the
/// bug this asserts against.
#[test]
fn a_second_handle_on_the_same_file_can_still_mint() {
    let dir = std::env::temp_dir().join(format!("squelch-wal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two-handles.db");
    // A leftover file from a previous run would test the wrong thing.
    let _ = std::fs::remove_file(&path);

    let daemon = SqliteStore::open(&path).unwrap();
    let acct = daemon.ensure_account("me@example.com").unwrap();
    let cli = SqliteStore::open(&path).unwrap();

    {
        // An OPEN READ TRANSACTION, held across the mint. Under a rollback
        // journal this is the shared lock a writer cannot get past.
        let reader = daemon.lock().unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM device_tokens;")
            .unwrap();

        let issued = cli.issue_device_token(acct, "iPhone").unwrap();

        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);

        let verified = daemon
            .verify_device_token(&issued.token)
            .unwrap()
            .expect("the daemon accepts the token the CLI minted");
        assert_eq!(verified.id, issued.id);
        assert_eq!(verified.account_id, acct);
    }

    // The mode that makes the above true, on both handles. Asserted only for a
    // FILE-backed store: `:memory:` answers "memory" and there is nothing to
    // share.
    for store in [&daemon, &cli] {
        let mode: String = store
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    drop(daemon);
    drop(cli);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- upgrade path ------------------------------------------------------

#[test]
fn init_adds_both_tables_to_a_preexisting_db() {
    // A populated install that predates per-device tokens: it already has an
    // account and an audit log, and neither new table exists. Both arrive through
    // `CREATE TABLE IF NOT EXISTS` in schema.sql — new TABLES need no migrate.rs
    // seam, only new COLUMNS do — and the whole issue -> verify -> pair flow works
    // on the upgraded DB without disturbing what was already there.
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE accounts(
             id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL, created_at TEXT NOT NULL);
         CREATE TABLE audit_log(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, ts TEXT NOT NULL,
             actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT, detail TEXT);
         INSERT INTO accounts(id, email, created_at)
             VALUES (1, 'me@example.com', '2026-01-01T00:00:00Z');
         INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
             VALUES (1, '2026-01-02T00:00:00Z', 'client-api', 'archive', '7', NULL);",
    )
    .unwrap();

    let store = SqliteStore::init(conn).unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();
    assert_eq!(
        acct, 1,
        "the pre-existing account is reused, not duplicated"
    );

    let issued = store.issue_device_token(acct, "iPhone").unwrap();
    assert!(store.verify_device_token(&issued.token).unwrap().is_some());
    let minted = store.mint_pairing_code(acct, ttl()).unwrap();
    assert!(store.claim_pairing_code(&minted.code, "iPad").is_ok());
    assert_eq!(store.list_device_tokens(acct).unwrap().len(), 2);

    // The pre-existing history survived, with the new rows appended to it.
    let log = store.list_audit(acct, 50).unwrap();
    assert!(log.iter().any(|e| e.action == "archive"));
    assert!(log.iter().any(|e| e.action == "pairing.claim"));
}
