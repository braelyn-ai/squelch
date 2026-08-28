//! On-box semantic embeddings for recall ("what did I say I'd send X").
//!
//! SECURITY INVARIANT: sealed (auth/2FA) content is NEVER embedded. The gate is
//! the store's embed-at-write path (`sensitivity == Normal`), not this module —
//! sealed text is structurally absent from the vector space. See docs/SECURITY.md.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{CoreError, Result};
use crate::text::truncate_chars;

mod fastembed_impl;
mod lazy;

pub use fastembed_impl::{DEFAULT_MODEL_CODE, FastEmbedder};
pub use lazy::{LazyEmbedder, ReapOutcome};

/// Turns text into a fixed-dimension embedding vector. CPU-bound; callers run it
/// under `spawn_blocking` so ingest never stalls on it. `dims()` MUST match the
/// vec0 `message_vecs` `float[N]` declaration in `store/schema.sql`.
pub trait Embedder: Send + Sync {
    /// Embed a single piece of text into a `dims()`-length `f32` vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch; the default fans out to [`Embedder::embed`].
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// The embedding dimensionality (e.g. 384 for BGE-small-en-v1.5).
    fn dims(&self) -> usize;
}

/// Resolved embedding config: which model, how wide, and where weights cache.
#[derive(Debug, Clone)]
pub struct EmbedSettings {
    /// fastembed model identifier string (e.g. "Xenova/bge-small-en-v1.5").
    pub model_name: String,
    /// Expected output dimension; must match the vec0 table declaration.
    pub dims: usize,
    /// Directory the ONNX weights download to on first run.
    pub cache_dir: PathBuf,
    /// Tokens the model reads per text; longer input is truncated at the
    /// tokenizer. Sets fastembed's `max_length` (its default is 512). Build
    /// this through [`crate::config::EmbedConfig::settings`], which clamps the
    /// value: a small `max_tokens` is not "unlimited", it is broken (see there).
    pub max_tokens: usize,
}

/// Flatten a message into the canonical text embedded AT INGEST (and by the
/// backfill, which reuses this). Query text does NOT come through here:
/// `semantic_search` and `hybrid_search` embed the raw query, so `max_chars`
/// never applies to it and the tokenizer cut is all a long query gets.
pub fn message_embed_text(subject: &str, body: &str, max_chars: usize) -> String {
    let mut s = String::with_capacity(subject.len() + body.len() + 2);
    s.push_str(subject.trim());
    if !body.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(body.trim());
    }
    truncate_chars(&s, max_chars)
}

/// Default number of characters of `subject + body` fed to the embedder at
/// ingest. Paired with [`DEFAULT_EMBED_MAX_TOKENS`]: roughly tokens x 4 for
/// English, so the character cut and the tokenizer cut land in about the same
/// place. The pairing is an ingest-only story; queries take no character cut.
pub const DEFAULT_EMBED_MAX_CHARS: usize = 1000;

/// Default token budget per text (fastembed `max_length`). 256, not the model's
/// 512 ceiling: attention scratch is quadratic in sequence length and a batch
/// pads to its longest member, so at a `backfill_batch` of 8 an fp32 pass of
/// 512-token texts adds +324 MB against +123 MB at 256 (at batch 1, +44 MB
/// against +13 MB); at a larger batch the same ratio applies to a far larger
/// base. Recall lives in the subject and the first ~1000 characters; long
/// newsletters lose their tails.
///
/// Lowering this leaves the corpus permanently MIXED. There is no re-embed
/// path: `messages_missing_vectors` only finds messages with NO vector, so mail
/// embedded at 512 keeps its old vector until `message_vecs` is reset. The
/// model is CLS-pooled, so the skew between the two reads is modest; it is
/// accepted and unmeasured, pending a `squelch-eval` recall pass.
pub const DEFAULT_EMBED_MAX_TOKENS: usize = 256;

/// Floor [`crate::config::EmbedConfig::settings`] clamps `max_tokens` up to.
/// Keeps 0, 1 and 2 out of the tokenizer, where they do the opposite of what a
/// small budget looks like it should do; the clamp's own doc has the mechanism.
pub const EMBED_MAX_TOKENS_FLOOR: usize = 8;

/// Ceiling [`crate::config::EmbedConfig::settings`] clamps `max_tokens` down
/// to: the pinned model's position budget, past which there is nothing to read.
pub const EMBED_MAX_TOKENS_CEILING: usize = 512;

/// Deterministic, download-free [`Embedder`] for tests: a bag-of-words hash, so
/// "planted doc ranks above decoy" is reproducible offline. Not for production —
/// no semantics beyond token overlap.
#[derive(Debug)]
pub struct StubEmbedder {
    dims: usize,
    _guard: Mutex<()>,
}

impl StubEmbedder {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            _guard: Mutex::new(()),
        }
    }
}

impl Embedder for StubEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if self.dims == 0 {
            return Err(CoreError::Other(anyhow::anyhow!("stub embedder dims=0")));
        }
        // Hash lowercased tokens into buckets, then L2-normalize: docs sharing
        // tokens end up close, unrelated ones do not.
        let mut v = vec![0.0f32; self.dims];
        for tok in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
        {
            let mut h: u64 = 1469598103934665603; // FNV offset basis
            for b in tok.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            let idx = (h % self.dims as u64) as usize;
            v[idx] += 1.0;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_text_joins_subject_and_body_and_trims() {
        assert_eq!(
            message_embed_text("  Re: invoice ", "\nattached\n", DEFAULT_EMBED_MAX_CHARS),
            "Re: invoice\n\nattached"
        );
        // An empty body contributes no separator.
        assert_eq!(message_embed_text("hi", "   ", 100), "hi");
    }

    #[test]
    fn embed_text_truncates_at_the_default_char_budget() {
        assert_eq!(DEFAULT_EMBED_MAX_CHARS, 1000);
        let body = "x".repeat(5000);
        let out = message_embed_text("subject", &body, DEFAULT_EMBED_MAX_CHARS);
        assert_eq!(out.chars().count(), DEFAULT_EMBED_MAX_CHARS);
        assert!(out.starts_with("subject\n\nxxx"));
        // Under the budget, nothing is cut.
        let short = message_embed_text("subject", "short body", DEFAULT_EMBED_MAX_CHARS);
        assert_eq!(short, "subject\n\nshort body");
    }

    /// The two defaults are a pair: chars ~ tokens x 4, so the char cut and the
    /// tokenizer cut land in about the same place.
    #[test]
    fn char_and_token_budgets_stay_paired() {
        let c = crate::config::EmbedConfig::default();
        assert_eq!(c.max_tokens, DEFAULT_EMBED_MAX_TOKENS);
        assert_eq!(c.max_chars, DEFAULT_EMBED_MAX_CHARS);
        assert!(c.max_chars <= c.max_tokens * 4);
        assert!(c.max_chars >= c.max_tokens * 3);
    }
}
