//! Validation of everything the control plane sends, and the one type that
//! carries a validated tenant label into a manifest.
//!
//! The warden is authenticated (only the control plane holds the bearer), so
//! none of this is defending against strangers. It is defending against a *bug*
//! in the control plane turning into a hostname collision, an object named
//! something unfortunate in a namespace that holds every tenant's mail, or a
//! plaintext refresh token at rest in a Secret. Each of those is unrecoverable
//! in a different way, so each gets an explicit refusal here rather than a
//! trusting `format!`.
//!
//! [`TenantName`] is the second half of the rule. Nothing in [`crate::objects`]
//! takes a `&str` label: every object name, selector value and hostname is
//! derived from a `TenantName`, and a `TenantName` exists only where
//! [`validate_label`] succeeded. That is what "a tenant label may appear solely
//! as a validated value inside typed object fields" means in types rather than
//! in a comment.
//!
//! PRIVACY: no error here echoes the value it rejected. These strings go back
//! over the wire and into the control plane's logs, and one of the values is a
//! mailbox address.

/// Shortest tenant label. Three is the shortest subdomain worth the collision
/// risk, and it keeps the reserved list from having to enumerate every
/// two-letter word anyone might want.
pub const MIN_LABEL_LEN: usize = 3;

/// Longest tenant label. A DNS label may be 63, but the label also becomes a
/// Kubernetes object name with a `-credential` suffix on it, and 30 is plenty
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

/// Longest address we will store. RFC 5321's ceiling.
pub const MAX_EMAIL_LEN: usize = 254;

/// Ceiling on the credential ciphertext. An age-armored credentials file is a
/// couple of kilobytes; this is loose enough for a format change and tight
/// enough that the provisioning route is not a place to spend etcd.
pub const MAX_CIPHERTEXT: usize = 64 * 1024;

/// Ceiling on the LLM virtual key. A gateway virtual key is a short opaque
/// token; this is loose enough for any token format and tight enough that the
/// route is not a place to park a document.
pub const MAX_LLM_API_KEY: usize = 4 * 1024;

/// The first line of an age ASCII-armored file (RFC 7468 style, as produced by
/// `age --armor` / the `age` crate's `ArmoredWriter`).
///
/// This constant is the whole reason the warden can promise it never stores a
/// plaintext refresh token: a body that does not start and end with the armor
/// is refused before anything reaches the API server. It is restated here
/// rather than taken from the `age` crate because the crate does not export it,
/// and because this check must keep working even if the warden's own use of age
/// (minting identities) ever goes away.
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
    #[error("account_email contains a character that is not allowed in an environment value")]
    Charset,
}

/// Why an LLM virtual key was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApiKeyError {
    #[error("api_key is empty")]
    Empty,
    #[error("api_key is longer than {MAX_LLM_API_KEY} bytes")]
    TooLong,
    #[error("api_key contains a character that is not allowed in a token")]
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

/// A tenant label that passed [`validate_label`], and the object names derived
/// from it.
///
/// Constructible only through [`TenantName::parse`]. Every manifest builder in
/// [`crate::objects`] takes one of these, so an unvalidated string cannot reach
/// a `metadata.name`, a selector, or an Ingress host without going through the
/// same four checks the wire does.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantName(String);

impl TenantName {
    /// Normalize and validate, or refuse.
    pub fn parse(raw: &str) -> Result<Self, LabelError> {
        validate_label(raw).map(Self)
    }

    /// The label itself. Also the name of the Deployment, the Service, the
    /// Ingress and the NetworkPolicy: one tenant, one name, four kinds.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `<label>-data`: the PersistentVolumeClaim. Survives a DELETE.
    pub fn data_pvc(&self) -> String {
        format!("{}-data", self.0)
    }

    /// `<label>-identity`: the Secret holding this tenant's age identity, its
    /// public recipient, and its account address. Created in phase one, and
    /// deleted by exactly one code path
    /// ([`crate::provision::Warden::sweep_pending`], and only while the tenant
    /// is still pending).
    pub fn identity_secret(&self) -> String {
        format!("{}-identity", self.0)
    }

    /// The label inside `<label>-identity`, for the sweep, which starts from a
    /// Secret name and has to get back to a validated tenant.
    pub fn from_identity_secret(secret_name: &str) -> Option<Self> {
        Self::parse(secret_name.strip_suffix("-identity")?).ok()
    }

    /// `<label>-credential`: the Secret holding the age-armored credentials
    /// file the control plane sealed to this tenant's recipient.
    pub fn credential_secret(&self) -> String {
        format!("{}-credential", self.0)
    }

    /// `<label>-llm`: the Secret holding this tenant's LLM gateway virtual
    /// key, minted per-tenant by the control plane and never read here.
    pub fn llm_secret(&self) -> String {
        format!("{}-llm", self.0)
    }
}

