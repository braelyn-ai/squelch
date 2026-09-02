//! Stage-1 sealed-message detector: auth mail (2FA codes, password resets, magic
//! links, login alerts, verification) must be detected BEFORE any other pass reads
//! the body. Biases to recall over precision — a false seal only hides benign mail
//! from the agent, a false negative leaks a code to an LLM/agent.
//! See docs/SECURITY.md §4.

use crate::triage::text::{any, rx};
use crate::types::SealedKind;
use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

/// How far a bare code line may sit from the auth phrasing that vouches for it,
/// in bytes. Real code mail puts them within a line or two of each other (0-30
/// bytes across the corpus this was tuned on); a receipt's ZIP code and its
/// unrelated "auth code" boilerplate were 1.6k apart.
const CODE_PHRASE_WINDOW: usize = 300;

/// Patterns naming a concrete, reader-addressed code. Shared by
/// [`Detector::otp`] and [`Detector::otp_code`] so a tightening cannot land in
/// one copy and miss the other.
const CONCRETE_CODE: &[&str] = &[
    r"\bcode[:\s]+\d{4,8}\b",
    // The code must be followed by a word naming it as one. Without that, any
    // year clears the bar: "2027 is your year" in a conference blast sealed as
    // a login code, and it beat the marketing guard on the way past.
    r"\b\d{4,8}\s+is your\b[^\n]{0,40}?\b(code|otp|passcode|password|pin|token)\b",
    // Likewise the code itself must follow, shaped like one (case-sensitive:
    // codes are upper/digits). "Your code is simpler with only one auth
    // pattern" is a developer newsletter talking about SOURCE code.
    r"\byour code is[:\s]+(?-i:[A-Z0-9][A-Z0-9-]{3,9})\b",
    r"\benter (this|the following) code\b",
];

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
    /// A code standing alone on its own line, which is how a big rendered
    /// `<h1>482913</h1>` flattens: no adjacent "code" word for [`Self::otp_code`]
    /// to anchor on. Too weak to seal alone (order numbers, ZIP codes), so it
    /// counts as a concrete code only alongside NEARBY auth phrasing — see
    /// [`detect_sealed`].
    code_line: Vec<Regex>,
    /// Phrases POINTING at a code rendered elsewhere in the mail. They name no
    /// code themselves, which is why a discount blast wears the same words
    /// ("use the code LUMA for 20% off"), so they seal only with a code-shaped
    /// number near them.
    code_pointer: Vec<Regex>,
    /// Any code-shaped number, wherever it sits. Only ever read as the thing a
    /// [`Self::code_pointer`] points at.
    code_run: Vec<Regex>,
    /// Marketing / newsletter markers. When these fire, topical auth mentions are
    /// ignored: an auth vendor's newsletter discusses 2FA/SSO/magic-links as
    /// PRODUCTS, and a real auth email is transactional, never a blast.
    marketing: Vec<Regex>,
}

