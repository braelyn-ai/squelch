//! Validation of everything the control plane sends.
//!
//! The warden is authenticated (only the control plane holds the bearer), so
//! none of this is defending against strangers. It is defending against a
//! *bug* in the control plane turning into a hostname collision, a systemd unit
//! named something unfortunate, an env file with an extra line in it, or a
//! plaintext refresh token at rest on the box. Each of those is unrecoverable
//! in a different way, so each gets an explicit refusal here rather than a
//! trusting `format!`.
//!
//! PRIVACY: no error here echoes the value it rejected. These strings go back
//! over the wire and into the control plane's logs, and one of the values is a
//! mailbox address.

/// Shortest tenant label. Three is the shortest subdomain worth the collision
/// risk, and it keeps the reserved list from having to enumerate every
//  two-letter word anyone might want.
pub const MIN_LABEL_LEN: usize = 3;

/// Longest tenant label. A DNS label may be 63, but the label also becomes a
/// systemd instance name, a file name, and a Caddy site block, and 30 is plenty
/// for a person's handle.
pub const MAX_LABEL_LEN: usize = 30;

/// Labels the deployment needs for itself, or that a user would read as
/// official. Taken verbatim from the hosted design contract; anything added
/// here must also be added to the control plane's copy, because a label that
/// passes there and fails here is a signup that dies after the Google consent.
pub const RESERVED_LABELS: &[&str] = &[
    "www", "mail", "smtp", "imap", "auth", "api", "admin", "signup", "warden", "mcp", "status",
    "help", "docs", "app", "relay", "track",
];

/// Longest address we will write into an env file. RFC 5321's ceiling.
pub const MAX_EMAIL_LEN: usize = 254;

/// Ceiling on the credential ciphertext. An age-armored `StoredToken` is a
/// couple of kilobytes; this is loose enough for a format change and tight
/// enough that the provisioning route is not a place to spend the box's disk.
pub const MAX_CIPHERTEXT: usize = 64 * 1024;

/// The first line of an age ASCII-armored file (RFC 7468 style, as produced by
/// `age --armor` / the `age` crate's `ArmoredWriter`).
///
/// This constant is the whole reason the warden can promise it never writes a
/// plaintext refresh token to disk: a body that does not start and end with the
/// armor is refused before anything touches the filesystem. It is restated here
/// rather than imported from the `age` crate because the warden does not link
/// age — it never decrypts, and a provisioner that could decrypt would be a
/// worse trust story than one that cannot.
pub const AGE_ARMOR_BEGIN: &str = "-----BEGIN AGE ENCRYPTED FILE-----";

/// The last line of an age ASCII-armored file.
pub const AGE_ARMOR_END: &str = "-----END AGE ENCRYPTED FILE-----";

/// Why a label was refused. Every message names the constraint, never the
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("label must be at least {MIN_LABEL_LEN} characters")]
    TooShort,
    #[error("label must be at most {MAX_LABEL_LEN} characters")]
    TooLong,
    #[error("label may contain only a-z, 0-9 and hyphen")]
    Charset,
    #[error("label may not start or end with a hyphen")]
    Hyphen,
    #[error("label is reserved")]
    Reserved,
}

/// Why an account address was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EmailError {
    #[error("account_email is empty")]
    Empty,
    #[error("account_email is longer than {MAX_EMAIL_LEN} characters")]
    TooLong,
    #[error("account_email must be a single address with one @ and text on both sides")]
    Shape,
    #[error("account_email contains a character that is not allowed in an environment file")]
    Charset,
}

/// Why a credential body was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CiphertextError {
    #[error("cred_read_ciphertext is empty")]
    Empty,
    #[error("cred_read_ciphertext is longer than {MAX_CIPHERTEXT} bytes")]
    TooLong,
    #[error("cred_read_ciphertext is not an age ASCII-armored file")]
    NotArmored,
    #[error("cred_read_ciphertext contains bytes that are not armor")]
    Charset,
}