impl std::fmt::Display for TenantName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Case-fold and trim a label into the one spelling everything downstream uses.
///
/// Done before validation, not after, so `Alice` and `alice` cannot both be
/// provisioned: DNS does not distinguish them, Kubernetes names may not contain
/// capitals at all, and neither may we.
pub fn normalize_label(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Validate a normalized label, returning it.
///
/// Equivalent to `^[a-z0-9](?:[a-z0-9-]{1,28}[a-z0-9])?$` plus the reserved
/// list, written out because the length bound and the hyphen bound say
/// different things and a reader deserves to be told which one they tripped.
/// This is strictly tighter than DNS-1123, which is the point: Kubernetes would
/// accept 63 characters and a leading digit, and we would rather refuse here
/// than discover a name collision in a shared namespace.
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
/// this value becomes `SQUELCH_ACCOUNT_EMAIL` inside a container. A newline or
/// a control byte there is legal as far as Kubernetes is concerned and is
/// nonsense as far as every reader of it is, and real Google mailboxes contain
/// neither. The quote/backslash/dollar refusals are inherited from the v1 env
/// file and kept: nothing is gained by loosening them, and a shell one day
/// reading this value would find nothing to expand.
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
/// The armor check is the invariant: the warden puts what the control plane
/// hands it, verbatim and unread, into a Secret the daemon will decrypt. If
/// that body were ever a plaintext credentials file (a control-plane bug, a
/// misconfigured recipient, a copy-paste), this is the last place on the path
/// that can notice, because nothing downstream reads it until a daemon has
/// already been started on it. So a body that is not armored is a refusal,
/// never a write.
///
/// It is NOT a proof that the ciphertext decrypts, or that it was encrypted to
/// this tenant's recipient. Nothing here can know that: the warden holds no age
/// identity after the apply that created it, by design. A body armored to the
/// wrong recipient fails loudly at the daemon instead, which is the right place
/// for it.
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
    // means what we store can never contain a NUL or a control byte.
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

/// Validate the LLM virtual key, returning it trimmed.
///
/// The warden stores this verbatim in a Secret and never presents it to
/// anyone; the charset rule is the same one the account address obeys and for
/// the same reason: the value becomes an environment variable inside a
/// container (via `secretKeyRef`), and a newline or a control byte there is
/// legal to Kubernetes and nonsense to every reader. No shape beyond that is
/// assumed, because the gateway's token format is the gateway's business.
///
/// PRIVACY: this is a live credential. No error here echoes it, and nothing on
/// this path logs it.
pub fn validate_llm_api_key(raw: &str) -> Result<String, ApiKeyError> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(ApiKeyError::Empty);
    }
    if key.len() > MAX_LLM_API_KEY {
        return Err(ApiKeyError::TooLong);
    }
    if key
        .bytes()
        .any(|b| !(b'!'..=b'~').contains(&b) || matches!(b, b'"' | b'\'' | b'\\' | b'$' | b'`'))
    {
        return Err(ApiKeyError::Charset);
    }
    Ok(key.to_string())
}

/// Whether a string is a pairing code as `squelchd pair` prints it: two groups
/// of four Crockford base32 symbols, joined by one hyphen.
///
/// Used to sanity-check what came back from the pod exec before it goes on the
/// wire. The alphabet is the daemon's (`0123456789ABCDEFGHJKMNPQRSTVWXYZ`, no
/// I, L, O or U), restated rather than imported for the same reason the age
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
        for label in [
            "abc",
            "alice",
            "a-b-c",
            "x1y2z3",
            &"a".repeat(MAX_LABEL_LEN),
        ] {
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

    /// Every name a manifest can carry is derived here, so this is the test
    /// that says what a tenant occupies in the shared namespace.
    #[test]
    fn derives_every_object_name_from_a_validated_label() {
        let name = TenantName::parse("  ALICE ").unwrap();
        assert_eq!(name.as_str(), "alice");
        assert_eq!(name.to_string(), "alice");
        assert_eq!(name.data_pvc(), "alice-data");
        assert_eq!(name.identity_secret(), "alice-identity");
        assert_eq!(name.credential_secret(), "alice-credential");
        assert_eq!(name.llm_secret(), "alice-llm");
        // Longest allowed label plus the longest suffix still fits the 63-byte
        // DNS-1123 ceiling Kubernetes enforces on object names.
        let longest = TenantName::parse(&"a".repeat(MAX_LABEL_LEN)).unwrap();
        assert!(longest.credential_secret().len() <= 63);
        assert!(longest.llm_secret().len() <= 63);
    }

    #[test]
    fn a_tenant_name_cannot_be_built_from_a_bad_label() {
        assert_eq!(TenantName::parse("mcp"), Err(LabelError::Reserved));
        assert_eq!(TenantName::parse("a/b"), Err(LabelError::Charset));
    }

    #[test]
    fn accepts_ordinary_addresses() {
        assert_eq!(
            validate_account_email(" Alice.B@example.com ").as_deref(),
            Ok("Alice.B@example.com")
        );
    }

    #[test]
    fn refuses_addresses_that_would_break_an_environment_value() {
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
        // The case that matters: a plaintext credentials file. It must never
        // reach a Secret, so it must never get past this function.
        let plaintext = r#"{"slots":{"read:you@x.com":{"refresh_token":"1//0gPLAINTEXT"}}}"#;
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
    fn accepts_ordinary_api_keys_and_refuses_broken_ones() {
        assert_eq!(
            validate_llm_api_key(" sk-vk-abc123 ").as_deref(),
            Ok("sk-vk-abc123")
        );
        assert_eq!(validate_llm_api_key(""), Err(ApiKeyError::Empty));
        assert_eq!(validate_llm_api_key("   "), Err(ApiKeyError::Empty));
        assert_eq!(
            validate_llm_api_key(&"a".repeat(MAX_LLM_API_KEY + 1)),
            Err(ApiKeyError::TooLong)
        );
        // The one that would break an environment value, or smuggle a second
        // one in behind it.
        assert_eq!(
            validate_llm_api_key("sk-vk\nSQUELCH_API_TOKEN=x"),
            Err(ApiKeyError::Charset)
        );
        assert_eq!(validate_llm_api_key("sk$vk"), Err(ApiKeyError::Charset));
        assert_eq!(validate_llm_api_key("sk vk"), Err(ApiKeyError::Charset));
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
