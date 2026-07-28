//! APNs provider authentication tokens.
//!
//! Apple wants an ES256 JWT signed with the `.p8` key, header `{alg, kid}` and
//! claims `{iss = team id, iat}`. There is no `exp`: APNs rejects a token whose
//! `iat` is older than one hour and rate-limits providers that re-sign on every
//! request, so the token is CACHED and re-signed only past [`MAX_TOKEN_AGE`].
//!
//! SECURITY: the signed token is a bearer credential for the whole app
//! identifier. It is never logged and never returned to a caller.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

/// Re-sign once the cached token is older than this. Apple's documented window
/// is 20-60 minutes: under 20 the provider gets throttled, over 60 the token is
/// rejected. 50 minutes sits inside both bounds with room for clock skew.
pub const MAX_TOKEN_AGE_SECS: i64 = 50 * 60;

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    /// The `.p8` was not a usable PKCS#8 EC private key.
    #[error("APNs key is not a usable ES256 PKCS#8 PEM: {0}")]
    Key(String),
    #[error("failed to sign APNs token: {0}")]
    Sign(String),
}

/// The APNs JWT claim set. `iat` only — see the module docs on `exp`.
#[derive(Debug, Serialize)]
struct Claims<'a> {
    iss: &'a str,
    iat: i64,
}

/// A cached, signed token plus the `iat` it was signed with.
#[derive(Clone)]
struct Cached {
    token: Arc<str>,
    iat: i64,
}

/// Signs and caches APNs provider tokens.
pub struct JwtSigner {
    key: EncodingKey,
    header: Header,
    team_id: String,
    /// Guarded so concurrent pushes cannot stampede a re-sign: the first waiter
    /// re-signs, the rest observe the fresh token. Signing is a sub-millisecond
    /// CPU operation with no `.await` inside, so a `std::sync::Mutex` is the
    /// right lock here — an async mutex would buy nothing.
    cached: Mutex<Option<Cached>>,
}

/// Hand-written so neither the key nor a signed token can reach a log through
/// a stray `{:?}`.
impl std::fmt::Debug for JwtSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtSigner")
            .field("kid", &self.header.kid)
            .field("iss", &self.team_id)
            .finish_non_exhaustive()
    }
}

impl JwtSigner {
    /// Build a signer from a `.p8` PKCS#8 PEM.
    pub fn new(pem: &str, key_id: &str, team_id: &str) -> Result<Self, JwtError> {
        let key =
            EncodingKey::from_ec_pem(pem.as_bytes()).map_err(|e| JwtError::Key(e.to_string()))?;
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(key_id.to_string());
        Ok(Self {
            key,
            header,
            team_id: team_id.to_string(),
            cached: Mutex::new(None),
        })
    }

    /// The current provider token, re-signing if the cached one has aged out.
    pub fn token(&self) -> Result<Arc<str>, JwtError> {
        self.token_at(now_unix())
    }

    /// [`JwtSigner::token`] with an injected clock. Exposed so the age-based
    /// re-sign can be tested without waiting fifty minutes.
    pub fn token_at(&self, now: i64) -> Result<Arc<str>, JwtError> {
        // Poisoning is RECOVERED, not propagated: the guarded value is a cached
        // token with no invariant a panic could corrupt, and propagating would
        // let one panic under this lock fail every future push forever while the
        // liveness probe still reported a healthy process.
        let mut slot = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_ref()
            && now.saturating_sub(c.iat) <= MAX_TOKEN_AGE_SECS
            && now >= c.iat
        {
            return Ok(c.token.clone());
        }
        let claims = Claims {
            iss: &self.team_id,
            iat: now,
        };
        let signed =
            encode(&self.header, &claims, &self.key).map_err(|e| JwtError::Sign(e.to_string()))?;
        let token: Arc<str> = Arc::from(signed.as_str());
        *slot = Some(Cached {
            token: token.clone(),
            iat: now,
        });
        Ok(token)
    }
}

