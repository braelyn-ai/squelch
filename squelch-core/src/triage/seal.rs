//! Stage-1 sealed-message detector: auth mail (2FA codes, password resets, magic
//! links, login alerts, verification) must be detected BEFORE any other pass reads
//! the body. Biases to recall over precision — a false seal only hides benign mail
//! from the agent, a false negative leaks a code to an LLM/agent.
//! See docs/SECURITY.md §4.

use crate::types::SealedKind;
use regex::Regex;
use std::sync::OnceLock;

/// A message's text surfaces available to the detector.
pub struct SealInput<'a> {
    pub from_addr: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
}

struct Detector {
    otp: Vec<Regex>,
    password_reset: Vec<Regex>,
    magic_link: Vec<Regex>,
    login_alert: Vec<Regex>,
    verification: Vec<Regex>,
    /// Sender-shape corroborators: security@/donotreply@ at a financial-ish
    /// domain, used to seal weaker login-ish phrasing.
    security_sender: Vec<Regex>,
    financial_domain: Vec<Regex>,
    /// Weaker login-ish phrasing; seals only with a security-shaped sender.
    login_soft: Vec<Regex>,
    /// Concrete, reader-addressed OTP codes. These seal even past the marketing
    /// guard — a genuine code leak is the highest-stakes miss.
    otp_code: Vec<Regex>,
    /// Marketing / newsletter markers. When these fire, topical auth mentions are
    /// ignored: an auth vendor's newsletter discusses 2FA/SSO/magic-links as
    /// PRODUCTS, and a real auth email is transactional, never a blast.
    marketing: Vec<Regex>,
}

fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).expect("static seal regex must compile")
}

fn detector() -> &'static Detector {
    static D: OnceLock<Detector> = OnceLock::new();
    D.get_or_init(|| Detector {
        otp: vec![
            rx(r"\bone[-\s]?time (pass)?code\b"),
            rx(r"\b(verification|security|login|auth(?:entication)?|access) code\b"),
            rx(r"\bOTP\b"),
            rx(r"\byour code is\b"),
            rx(r"\bcode[:\s]+\d{4,8}\b"),
            rx(r"\b\d{4,8}\s+is your\b"),
            rx(r"\benter (this|the following) code\b"),
            rx(r"\btwo[-\s]?factor\b"),
            rx(r"\b2fa\b"),
        ],
        password_reset: vec![
            rx(r"\bpassword reset\b"),
            rx(r"\breset your password\b"),
            rx(r"\bforgot(ten)? (your )?password\b"),
            rx(r"\bchange your password\b"),
            rx(r"\bset (a )?new password\b"),
            rx(r"\bpassword (change|recovery)\b"),
        ],
        magic_link: vec![
            rx(r"\bmagic link\b"),
            rx(r"\bsign[-\s]?in link\b"),
            rx(r"\blog[-\s]?in link\b"),
            rx(r"\bclick (here|this link) to (sign|log)[-\s]?in\b"),
            rx(r"\buse this link to (sign|log)[-\s]?in\b"),
        ],
        login_alert: vec![
            rx(r"\bnew (sign[-\s]?in|login)\b"),
            rx(r"\bnew device\b"),
            rx(r"\bsuspicious (sign[-\s]?in|login|activity)\b"),
            rx(r"\bunusual (sign[-\s]?in|login|activity)\b"),
            rx(r"\bsecurity alert\b"),
            rx(r"\bwas this you\b"),
            rx(r"\bsomeone (just )?(signed|logged) in\b"),
            rx(r"\bsign[-\s]?in (attempt|detected)\b"),
            rx(r"\bconfirming your (recent )?login\b"),
            rx(r"\bsigned in (to|from)\b"),
            rx(r"\blogin (from|detected|alert)\b"),
            rx(r"\brecent login\b"),
        ],
        verification: vec![
            rx(r"\bverify your (email|account|identity|address)\b"),
            rx(r"\bconfirm your (email|account|address)\b"),
            rx(r"\bemail verification\b"),
            rx(r"\bactivate your account\b"),
            rx(r"\bverification (link|email|request)\b"),
        ],
        security_sender: vec![
            rx(r"^(security|secure|donotreply|do[-_.]?not[-_.]?reply|no[-_.]?reply|alerts?|account|notify|notifications?)@"),
        ],
        financial_domain: vec![
            rx(r"@(mail\.)?(schwab|chase|wellsfargo|bankofamerica|bofa|citi|capitalone|amex|americanexpress|fidelity|vanguard|paypal|venmo|ally|discover|usbank|pnc|tdbank)\."),
            rx(r"@[^@]*(bank|creditunion|financial|fcu)\."),
        ],
        login_soft: vec![
            rx(r"\blog(ged)?[-\s]?in\b"),
            rx(r"\bsign(ed)?[-\s]?in\b"),
            rx(r"\baccount (access|activity)\b"),
        ],
        otp_code: vec![
            rx(r"\bcode[:\s]+\d{4,8}\b"),
            rx(r"\b\d{4,8}\s+is your\b"),
            rx(r"\byour code is\b"),
            rx(r"\benter (this|the following) code\b"),
        ],
        marketing: vec![
            rx(r"\bunsubscribe\b"),
            rx(r"\bview (this )?(email|message)?\s*in (your )?browser\b"),
            rx(r"\bmanage (your )?(email |notification )?preferences\b"),
            rx(r"\b(email|notification) preferences\b"),
            rx(r"\byou('?re| are) receiving this (email|message)\b"),
            rx(r"\bwebinar\b"),
            rx(r"\bnewsletter\b"),
        ],
    })
}

