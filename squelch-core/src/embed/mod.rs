//! On-box semantic embeddings for v1 recall ("what did I say I'd send X").
//!
//! SECURITY INVARIANT: sealed (auth/2FA) content is NEVER embedded. The store's
//! embed-at-write path is gated on `sensitivity == Normal`, so sealed text is
//! structurally absent from the vector space — there is nothing to filter later.
//! This module holds no such gate itself (it just turns text into vectors); the
//! gate lives at every call site in the store.
//!
//! The [`Embedder`] trait is the seam: production uses [`FastEmbedder`] (ONNX,
//! CPU-only, weights cached under the data dir); tests use a tiny deterministic
//! stub so the SQL/gating can be unit-tested without a model download.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{CoreError, Result};

mod fastembed_impl;

pub use fastembed_impl::FastEmbedder;

/// Turns text into a fixed-dimension embedding vector. CPU-bound; callers run it
/// off the async poll loop (`spawn_blocking`) so ingest never stalls on it.
///
/// The dimension MUST match the vec0 table's `float[N]` declaration (see
/// `store/schema.sql`, `message_vecs`). [`Embedder::dims`] exposes it so the
/// store can assert at open time.
pub trait Embedder: Send + Sync {
    /// Embed a single piece of text into a `dims()`-length `f32` vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch. Default fans out to [`Embedder::embed`]; the fastembed impl
    /// overrides this to use a single batched ONNX pass (the backfill hot path).
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// The embedding dimensionality (e.g. 384 for BGE-small-en-v1.5).
    fn dims(&self) -> usize;
}

/// Resolved embedding config: which model, how wide, and where weights cache.
/// Built from [`crate::config::EmbedConfig`] plus the data dir.
#[derive(Debug, Clone)]
pub struct EmbedSettings {
    /// fastembed model identifier string (e.g. "bge-small-en-v1.5").
    pub model_name: String,
    /// Expected output dimension; must match the vec0 table declaration.
    pub dims: usize,
    /// Directory the ONNX weights download to on first run.
    pub cache_dir: PathBuf,
}

/// Flatten a message into the text we embed: subject then body, joined by a
/// blank line, truncated to `max_chars` characters (on a char boundary). This is
/// the single canonical shaping used at BOTH ingest and query embed time so the
/// vector space is consistent.
pub fn message_embed_text(subject: &str, body: &str, max_chars: usize) -> String {
    let mut s = String::with_capacity(subject.len() + body.len() + 2);
    s.push_str(subject.trim());
    if !body.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(body.trim());
    }
    truncate_chars(&s, max_chars)
}

/// Truncate to at most `max_chars` characters without splitting a UTF-8 char.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

/// Default number of characters of `subject + body` fed to the embedder.
pub const DEFAULT_EMBED_MAX_CHARS: usize = 2000;

/// A deterministic, download-free [`Embedder`] for tests. Produces a stable
/// vector from a bag-of-words hash so "planted relevant doc ranks above decoy"
/// is reproducible offline. NOT for production (no semantics beyond token
/// overlap).
#[derive(Debug)]
pub struct StubEmbedder {
    dims: usize,
    // Kept for API symmetry / potential future locking parity with real impls.
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
        // Hash each lowercased token into a bucket and L2-normalize. Documents
        // sharing tokens end up close; unrelated ones do not.
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
