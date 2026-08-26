//! Production [`Embedder`]: fastembed (ONNX Runtime, CPU), weights cached on disk.
//! A not-yet-cached model triggers a one-time download to `settings.cache_dir`;
//! every later run loads locally with no network.

use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::{EmbedSettings, Embedder};
use crate::error::{CoreError, Result};

/// The fastembed `model_code` the daemon embeds with unless config says
/// otherwise: BGE-small-en-v1.5, fp32, 384-dim, roughly 126 MB on disk.
///
/// Written out as a full code rather than the family name because fastembed
/// ships this family twice, and naming the family used to pick between the two
/// at random per boot. [`resolve_model`] carries the history and the numbers.
pub const DEFAULT_MODEL_CODE: &str = "Xenova/bge-small-en-v1.5";

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
        let ResolvedModel { model, code, dim } = resolve_model(&settings.model_name)?;
        if dim != settings.dims {
            return Err(CoreError::InvalidInput(format!(
                "embedding model '{}' (resolved to {code}) has dim {dim}, \
                 but config/schema expects {}",
                settings.model_name, settings.dims
            )));
        }

        // A stable, greppable line so operators know weights are being fetched.
        // It names the resolved code, not the configured string: the config may
        // hold an alias, and what an operator wants off the wire is the answer.
        let already_cached = model_appears_cached(&settings.cache_dir);
        if !already_cached {
            eprintln!(
                "squelch: downloading embedding model '{code}' weights to {} (first run only)",
                settings.cache_dir.display()
            );
        }

        let opts = InitOptions::new(model)
            .with_cache_dir(settings.cache_dir.clone())
            .with_show_download_progress(!already_cached);

        let embedding = TextEmbedding::try_new(opts)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed init: {e}")))?;

        // Printed AFTER init succeeds, so the line means "these weights are
        // resident" rather than "these were asked for". This is the only place
        // a pod's logs say which of fastembed's same-named builds it is running,
        // and its absence is why a fleet running two different models went
        // unnoticed until someone went looking at RSS. See [`resolve_model`].
        eprintln!("squelch: embedding model {code} ({dim}-dim) loaded");

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

/// What [`resolve_model`] settles a config string into: the fastembed variant to
/// hand [`InitOptions`], the `model_code` it matched (the config string may be an
/// alias, and the logs want the real identity), and the dimension fastembed
/// reports for it.
#[derive(Debug)]
struct ResolvedModel {
    model: EmbeddingModel,
    code: String,
    dim: usize,
}

