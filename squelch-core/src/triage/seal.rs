//! Stage-1 sealed-message detector.
//!
//! Auth-related mail (2FA codes, password resets, magic links, login alerts,
//! verification) must be sealed BEFORE anything else looks at it. The detector
//! biases toward over-sealing: recall over precision. A false positive just
//! hides a benign email from the agent (the TUI still shows it); a false
//! negative leaks a security-sensitive code to an LLM/agent. We prefer the
//! former.

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
    /// Sender-shape corroborators: security@/donotreply@/etc. at a financial-ish
    /// domain. Used to seal weaker login-ish phrasing when the sender looks like
    /// a bank/security notifier (bug #4).
    security_sender: Vec<Regex>,
    financial_domain: Vec<Regex>,
    /// Weaker login-ish phrasing that only seals when corroborated by a
    /// security-shaped sender.
    login_soft: Vec<Regex>,
}

fn rx(p: &str) -> Regex {
    // All patterns are authored case-insensitive.
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
            // bug #4: Schwab-style "Confirming your recent login".
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
            // Bank/broker-ish domains and mail-subdomains thereof.
            rx(r"@(mail\.)?(schwab|chase|wellsfargo|bankofamerica|bofa|citi|capitalone|amex|americanexpress|fidelity|vanguard|paypal|venmo|ally|discover|usbank|pnc|tdbank)\."),
            rx(r"@[^@]*(bank|creditunion|financial|fcu)\."),
        ],
        login_soft: vec![
            // Weaker phrasing that seals only with a security-shaped sender.
            rx(r"\blog(ged)?[-\s]?in\b"),
            rx(r"\bsign(ed)?[-\s]?in\b"),
            rx(r"\baccount (access|activity)\b"),
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
    // bug #4 corroborator: weaker login-ish phrasing seals when the sender looks
    // like a security/no-reply notifier at a financial-ish domain. Biased to
    // over-seal: a bank telling you about account access is auth-adjacent.
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
        // Confirmed against real mail: this LOGIN ALERT scored noise/40
        // fallthrough and was NOT sealed. It must be Sealed(LoginAlert).
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
        // Same weak phrasing from a random marketing sender => NOT sealed
        // (no strong phrasing, no security/financial sender shape).
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
        // Negative fixture: pure marketing that mentions "sign in to view your
        // offer" from a non-financial marketing sender should not seal.
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