fn any_match(regexes: &[Regex], haystacks: &[&str]) -> bool {
    regexes
        .iter()
        .any(|re| haystacks.iter().any(|h| re.is_match(h)))
}

/// Returns `Some(kind)` if the message should be sealed. Ordering encodes
/// priority when multiple signals fire (OTP is the most sensitive).
pub fn detect_sealed(input: &SealInput) -> Option<SealedKind> {
    let d = detector();
    let hay = [input.subject, input.body];

    // A concrete reader-addressed code always seals — it wins over the marketing
    // guard below, because a leaked code is the highest-stakes miss.
    if any_match(&d.otp_code, &hay) {
        return Some(SealedKind::Otp);
    }

    // Auth-vendor newsletters discuss 2FA / SSO / magic-links as PRODUCTS; those
    // topical mentions must NOT seal.
    if any_match(&d.marketing, &hay) {
        return None;
    }

    if any_match(&d.otp, &hay) {
        return Some(SealedKind::Otp);
    }
    if any_match(&d.password_reset, &hay) {
        return Some(SealedKind::PasswordReset);
    }
    if any_match(&d.magic_link, &hay) {
        return Some(SealedKind::MagicLink);
    }
    if any_match(&d.login_alert, &hay) {
        return Some(SealedKind::LoginAlert);
    }
    if any_match(&d.verification, &hay) {
        return Some(SealedKind::Verification);
    }
    // Weak login-ish phrasing seals when the sender is a security/no-reply
    // notifier at a financial-ish domain — biased to over-seal.
    let sender_is_security = d
        .security_sender
        .iter()
        .any(|re| re.is_match(input.from_addr));
    let sender_is_financial = d
        .financial_domain
        .iter()
        .any(|re| re.is_match(input.from_addr));
    if sender_is_security && sender_is_financial && any_match(&d.login_soft, &hay) {
        return Some(SealedKind::LoginAlert);
    }
    None
}