/// Map a config model-name string to a fastembed [`EmbeddingModel`],
/// deterministically. Accepted spellings, in order of preference:
///
/// 1. a fastembed variant name (`BGESmallENV15`, case-insensitive), which is
///    unique by construction and the only way to name a build whose
///    `model_code` fastembed shares with a second variant (see below);
/// 2. an exact `model_code` (`Xenova/bge-small-en-v1.5`, case-insensitive), or
///    one of the friendly aliases `bge-small-en-v1.5` / `bgesmallenv15` /
///    `bge_small_en_v15`, which are spellings of [`DEFAULT_MODEL_CODE`] and of
///    nothing else;
/// 3. a substring of a `model_code` (`bge-small-zh`), honoured only when exactly
///    one entry contains it. Two or more is an error naming every candidate, so
///    the operator can copy a full code back into config.
///
/// WHY THE CARE. `TextEmbedding::list_supported_models()` is collected from a
/// `static MODEL_MAP: OnceLock<HashMap<..>>` (fastembed 5.17.2,
/// `src/models/text_embedding.rs`), so the order it comes back in is whatever
/// the process's hash seed decides: different on every boot. The first version
/// of this function walked that list and took the first entry whose code merely
/// CONTAINED the alias, and two entries contain `bge-small-en-v1.5`:
/// `Xenova/bge-small-en-v1.5` (fp32, 126 MB) and `Qdrant/bge-small-en-v1.5-onnx-Q`
/// (int8, 63 MB). Which weights a daemon loaded was therefore a coin flip per
/// process. On 2026-08-26 the hosted fleet was found split two pods to two, with
/// both builds downloaded onto every tenant volume.
///
/// The two are not interchangeable, and the smaller file is the worse deal.
/// Measured on Linux glibc: fp32 costs +195 MB to load and +44 MB of scratch per
/// inference; int8 costs +100 MB to load but +196 MB per inference, because
/// dynamic quantization allocates dequantized working buffers on every call. So
/// the int8 build is both more expensive to run and the lower-quality weights.
/// fp32 is the pin.
///
/// Ambiguity is an error rather than a preference because the same trap sits
/// behind exact codes too: fastembed lists `Xenova/all-MiniLM-L12-v2`,
/// `nomic-ai/nomic-embed-text-v1.5`, the mxbai, gte and snowflake families each
/// under ONE code shared by the fp32 and quantized variants, which differ only
/// in `model_file`. Any rule that silently picks one of those is the same coin
/// flip wearing a straighter face; the variant name is how you say which.
fn resolve_model(name: &str) -> Result<ResolvedModel> {
    let want = name.trim();
    if want.is_empty() {
        return Err(CoreError::InvalidInput(
            "embedding model name is empty (see fastembed supported models)".to_string(),
        ));
    }

    // Form 1. fastembed's own `FromStr` compares against each variant's Debug
    // spelling case-insensitively, so a hit here is unique by construction.
    if let Ok(model) = want.parse::<EmbeddingModel>() {
        // Copy out of the borrowed ModelInfo before `model` moves into the struct.
        let (code, dim) = {
            let info = TextEmbedding::get_model_info(&model)
                .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed model info: {e}")))?;
            (info.model_code.clone(), info.dim)
        };
        return Ok(ResolvedModel { model, code, dim });
    }

    // Form 2. The friendly aliases name ONE exact code. Never a substring to
    // scan for: substring-scanning the family name is the original bug.
    // (`bgesmallenv15` is also a variant name, so it never reaches this arm;
    // it stays listed because the alias set is a promise, not an accident.)
    let want_lower = want.to_lowercase();
    let want_code = match want_lower.as_str() {
        "bge-small-en-v1.5" | "bgesmallenv15" | "bge_small_en_v15" => {
            DEFAULT_MODEL_CODE.to_lowercase()
        }
        _ => want_lower,
    };

    // Collect every match instead of returning the first, so the answer cannot
    // depend on the HashMap's order, and sort so the error text is stable too.
    let models = TextEmbedding::list_supported_models();
    let mut kind = "model_code";
    let mut hits: Vec<_> = models
        .iter()
        .filter(|i| i.model_code.to_lowercase() == want_code)
        .collect();
    if hits.is_empty() {
        kind = "substring";
        hits = models
            .iter()
            .filter(|i| i.model_code.to_lowercase().contains(&want_code))
            .collect();
    }
    hits.sort_by(|a, b| {
        a.model_code
            .cmp(&b.model_code)
            .then_with(|| a.model.to_string().cmp(&b.model.to_string()))
    });

    match hits.as_slice() {
        [] => Err(CoreError::InvalidInput(format!(
            "unknown embedding model '{name}' (see fastembed supported models)"
        ))),
        [info] => Ok(ResolvedModel {
            model: info.model.clone(),
            code: info.model_code.clone(),
            dim: info.dim,
        }),
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|i| format!("{} ({})", i.model_code, i.model))
                .collect();
            Err(CoreError::InvalidInput(format!(
                "embedding model '{name}' is ambiguous: {kind} matches {}; \
                 name one by its full model_code or by its fastembed variant name",
                candidates.join(", ")
            )))
        }
    }
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

/// Every test here is a lookup against fastembed's in-process model table.
/// Nothing touches the network or the cache dir, so no test downloads weights.
#[cfg(test)]
mod tests {
    use super::*;