/// Case-fold and trim a label into the one spelling everything downstream uses.
///
/// Done before validation, not after, so `Alice` and `alice` cannot both be
/// provisioned: DNS does not distinguish them and neither may we.
pub fn normalize_label(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Validate a normalized label, returning it.
///
/// Equivalent to `^[a-z0-9](?:[a-z0-9-]{1,28}[a-z0-9])?$` plus the reserved
/// list, written out because the length bound and the hyphen bound say
/// different things and a reader deserves to be told which one they tripped.
/// Hand-rolled rather than `regex`: this is four `bytes()` passes, and the
/// error a regex gives back is "no match".
pub fn validate_label(raw: &str) -> Result<String, LabelError> {
    let label = normalize_label(raw);
    if label.len() < MIN_LABEL_LEN {
        return Err(LabelError::TooShort);
    }
    if label.len() > MAX_LABEL_LEN {
        return Err(LabelError::TooLong);
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(LabelError::Charset);
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(LabelError::Hyphen);
    }
    if RESERVED_LABELS.contains(&label.as_str()) {
        return Err(LabelError::Reserved);
    }
    Ok(label)
}

/// Validate the mailbox address, returning it trimmed.
///
/// Case is preserved: the control plane learned this string from Google's
/// `users.getProfile`, the daemon compares it against what Google reports on
/// its own refreshes, and lowercasing it here would be this service inventing
/// an opinion about someone else's address.
///
/// The charset rule is the load-bearing one and it is not about email at all:
/// this value is written into a systemd `EnvironmentFile`, where a newline
/// would start a new assignment. A quote, a backslash, or a dollar sign would
/// each survive into systemd's own unquoting with a meaning nobody intended.
/// Real Google mailboxes contain none of them.
pub fn validate_account_email(raw: &str) -> Result<String, EmailError> {
    let email = raw.trim();
    if email.is_empty() {
        return Err(EmailError::Empty);
    }
    if email.len() > MAX_EMAIL_LEN {
        return Err(EmailError::TooLong);
    }
    if email
        .bytes()
        .any(|b| !(b'!'..=b'~').contains(&b) || matches!(b, b'"' | b'\'' | b'\\' | b'$' | b'`'))
    {
        return Err(EmailError::Charset);
    }
    let mut parts = email.split('@');
    let (local, domain, extra) = (parts.next(), parts.next(), parts.next());
    match (local, domain, extra) {
        (Some(l), Some(d), None) if !l.is_empty() && d.contains('.') && !d.starts_with('.') => {
            Ok(email.to_string())
        }
        _ => Err(EmailError::Shape),
    }
}

/// Validate the credential body and normalize its trailing newline.
///
/// The armor check is the invariant: the warden writes what the control plane
/// hands it, verbatim and unread, into a file the daemon will decrypt. If that
/// body were ever a plaintext `StoredToken` — a control-plane bug, a
/// misconfigured recipient, a copy-paste — this is the last place on the path
/// that can notice, because nothing downstream reads it until systemd has
/// already started a daemon on it. So a body that is not armored is a refusal,
/// never a write.
///
/// It is NOT a proof that the ciphertext decrypts, or that it was encrypted to
/// this box's recipient. Nothing here can know that: the warden holds no age
/// identity, by design. A body armored to the wrong recipient fails loudly at
/// the daemon instead, which is the right place for it.
pub fn validate_ciphertext(raw: &str) -> Result<String, CiphertextError> {
    let body = raw.trim();
    if body.is_empty() {
        return Err(CiphertextError::Empty);
    }
    if raw.len() > MAX_CIPHERTEXT {
        return Err(CiphertextError::TooLong);
    }
    // Armor is base64 plus the two banner lines, so the only bytes that may
    // appear are printable ASCII and the line breaks between them. This also
    // means the file we write can never contain a NUL or a control byte.
    if body
        .bytes()
        .any(|b| !(b == b'\n' || b == b'\r' || (b' '..=b'~').contains(&b)))
    {
        return Err(CiphertextError::Charset);
    }
    let mut lines = body.lines();
    let first = lines.next().unwrap_or_default().trim_end();
    let last = body.lines().next_back().unwrap_or_default().trim_end();
    if first != AGE_ARMOR_BEGIN || last != AGE_ARMOR_END {
        return Err(CiphertextError::NotArmored);
    }
    // Exactly one trailing newline: age's armored reader accepts either, and a
    // file that ends in a newline is the one that does not surprise a human
    // running `cat` on it during an incident.
    Ok(format!("{body}\n"))
}

/// Whether a string is a pairing code as `squelchd pair` prints it: two groups
/// of four Crockford base32 symbols, joined by one hyphen.
///
/// Used to sanity-check what came back from the pair exec before it goes on the
/// wire. The alphabet is the daemon's (`0123456789ABCDEFGHJKMNPQRSTVWXYZ` —
/// no I, L, O or U), restated rather than imported for the same reason the age
/// armor banner is.
pub fn is_pairing_code(s: &str) -> bool {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let Some((a, b)) = s.split_once('-') else {
        return false;
    };
    let ok = |g: &str| g.len() == 4 && g.bytes().all(|c| ALPHABET.contains(&c));
    ok(a) && ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_labels() {
        for label in ["abc", "alice", "a-b-c", "x1y2z3", &"a".repeat(MAX_LABEL_LEN)] {
            assert_eq!(validate_label(label).as_deref(), Ok(label), "{label}");
        }
    }

    #[test]
    fn case_folds_before_validating() {
        assert_eq!(validate_label("  ALICE  ").as_deref(), Ok("alice"));
        // The fold happens first, so a reserved label cannot be smuggled in
        // wearing capitals.
        assert_eq!(validate_label("WWW"), Err(LabelError::Reserved));
    }

    #[test]
    fn refuses_bad_labels() {
        assert_eq!(validate_label("ab"), Err(LabelError::TooShort));
        assert_eq!(validate_label(""), Err(LabelError::TooShort));
        assert_eq!(
            validate_label(&"a".repeat(MAX_LABEL_LEN + 1)),
            Err(LabelError::TooLong)
        );
        assert_eq!(validate_label("-abc"), Err(LabelError::Hyphen));
        assert_eq!(validate_label("abc-"), Err(LabelError::Hyphen));
        assert_eq!(validate_label("a_b"), Err(LabelError::Charset));
        assert_eq!(validate_label("a.b"), Err(LabelError::Charset));
        assert_eq!(validate_label("a/b"), Err(LabelError::Charset));
        assert_eq!(validate_label("../etc"), Err(LabelError::Charset));
        assert_eq!(validate_label("a b"), Err(LabelError::Charset));
        assert_eq!(validate_label("café"), Err(LabelError::Charset));
        assert_eq!(validate_label("mcp"), Err(LabelError::Reserved));
    }

    #[test]
    fn accepts_ordinary_addresses() {
        assert_eq!(
            validate_account_email(" Alice.B@example.com ").as_deref(),
            Ok("Alice.B@example.com")
        );
    }

    #[test]
    fn refuses_addresses_that_would_break_an_env_file() {
        // The whole point: a newline here would append an assignment of the
        // attacker's choosing to the tenant's environment.
        assert_eq!(
            validate_account_email("a@example.com\nSQUELCH_API_TOKEN=x"),
            Err(EmailError::Charset)
        );
        assert_eq!(
            validate_account_email("a@example.com\r\nX=1"),
            Err(EmailError::Charset)
        );
        assert_eq!(
            validate_account_email("\"a\"@example.com"),
            Err(EmailError::Charset)
        );
        assert_eq!(
            validate_account_email("a$b@example.com"),
            Err(EmailError::Charset)
        );
        assert_eq!(validate_account_email(""), Err(EmailError::Empty));
        assert_eq!(validate_account_email("nobody"), Err(EmailError::Shape));
        assert_eq!(validate_account_email("a@b"), Err(EmailError::Shape));
        assert_eq!(
            validate_account_email("a@b@example.com"),
            Err(EmailError::Shape)
        );
        assert_eq!(
            validate_account_email(&format!("{}@example.com", "a".repeat(MAX_EMAIL_LEN))),
            Err(EmailError::TooLong)
        );
    }

    fn armored(body: &str) -> String {
        format!("{AGE_ARMOR_BEGIN}\n{body}\n{AGE_ARMOR_END}")
    }

    #[test]
    fn accepts_armored_ciphertext_and_normalizes_the_newline() {
        let ct = armored("YWdlLWVuY3J5cHRpb24ub3JnL3Yx");
        let out = validate_ciphertext(&ct).unwrap();
        assert_eq!(out, format!("{ct}\n"));
        // Already newline-terminated, or terminated several times over, lands
        // on the same bytes.
        assert_eq!(validate_ciphertext(&format!("{ct}\n\n\n")).unwrap(), out);
    }

    #[test]
    fn refuses_anything_that_is_not_armor() {
        // The case that matters: a plaintext StoredToken. It must never reach
        // the disk, so it must never get past this function.
        let plaintext = r#"{"refresh_token":"1//0gPLAINTEXT","access_token":"ya29."}"#;
        assert_eq!(
            validate_ciphertext(plaintext),
            Err(CiphertextError::NotArmored)
        );
        assert_eq!(validate_ciphertext(""), Err(CiphertextError::Empty));
        assert_eq!(validate_ciphertext("   \n "), Err(CiphertextError::Empty));
        // Begins right, ends wrong: a truncated body would decrypt to nothing
        // at the daemon, hours later.
        assert_eq!(
            validate_ciphertext(&format!("{AGE_ARMOR_BEGIN}\nYWdl")),
            Err(CiphertextError::NotArmored)
        );
        assert_eq!(
            validate_ciphertext(&armored("YWdl\0YmluYXJ5")),
            Err(CiphertextError::Charset)
        );
        assert_eq!(
            validate_ciphertext(&"a".repeat(MAX_CIPHERTEXT + 1)),
            Err(CiphertextError::TooLong)
        );
    }

    #[test]
    fn recognizes_pairing_codes() {
        assert!(is_pairing_code("ABCD-1234"));
        assert!(is_pairing_code("0000-0000"));
        // Not in the Crockford alphabet: I, L, O, U.
        assert!(!is_pairing_code("ABCI-1234"));
        assert!(!is_pairing_code("abcd-1234"));
        assert!(!is_pairing_code("ABCD1234"));
        assert!(!is_pairing_code("ABC-1234"));
        assert!(!is_pairing_code("ABCD-1234-5678"));
        assert!(!is_pairing_code(""));
    }
}