/// Convenience: is this message sealed?
pub fn is_sealed(input: &SealInput) -> bool {
    detect_sealed(input).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp<'a>(subject: &'a str, body: &'a str) -> SealInput<'a> {
        SealInput {
            from_addr: "noreply@service.com",
            subject,
            body,
        }
    }

    #[test]
    fn catches_otp_samples() {
        let cases = [
            ("Your verification code", "123456 is your code"),
            ("Sign-in", "Your one-time passcode is 8842"),
            ("Login", "Enter this code: 447120 to continue"),
            ("2FA", "Your OTP is ready"),
            ("Security", "your login code is below"),
            ("", "Use two-factor authentication code 90210"),
        ];
        for (s, b) in cases {
            assert_eq!(
                detect_sealed(&inp(s, b)),
                Some(SealedKind::Otp),
                "OTP not sealed: subject={s:?} body={b:?}"
            );
        }
    }

    #[test]
    fn catches_password_reset_samples() {
        let cases = [
            ("Password reset requested", "Click to reset your password"),
            ("", "You asked to reset your password"),
            ("Change your password", "someone requested a password change"),
            ("Forgot your password?", "here is how to recover"),
        ];
        for (s, b) in cases {
            assert_eq!(
                detect_sealed(&inp(s, b)),
                Some(SealedKind::PasswordReset),
                "reset not sealed: {s:?}/{b:?}"
            );
        }
    }

    #[test]
    fn catches_magic_link_and_login_and_verification() {
        assert_eq!(
            detect_sealed(&inp("Sign in", "Here is your magic link")),
            Some(SealedKind::MagicLink)
        );
        assert_eq!(
            detect_sealed(&inp("New sign-in to your account", "was this you?")),
            Some(SealedKind::LoginAlert)
        );
        assert_eq!(
            detect_sealed(&inp("Verify your email", "confirm your account")),
            Some(SealedKind::Verification)
        );
    }

    /// Build an input with an explicit sender (for sender-shape corroboration).
    fn inp_from<'a>(from: &'a str, subject: &'a str, body: &'a str) -> SealInput<'a> {
        SealInput {
            from_addr: from,
            subject,
            body,
        }
    }

    #[test]
    fn bug4_schwab_login_confirmation_seals() {
        // A bank's login-confirmation notice must seal as LoginAlert.
        let got = detect_sealed(&inp_from(
            "donotreply@mail.schwab.com",
            "Confirming your recent login",
            "We're confirming your recent login to your Schwab account.",
        ));
        assert_eq!(got, Some(SealedKind::LoginAlert));
    }

    #[test]
    fn extended_login_alert_phrasings_seal() {
        // These seal on phrasing alone, any sender.
        let cases = [
            "New sign-in to your account",
            "New sign in detected",
            "You signed in to a new device",
            "signed in from a new location",
            "Login from an unrecognized device",
            "login detected",
            "login alert",
            "Security alert on your account",
            "Unusual activity detected",
            "Unusual sign-in detected",
        ];
        for s in cases {
            assert_eq!(
                detect_sealed(&inp(s, "")),
                Some(SealedKind::LoginAlert),
                "login phrasing not sealed: {s:?}"
            );
        }
    }

    #[test]
    fn soft_login_phrasing_seals_only_for_security_financial_sender() {
        // Weak phrasing ("account access") from a bank's no-reply => sealed.
        assert_eq!(
            detect_sealed(&inp_from(
                "security@mail.chase.com",
                "Account access",
                "There was recent account access on your profile.",
            )),
            Some(SealedKind::LoginAlert),
        );
        // Same weak phrasing from a marketing sender => NOT sealed.
        assert_eq!(
            detect_sealed(&inp_from(
                "hello@randomshop.com",
                "Account access",
                "Manage your account access preferences.",
            )),
            None,
        );
    }

    #[test]
    fn marketing_signin_offer_does_not_seal() {
        // Marketing that mentions signing in, from a non-financial sender.
        assert_eq!(
            detect_sealed(&inp_from(
                "deals@shopmail.com",
                "Sign in to view your exclusive offer",
                "Sign in to see 20% off. Unsubscribe anytime.",
            )),
            None,
        );
    }

    #[test]
    fn auth_vendor_newsletter_does_not_seal() {
        // Auth vendors' newsletters discuss 2FA / SSO / magic links as PRODUCTS:
        // with marketing markers present, topical auth mentions must NOT seal.
        let cases = [
            (
                "marketing@workos.com",
                "Ship SSO and 2FA faster",
                "Our new magic link and two-factor APIs are live. Read the blog. Unsubscribe.",
            ),
            (
                "hello@auth0.com",
                "The passwordless newsletter",
                "Everything about OTP and magic links this month. Manage your email preferences.",
            ),
            (
                "team@clerk.com",
                "Add authentication in minutes",
                "New: verify your users with a one-time passcode flow. View this email in your browser.",
            ),
        ];
        for (f, s, b) in cases {
            assert_eq!(
                detect_sealed(&inp_from(f, s, b)),
                None,
                "auth-vendor newsletter wrongly sealed: {f:?} {s:?}"
            );
        }
    }

    #[test]
    fn real_code_seals_even_with_marketing_footer() {
        // The concrete-code check wins over the marketing guard.
        assert_eq!(
            detect_sealed(&inp_from(
                "noreply@service.com",
                "Your verification code",
                "Your code is 448201. If this wasn't you, ignore. Unsubscribe.",
            )),
            Some(SealedKind::Otp),
        );
    }

    #[test]
    fn leaves_normal_mail_alone() {
        let cases = [
            ("Lunch tomorrow?", "Want to grab lunch around noon?"),
            ("Q3 report", "Attached is the quarterly report."),
            ("Re: project timeline", "Let's push the deadline a week."),
            ("Your order shipped", "Your package is on the way."),
        ];
        for (s, b) in cases {
            assert_eq!(
                detect_sealed(&inp(s, b)),
                None,
                "false positive sealed: {s:?}/{b:?}"
            );
        }
    }
}
