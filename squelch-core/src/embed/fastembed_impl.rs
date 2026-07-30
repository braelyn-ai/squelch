//! Production [`Embedder`]: fastembed (ONNX Runtime, CPU), weights cached on disk.
//! A not-yet-cached model triggers a one-time download to `settings.cache_dir`;
//! every later run loads locally with no network.

use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::{EmbedSettings, Embedder};
use crate::error::{CoreError, Result};

/// fastembed-backed embedder. `TextEmbedding::embed` takes `&mut self`, hence the
/// `Mutex`; embedding runs under `spawn_blocking`, so contention is a non-issue.
pub struct FastEmbedder {
    model: Mutex<TextEmbedding>,
    dims: usize,
}

impl FastEmbedder {
    /// Construct from resolved [`EmbedSettings`], downloading weights on first use.
    /// Errors on an unknown model name, or a reported dimension that disagrees with
    /// `settings.dims` — such a mismatch would silently corrupt the vec0 table.
    pub fn new(settings: &EmbedSettings) -> Result<Self> {
        let (model, dim) = resolve_model(&settings.model_name)?;
        if dim != settings.dims {
            return Err(CoreError::InvalidInput(format!(
                "embedding model '{}' has dim {dim}, but config/schema expects {}",
                settings.model_name, settings.dims
            )));
        }

        // A stable, greppable line so operators know weights are being fetched.
        let already_cached = model_appears_cached(&settings.cache_dir);
        if !already_cached {
            eprintln!(
                "squelch: downloading embedding model '{}' weights to {} (first run only)",
                settings.model_name,
                settings.cache_dir.display()
            );
        }

        let opts = InitOptions::new(model)
            .with_cache_dir(settings.cache_dir.clone())
            .with_show_download_progress(!already_cached);

        let embedding = TextEmbedding::try_new(opts)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed init: {e}")))?;

        Ok(Self {
            model: Mutex::new(embedding),
            dims: settings.dims,
        })
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| CoreError::Other(anyhow::anyhow!("fastembed returned no vector")))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut model = self
            .model
            .lock()
            .map_err(|_| CoreError::Other(anyhow::anyhow!("embedder mutex poisoned")))?;
        let vecs = model
            .embed(texts, None)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed embed: {e}")))?;
        Ok(vecs)
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

/// Map a config model-name string to a fastembed [`EmbeddingModel`] + its
/// dimension. Accepts the canonical fastembed `model_code`
/// ("Qdrant/bge-small-en-v1.5") or a short alias ("bge-small-en-v1.5").
fn resolve_model(name: &str) -> Result<(EmbeddingModel, usize)> {
    let want = name.trim().to_lowercase();
    // Friendly aliases -> the fastembed model_code substring to match on.
    let alias = match want.as_str() {
        "bge-small-en-v1.5" | "bgesmallenv15" | "bge_small_en_v15" => "bge-small-en-v1.5",
        other => other,
    };
    for info in TextEmbedding::list_supported_models() {
        let code = info.model_code.to_lowercase();
        if code == want || code.ends_with(alias) || code.contains(alias) {
            return Ok((info.model, info.dim));
        }
    }
    Err(CoreError::InvalidInput(format!(
        "unknown embedding model '{name}' (see fastembed supported models)"
    )))
}

/// Best-effort check for weights already in the cache dir, so the first-download
/// notice fires only on a real download. Unknowable -> assume not cached (worst
/// case: one extra notice line).
fn model_appears_cached(cache_dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return false;
    };
    // fastembed lays weights out in per-model subdirs; any non-empty one counts.
    for entry in entries.flatten() {
        if entry.path().is_dir()
            && std::fs::read_dir(entry.path())
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}