/// Seconds since the epoch. A pre-1970 clock is not a case worth modelling.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixture_test_key.p8");
    const FIXTURE_PUB: &str = include_str!("../tests/fixture_test_key.pub.pem");

    fn signer() -> JwtSigner {
        JwtSigner::new(FIXTURE, "KEYID12345", "TEAMID6789").unwrap()
    }

    #[test]
    fn rejects_a_non_pem_key() {
        let err = JwtSigner::new("not a pem", "K", "T").unwrap_err();
        assert!(matches!(err, JwtError::Key(_)));
    }

    #[test]
    fn signs_with_es256_and_the_configured_kid() {
        let s = signer();
        let tok = s.token_at(1_700_000_000).unwrap();
        let header = jsonwebtoken::decode_header(tok.as_ref()).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some("KEYID12345"));
    }

    #[test]
    fn claims_are_issuer_and_iat_only() {
        let s = signer();
        let tok = s.token_at(1_700_000_000).unwrap();
        let claims = decode_claims(&tok);
        assert_eq!(claims["iss"], "TEAMID6789");
        assert_eq!(claims["iat"], 1_700_000_000i64);
        // No exp: APNs ages tokens by iat, and an exp we got wrong would be a
        // hard rejection.
        assert!(claims.get("exp").is_none());
        assert_eq!(claims.as_object().unwrap().len(), 2);
    }

    #[test]
    fn caches_until_fifty_minutes_then_resigns() {
        let s = signer();
        let t0 = 1_700_000_000;
        let a = s.token_at(t0).unwrap();
        // Inside the window: same token, no re-sign.
        let b = s.token_at(t0 + MAX_TOKEN_AGE_SECS).unwrap();
        assert_eq!(a.as_ref(), b.as_ref());
        assert_eq!(decode_claims(&b)["iat"], t0);

        // One second past: fresh signature with the new iat.
        let c = s.token_at(t0 + MAX_TOKEN_AGE_SECS + 1).unwrap();
        assert_ne!(a.as_ref(), c.as_ref());
        assert_eq!(decode_claims(&c)["iat"], t0 + MAX_TOKEN_AGE_SECS + 1);
    }

    #[test]
    fn resigns_when_the_clock_moves_backwards() {
        let s = signer();
        let t0 = 1_700_000_000;
        let a = s.token_at(t0).unwrap();
        // A token dated in the future is rejected by APNs, so a backwards clock
        // step must invalidate the cache rather than serve it.
        let b = s.token_at(t0 - 60).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
        assert_eq!(decode_claims(&b)["iat"], t0 - 60);
    }

    #[test]
    fn verifies_against_the_fixture_public_key() {
        let s = signer();
        let tok = s.token_at(now_unix()).unwrap();
        let mut validation = jsonwebtoken::Validation::new(Algorithm::ES256);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation.validate_aud = false;
        validation.set_issuer(&["TEAMID6789"]);
        let key = jsonwebtoken::DecodingKey::from_ec_pem(FIXTURE_PUB.as_bytes()).unwrap();
        let data =
            jsonwebtoken::decode::<serde_json::Value>(tok.as_ref(), &key, &validation).unwrap();
        assert_eq!(data.claims["iss"], "TEAMID6789");
    }

    /// Decode the payload segment without verifying — the signature is checked
    /// in its own test.
    fn decode_claims(token: &str) -> serde_json::Value {
        let payload = token.split('.').nth(1).expect("jwt has three segments");
        serde_json::from_slice(&b64url_decode(payload)).unwrap()
    }

    /// Minimal unpadded base64url decoder for the claims assertions.
    fn b64url_decode(s: &str) -> Vec<u8> {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits = 0;
        for ch in s.bytes() {
            let v = T.iter().position(|&c| c == ch).expect("base64url alphabet") as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }
}
