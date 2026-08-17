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

pub use fastembed_impl::FastEmbedder;

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
    /// fastembed model identifier string (e.g. "bge-small-en-v1.5").
    pub model_name: String,
    /// Expected output dimension; must match the vec0 table declaration.
    pub dims: usize,
    /// Directory the ONNX weights download to on first run.
    pub cache_dir: PathBuf,
}

/// Flatten a message into the canonical text used at both ingest and query time.
pub fn message_embed_text(subject: &str, body: &str, max_chars: usize) -> String {
    let mut s = String::with_capacity(subject.len() + body.len() + 2);
    s.push_str(subject.trim());
    if !body.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(body.trim());
    }
    truncate_chars(&s, max_chars)
}

/// Default number of characters of `subject + body` fed to the embedder.
pub const DEFAULT_EMBED_MAX_CHARS: usize = 2000;

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