fn detector() -> &'static Detector {
    static D: OnceLock<Detector> = OnceLock::new();
    D.get_or_init(|| Detector {
        otp: [
            // Topical: names the mechanism, with or without a code present.
            r"\bone[-\s]?time (pass)?code\b",
            r"\b(verification|security|login|auth(?:entication)?|access) code\b",
            r"\bOTP\b",
            r"\btwo[-\s]?factor\b",
            r"\b2fa\b",
        ]
        .iter()
        .chain(CONCRETE_CODE)
        .map(|p| rx(p))
        .collect(),
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
            // Brand-in-the-middle login verification ("Verify your PostHog
            // login") — the exact subject the 2026-08-20 miss wore.
            rx(r"\b(verify|confirm) your\b.{0,40}\b(login|log[-\s]?in|sign[-\s]?in|signin|device)\b"),
            rx(r"\bconfirm (it'?s|it is|that'?s) you\b"),
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
        otp_code: CONCRETE_CODE.iter().copied().map(rx).collect(),
        code_line: vec![rx(r"(?m)^\s*\d{4,8}\s*$")],
        code_pointer: vec![
            rx(r"\buse (this|the)( following)? code\b"),
            rx(r"\b(the )?code below\b"),
        ],
        code_run: vec![rx(r"\b\d{4,8}\b")],
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

/// Returns `Some(kind)` if the message should be sealed. Ordering encodes
/// priority when multiple signals fire (OTP is the most sensitive).
pub fn detect_sealed(input: &SealInput) -> Option<SealedKind> {
    let d = detector();
    // Auth phrasing never lives in a URL's query string or fragment, but
    // percent-encoding there manufactures matches out of nothing: a tracking
    // blob carrying `%2Fa%2B` reads as a word-bounded "2fa" and sealed an Apple
    // developer newsletter. Drop those before any battery reads the text.
    let subject = strip_url_params(input.subject);
    let body = strip_url_params(input.body);
    let hay = [subject.as_ref(), body.as_ref()];

    // A concrete reader-addressed code always seals — it wins over the marketing
    // guard below, because a leaked code is the highest-stakes miss.
    if any(&d.otp_code, &hay) {
        return Some(SealedKind::Otp);
    }
    // A code can also be rendered away from the words that vouch for it, in
    // two shapes: auth phrasing beside a code standing alone on its own line
    // (a big rendered `<h1>482913</h1>` flattens to exactly that), and a
    // pointer phrase beside any code-shaped number ("use the following
    // code" … 482913). Both win over the marketing guard too — the phrasing may
    // be exactly what the guard would veto ("verify your login" + footer) and
    // the code is still real.
    //
    // Neither half seals alone: a bare number is any order number or ZIP code,
    // and a pointer with no number is a discount ("use the code LUMA for 20%
    // off"). Nor does the pair seal at any distance — a registrar's order
    // summary mentions an "auth code" 1.6k bytes from the ZIP in its billing
    // address. Phrasing in the SUBJECT is exempt: a subject speaks for the
    // whole body.
    let vouched = |phrases: &[&[Regex]], targets: &[Regex]| -> bool {
        let hits = spans(targets, body.as_ref());
        !hits.is_empty()
            && (phrases.iter().any(|p| any(p, &[subject.as_ref()])) || {
                let near_by: Vec<_> = phrases
                    .iter()
                    .flat_map(|p| spans(p, body.as_ref()))
                    .collect();
                near(&hits, &near_by, CODE_PHRASE_WINDOW)
            })
    };
    if vouched(&[&d.otp, &d.magic_link, &d.verification], &d.code_line)
        || vouched(&[&d.code_pointer], &d.code_run)
    {
        return Some(SealedKind::Otp);
    }

    // Auth-vendor newsletters discuss 2FA / SSO / magic-links as PRODUCTS; those
    // topical mentions must NOT seal.
    if any(&d.marketing, &hay) {
        return None;
    }

    if any(&d.otp, &hay) {
        return Some(SealedKind::Otp);
    }
    if any(&d.password_reset, &hay) {
        return Some(SealedKind::PasswordReset);
    }
    if any(&d.magic_link, &hay) {
        return Some(SealedKind::MagicLink);
    }
    if any(&d.login_alert, &hay) {
        return Some(SealedKind::LoginAlert);
    }
    if any(&d.verification, &hay) {
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
    if sender_is_security && sender_is_financial && any(&d.login_soft, &hay) {
        return Some(SealedKind::LoginAlert);
    }
    None
}

/// Strips every URL's query string and fragment, keeping scheme, host and path.
fn strip_url_params(s: &str) -> Cow<'_, str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| rx(r#"(https?://[^\s<>"']*?)[?#][^\s<>"']*"#))
        .replace_all(s, "${1}")
}

/// Byte spans of every match of `battery` in `hay`.
fn spans(battery: &[Regex], hay: &str) -> Vec<(usize, usize)> {
    battery
        .iter()
        .flat_map(|re| re.find_iter(hay).map(|m| (m.start(), m.end())))
        .collect()
}

/// Does any `a` span sit within `window` bytes of any `b` span (overlap counts)?
fn near(a: &[(usize, usize)], b: &[(usize, usize)], window: usize) -> bool {
    a.iter().any(|x| {
        // One of the two subtractions saturates to 0 whichever span leads, and
        // both do when they overlap.
        b.iter()
            .any(|y| y.0.saturating_sub(x.1) <= window && x.0.saturating_sub(y.1) <= window)
    })
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
            (
                "Change your password",
                "someone requested a password change",
            ),
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

    /// The 2026-08-20 miss: a login-code email whose transactional footer
    /// tripped the marketing guard, whose code rendered as a bare line no
    /// `otp_code` pattern anchored on, and whose subject put the brand between
    /// "your" and "login". It reached Stage-1 as normal mail and was buried as
    /// noise. Every arm of the shape must seal on its own now.
    #[test]
    fn posthog_shaped_login_code_seals() {
        // The full shape: brandy subject + "use the following code" + bare
        // code line + receiving-this-email footer.
        assert_eq!(
            detect_sealed(&inp_from(
                "noreply@posthog.com",
                "Verify your PostHog login",
                "Are you who you say you are? Just checking! Use the following \
                 code to verify your identity.\n482913\nYou're receiving this \
                 email because a login was attempted.",
            )),
            Some(SealedKind::Otp),
        );
        // Bare code line + auth phrasing, still under a marketing-shaped footer.
        assert_eq!(
            detect_sealed(&inp_from(
                "noreply@posthog.com",
                "Verify your PostHog login",
                "Confirm it's you.\n551204\nYou're receiving this email because \
                 someone tried to log in.",
            )),
            Some(SealedKind::Otp),
        );
        // Subject alone (image-only body must still seal).
        assert_eq!(
            detect_sealed(&inp_from(
                "noreply@posthog.com",
                "Verify your PostHog login",
                "",
            )),
            Some(SealedKind::Verification),
        );
    }

    #[test]
    fn bare_digit_lines_alone_do_not_seal() {
        // An order number on its own line, no auth phrasing: not auth mail.
        assert_eq!(
            detect_sealed(&inp(
                "Your order shipped",
                "Order number\n448201\nYour package is on the way.",
            )),
            None,
        );
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

    /// The 2026-09-02 false seal: a conference save-the-date whose copy read
    /// "2027 is your year". A YEAR followed by "is your" cleared the
    /// concrete-code bar, which runs ahead of the marketing guard, so the
    /// Mailchimp footer could not save it.
    #[test]
    fn a_year_in_marketing_copy_is_not_a_code() {
        assert_eq!(
            detect_sealed(&inp_from(
                "hello@stepconference.com",
                "Hello Dubai. We're Back Feb 3-4.",
                "If you've done Step Dubai before, you know.\nIf you haven't, \
                 2027 is your year.\nThis is only the date.\nunsubscribe",
            )),
            None,
        );
        // The shape it exists for still seals.
        assert_eq!(
            detect_sealed(&inp("268657 is your Luma sign-in code", "")),
            Some(SealedKind::Otp),
        );
        assert_eq!(
            detect_sealed(&inp("", "482913 is your one-time passcode")),
            Some(SealedKind::Otp),
        );
    }

    /// A developer newsletter discussing SOURCE code: "your code is" with no
    /// code after it is not an OTP, and the marketing guard must get its turn.
    #[test]
    fn source_code_talk_is_not_a_code() {
        assert_eq!(
            detect_sealed(&inp_from(
                "updates@workos.com",
                "New this month: API Gateway",
                "No round trip to verify the API key. Your code is simpler with \
                 only one auth pattern, and 2FA still works. Unsubscribe.",
            )),
            None,
        );
        // A real code after the phrase still seals, whatever its shape.
        for b in [
            "Your code is: FHLSB8",
            "Your code is 482913",
            "your code is G-482913",
        ] {
            assert_eq!(
                detect_sealed(&inp("Sign in", b)),
                Some(SealedKind::Otp),
                "real code missed: {b:?}"
            );
        }
    }

    /// Percent-encoding inside a tracking URL manufactured a word-bounded
    /// "2fa" (`%2Fa%2B`) and sealed an Apple developer newsletter.
    #[test]
    fn percent_encoded_urls_do_not_fake_auth_phrasing() {
        assert_eq!(
            detect_sealed(&inp_from(
                "developer@insideapple.apple.com",
                "Tax and Price Updates for Apps",
                "Read the announcement: https://developer.apple.com/go?t=\
                 KrgH%2BG5BpAK%2Fa%2B9JbW62h5UjcY3uQrj32ua#otp",
            )),
            None,
        );
        // The strip stops at the URL: text after it is still read.
        assert_eq!(
            detect_sealed(&inp(
                "Action needed",
                "https://acme.com/go?u=123&c=456 Verify your email to finish.",
            )),
            Some(SealedKind::Verification),
        );
    }

    /// A registrar's order summary carries a ZIP code on its own line and,
    /// 1.6k bytes later, boilerplate about domain-transfer "auth codes". The
    /// bare-code arm must not marry the two.
    #[test]
    fn a_bare_code_line_needs_phrasing_near_it() {
        let far = format!(
            "Billing address\nSan Francisco\nCA\n94114\nUS\n{}\nverify your \
             email to finish. unsubscribe",
            "Thanks for your order. ".repeat(40),
        );
        assert_eq!(detect_sealed(&inp("Order Summary #209501748", &far)), None);
        // Same phrasing, next to the digits: sealed.
        let near = "Verify your email.\n94114\nunsubscribe";
        assert_eq!(
            detect_sealed(&inp("Order Summary #209501748", near)),
            Some(SealedKind::Otp),
        );
        // Phrasing in the subject vouches for a code anywhere in the body.
        assert_eq!(
            detect_sealed(&inp(
                "Your verification code",
                &format!("Hi there.\n{}\n482913\n", "filler text. ".repeat(60)),
            )),
            Some(SealedKind::Otp),
        );
    }

    /// A pointer phrase ("use the code", "the code below") names no code, so a
    /// discount blast wears the same words. It seals only with a code-shaped
    /// number near it.
    #[test]
    fn a_promo_code_pointer_is_not_a_login_code() {
        assert_eq!(
            detect_sealed(&inp_from(
                "resend@calendar.luma-mail.com",
                "Crafting high-quality software",
                "Tickets are on sale today. As a thank you for being part of our \
                 community, you can use the code LUMA for 20% off. Reserve your \
                 spot.",
            )),
            None,
        );
        // The same pointer with an actual code near it still seals.
        assert_eq!(
            detect_sealed(&inp(
                "Sign in",
                "Use the code below to continue.\n\n482913\n",
            )),
            Some(SealedKind::Otp),
        );
        assert_eq!(
            detect_sealed(&inp("Sign in", "Use this code to continue: 482913")),
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