    /// The regression the whole change exists for. Fifty rounds is partly
    /// theatre, since a HashMap's order is fixed for the life of a process and
    /// one run therefore cannot reproduce a cross-boot coin flip; the real
    /// guard is that the alias is answered before any list walk happens at all.
    /// The loop is here so that a future first-match scan, reintroduced by
    /// someone who did not read the doc comment, still gets a chance to trip.
    #[test]
    fn default_alias_resolves_to_fp32_every_time() {
        for alias in ["bge-small-en-v1.5", "bgesmallenv15", "bge_small_en_v15"] {
            for _ in 0..50 {
                let r = resolve_model(alias).unwrap();
                assert_eq!(r.model, EmbeddingModel::BGESmallENV15, "alias {alias}");
                assert_eq!(r.code, DEFAULT_MODEL_CODE, "alias {alias}");
                assert_eq!(r.dim, 384, "alias {alias}");
            }
        }
    }

    /// The shipped default is a full code, so it takes the exact-code path
    /// rather than the alias table; case and stray whitespace are forgiven.
    #[test]
    fn default_code_resolves_to_fp32() {
        for spelling in [DEFAULT_MODEL_CODE, "  xenova/BGE-Small-EN-v1.5  "] {
            let r = resolve_model(spelling).unwrap();
            assert_eq!(r.model, EmbeddingModel::BGESmallENV15, "{spelling}");
            assert_eq!(r.code, DEFAULT_MODEL_CODE, "{spelling}");
        }
    }

    /// Pinning fp32 must not make the int8 build unreachable: an operator who
    /// asks for it by name still gets it, by either spelling.
    #[test]
    fn quantized_build_is_still_reachable_by_name() {
        for spelling in ["Qdrant/bge-small-en-v1.5-onnx-Q", "bgesmallenv15q"] {
            let r = resolve_model(spelling).unwrap();
            assert_eq!(r.model, EmbeddingModel::BGESmallENV15Q, "{spelling}");
            assert_eq!(r.code, "Qdrant/bge-small-en-v1.5-onnx-Q", "{spelling}");
            assert_eq!(r.dim, 384, "{spelling}");
        }
    }

    #[test]
    fn unknown_name_errors() {
        let err = resolve_model("definitely-not-a-model")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown embedding model"), "{err}");
        // An all-whitespace name must not fall through to the substring path,
        // where the empty string is contained in every code in the table.
        let err = resolve_model("   ").unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// The exact shape of the original bug, one family over: a bare family name
    /// is a substring of both the fp32 and the int8 code. Refuse it, and name
    /// both so the operator can copy a full code straight out of the message.
    #[test]
    fn ambiguous_substring_errors_naming_candidates() {
        let err = resolve_model("bge-base-en-v1.5").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("Xenova/bge-base-en-v1.5"), "{err}");
        assert!(err.contains("Qdrant/bge-base-en-v1.5-onnx-Q"), "{err}");
    }

    /// A substring that picks out one entry is still a convenience worth having.
    #[test]
    fn unique_substring_resolves() {
        let r = resolve_model("bge-small-zh").unwrap();
        assert_eq!(r.model, EmbeddingModel::BGESmallZHV15);
        assert_eq!(r.code, "Xenova/bge-small-zh-v1.5");
    }

    /// fastembed reuses one `model_code` for the fp32 and quantized variants of
    /// several families, so even an exact code can be a coin flip. It must
    /// error, and the variant name must be the way through.
    #[test]
    fn duplicated_exact_code_errors_and_variant_name_disambiguates() {
        let err = resolve_model("Xenova/all-MiniLM-L12-v2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("model_code matches"), "{err}");
        assert!(err.contains("AllMiniLML12V2Q"), "{err}");

        let r = resolve_model("AllMiniLML12V2Q").unwrap();
        assert_eq!(r.model, EmbeddingModel::AllMiniLML12V2Q);
        assert_eq!(r.code, "Xenova/all-MiniLM-L12-v2");
    }
}
