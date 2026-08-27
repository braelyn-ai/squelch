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
        let (embedding, dims) = load_session(settings)?;
        Ok(Self {
            model: Mutex::new(embedding),
            dims,
        })
    }
}

/// Resolve `settings` to a model, check its dimension against `settings.dims`,
/// and load the ONNX session, downloading weights on first run. THE one place a
/// session is built, shared by [`FastEmbedder`] (loads once, holds it for the
/// life of the process) and [`super::LazyEmbedder`] (loads, unloads when idle,
/// loads again), so both validate and log the same way. Returns the session and
/// the now-confirmed dimension.
fn load_session(settings: &EmbedSettings) -> Result<(TextEmbedding, usize)> {
    let ResolvedModel {
        model,
        code,
        dim,
        file,
    } = resolve_model(&settings.model_name)?;
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
    let already_cached = model_appears_cached(&settings.cache_dir, &code, &file);
    if !already_cached {
        eprintln!(
            "squelch: downloading embedding model '{code}' weights to {} (first run only)",
            settings.cache_dir.display()
        );
    }

    let opts = InitOptions::new(model)
        .with_cache_dir(settings.cache_dir.clone())
        .with_show_download_progress(!already_cached)
        .with_max_length(settings.max_tokens);

    let embedding = TextEmbedding::try_new(opts)
        .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed init: {e}")))?;

    // Printed AFTER init succeeds, so the line means "these weights are
    // resident" rather than "these were asked for". This is the only place
    // a pod's logs say which of fastembed's same-named builds it is running,
    // and its absence is why a fleet running two different models went
    // unnoticed until someone went looking at RSS. See [`resolve_model`].
    // Once per process, not per load: LazyEmbedder rebuilds the session after
    // an idle unload and logs that reload itself, so the model line here would
    // otherwise repeat every ten minutes and drown the one answer it exists to
    // give (which of fastembed's same-named builds this pod runs).
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    ANNOUNCED.call_once(|| eprintln!("squelch: embedding model {code} ({dim}-dim) loaded"));

    Ok((embedding, dim))
}

/// [`load_session`] behind [`super::lazy::EmbedSession`], which is the shape
/// [`super::LazyEmbedder`] loads through. Boxing here rather than there keeps
/// every fastembed type inside this file: the lazy wrapper is pure lifecycle
/// (load, stamp, drop) and can be tested against a fake session with no weights
/// on disk.
pub(super) fn load_boxed_session(
    settings: &EmbedSettings,
) -> Result<Box<dyn super::lazy::EmbedSession>> {
    let (session, _dims) = load_session(settings)?;
    Ok(Box::new(session))
}

/// The loaded half of [`super::LazyEmbedder`]. `TextEmbedding::embed` takes
/// `&mut self`, which is why the trait does too and why every caller reaches it
/// through a mutex.
impl super::lazy::EmbedSession for TextEmbedding {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts, None)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed embed: {e}")))
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
/// alias, and the logs want the real identity), the dimension fastembed reports
/// for it, and the `model_file` inside that repo, which is what
/// [`model_appears_cached`] asks the cache about.
#[derive(Debug)]
struct ResolvedModel {
    model: EmbeddingModel,
    code: String,
    dim: usize,
    file: String,
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
///    one entry contains it. Two or more is an error listing the candidates (up
///    to eight of them), so the operator can copy a usable spelling back into
///    config.
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
/// The two are not interchangeable. MEASURED on Linux glibc, over a corpus of
/// 256 texts at the model's own 512-token `max_length`: fp32 costs +195 MB to
/// load and another +44 MB of scratch while embedding at batch 1; int8 costs
/// +100 MB to load and another +196 MB. At batch 8 the scratch is +324 MB
/// against +363 MB. That is the measurement and not an explanation: fastembed
/// marks `BGESmallENV15Q` as `QuantizationMode::Static`
/// (`fastembed/src/text_embedding/impl.rs`), so nothing is dequantizing weights
/// per call and the mechanism stays unpinned, most likely the int8 kernels'
/// own per-run scratch.
///
/// READ THOSE NUMBERS HONESTLY, because a memory pass that reads them the other
/// way will unpin this. fp32 wins on the path the daemon actually spends its
/// life on: ingest embeds one message at a time and a search embeds one query,
/// and at batch 1 the scratch is +44 MB against +196 MB. At batch 8, which only
/// the vector backfill reaches, the two are a wash and int8's total is in fact
/// the smaller (100 + 363 against 195 + 324). And now that the session unloads
/// while idle ([`super::LazyEmbedder`]), what a resting pod holds is the LOAD
/// cost, where int8 is smaller by 95 MB. So the pin is not a memory win. fp32
/// is the pin because it is the better weights and because a pin is
/// deterministic, which is the half that was actually broken.
///
/// A pod that ran int8 needs NO RE-EMBED after the pin. Measured here with both
/// builds cached, 8 mail-like texts at max_length 256: same-text cosine between
/// the two builds is 1.0000 mean and 1.0000 min, nearest-neighbour agreement is
/// 8 of 8, and a corpus holding half its vectors from each build still ranks
/// the right document first for a query. Whatever is already in `message_vecs`
/// stays where it is.
///
/// Ambiguity is an error rather than a preference because the same trap sits
/// behind exact codes too: 23 of fastembed 5.17.2's 46 entries sit under a
/// `model_code` that names more than one build. Eleven codes are shared that
/// way, by `Xenova/all-MiniLM-L12-v2`, `nomic-ai/nomic-embed-text-v1.5`,
/// `mixedbread-ai/mxbai-embed-large-v1`, both `Alibaba-NLP/gte-*-en-v1.5`,
/// `onnx-community/embeddinggemma-300m-ONNX` (three builds) and the five
/// `snowflake-arctic-embed-*` families, whose members differ only in
/// `model_file`. Any rule that silently picks one of those is the same coin
/// flip wearing a straighter face; the variant name is how you say which.
///
/// Which advice a tie gets therefore turns on whether the CANDIDATES share a
/// `model_code`, not on whether the name that was typed was an exact code.
/// `all-MiniLM-L12-v2` is a substring match whose two hits both sit under
/// `Xenova/all-MiniLM-L12-v2`, so telling that operator to name one by its full
/// model_code walks them straight into the other error.
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
        let (code, dim, file) = {
            let info = TextEmbedding::get_model_info(&model)
                .map_err(|e| CoreError::Other(anyhow::anyhow!("fastembed model info: {e}")))?;
            (info.model_code.clone(), info.dim, info.model_file.clone())
        };
        return Ok(ResolvedModel {
            model,
            code,
            dim,
            file,
        });
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
    let mut hits: Vec<_> = models
        .iter()
        .filter(|i| i.model_code.to_lowercase() == want_code)
        .collect();
    if hits.is_empty() {
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
            file: info.model_file.clone(),
        }),
        // Two shapes of tie, and they want opposite advice. When every hit sits
        // under ONE model_code, no code can pick between them, so that message
        // asks for a variant name and lists nothing else. Only a tie that spans
        // several codes is fixable by naming one exactly. Note which way round
        // that is: it is a fact about the candidates, not about whether the
        // operator happened to type a whole code.
        many => {
            let one_code = many
                .iter()
                .all(|i| i.model_code.eq_ignore_ascii_case(&many[0].model_code));
            let listed = candidate_list(many, one_code);
            Err(CoreError::InvalidInput(if one_code {
                format!(
                    "embedding model '{name}' is ambiguous: it resolves to model_code \
                     {}, which names {} fastembed builds; a full model_code cannot \
                     pick between them, so name the one you want by its variant \
                     name: {listed}",
                    many[0].model_code,
                    many.len()
                )
            } else {
                format!(
                    "embedding model '{name}' is ambiguous: it is a substring of {} \
                     model_codes; name one exactly, by full model_code or by \
                     fastembed variant name: {listed}",
                    many.len()
                )
            }))
        }
    }
}

/// The candidates an ambiguity error ends with. Variant names alone when every
/// hit shares one `model_code`, since the code is not the answer there;
/// `code (Variant)` when the tie spans several codes, where either half is a
/// usable spelling. Capped, because a two-character substring matches most of
/// the table and an error nobody finishes reading is one nobody acts on.
fn candidate_list(hits: &[&fastembed::ModelInfo<EmbeddingModel>], one_code: bool) -> String {
    const MAX: usize = 8;
    let listed = hits
        .iter()
        .take(MAX)
        .map(|i| {
            if one_code {
                i.model.to_string()
            } else {
                format!("{} ({})", i.model_code, i.model)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    match hits.len().checked_sub(MAX) {
        Some(rest) if rest > 0 => format!("{listed}, and {rest} more"),
        _ => listed,
    }
}

/// Best-effort check for THIS model's weights already in the cache dir, so the
/// first-download notice fires only on a real download. Unknowable -> assume not
/// cached (worst case: one extra notice line).
///
/// Per-model, not per-directory: a volume that has only ever run the int8 build
/// holds a populated `models--Qdrant--bge-small-en-v1.5-onnx-Q`, and a check
/// that any non-empty subdir counts calls that cached and then silently pulls
/// 126 MB of fp32.
///
/// It asks hf-hub rather than reading the layout itself, because "the model's
/// directory has something in it" is ALSO true of a download that never
/// finished: hf-hub `create_dir_all`s the blob's parent and takes a `.lock`
/// beside it before a single byte arrives, so an interrupted first run leaves a
/// non-empty `blobs/` behind and the retry — the run that really does pull 126
/// MB — prints nothing. `Cache::get` answers on the snapshot pointer instead,
/// which exists only after `refs/main` is written, and it is the same lookup
/// `TextEmbedding::try_new` itself makes. `code` is the RESOLVED `model_code`
/// and `model_file` the file inside that repo fastembed loads; an empty
/// `model_file` means nobody could tell us what to ask about, and only then
/// does the directory check stand in.
fn model_appears_cached(cache_dir: &std::path::Path, code: &str, model_file: &str) -> bool {
    if !model_file.is_empty() {
        return hf_hub::Cache::new(cache_dir.to_path_buf())
            .model(code.to_string())
            .get(model_file)
            .is_some();
    }
    let dir = cache_dir.join(format!("models--{}", code.replace('/', "--")));
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Every test here is a lookup against fastembed's in-process model table, or
/// against a temp dir standing in for a weights cache. Nothing touches the
/// network and nothing reads the real cache, so no test downloads weights.
#[cfg(test)]
mod tests {
    use super::*;

    /// The regression the whole change exists for. Repeating the call would
    /// prove nothing: a HashMap's order is fixed for the life of a process, so
    /// no in-process loop can reproduce a cross-boot coin flip. What makes the
    /// answer stable is that only `bgesmallenv15` is short-circuited by
    /// fastembed's `FromStr`; the other two walk the list, and the walk is
    /// exhaustive and equality-based, so the order it walks in cannot matter.
    #[test]
    fn default_aliases_resolve_to_fp32() {
        for alias in ["bge-small-en-v1.5", "bgesmallenv15", "bge_small_en_v15"] {
            let r = resolve_model(alias).unwrap();
            assert_eq!(r.model, EmbeddingModel::BGESmallENV15, "alias {alias}");
            assert_eq!(r.code, DEFAULT_MODEL_CODE, "alias {alias}");
            assert_eq!(r.dim, 384, "alias {alias}");
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
        assert!(err.contains("is a substring of 2 model_codes"), "{err}");
        // Distinct codes, so a full code IS the fix here. Contrast with
        // `a_substring_tie_under_one_code_asks_for_a_variant_name`.
        assert!(
            err.contains("name one exactly, by full model_code"),
            "{err}"
        );
        assert!(
            err.contains("Xenova/bge-base-en-v1.5 (BGEBaseENV15)"),
            "{err}"
        );
        assert!(
            err.contains("Qdrant/bge-base-en-v1.5-onnx-Q (BGEBaseENV15Q)"),
            "{err}"
        );
    }

    /// A substring wide enough to hit half the table must not answer with half
    /// the table: the list caps at eight and says how many it left out. `bge`
    /// matches nine entries, so it is one over the line.
    #[test]
    fn a_wide_substring_tie_caps_its_candidate_list() {
        let err = resolve_model("bge").unwrap_err().to_string();
        assert!(err.contains("is a substring of 9 model_codes"), "{err}");
        // One `code (Variant)` pair per listed candidate, and no tenth.
        assert_eq!(err.matches(" (").count(), 8, "{err}");
        assert!(err.ends_with(", and 1 more"), "{err}");
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
    ///
    /// The ADVICE is the point here. Telling an operator to name the model by
    /// its full model_code is useless when the full model_code is what they
    /// typed, so this tie asks for a variant name and lists nothing but variant
    /// names.
    #[test]
    fn duplicated_exact_code_errors_and_variant_name_disambiguates() {
        let err = resolve_model("Xenova/all-MiniLM-L12-v2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("which names 2 fastembed builds"), "{err}");
        assert!(err.ends_with("AllMiniLML12V2, AllMiniLML12V2Q"), "{err}");
        assert!(!err.contains("name one exactly"), "{err}");

        let r = resolve_model("AllMiniLML12V2Q").unwrap();
        assert_eq!(r.model, EmbeddingModel::AllMiniLML12V2Q);
        assert_eq!(r.code, "Xenova/all-MiniLM-L12-v2");
    }

    /// The half the first version got backwards. `all-MiniLM-L12-v2` is a
    /// SUBSTRING match, so the old code handed it the substring advice — "name
    /// one exactly, by full model_code" — and both hits carry the same full
    /// model_code, so an operator who followed it landed on the other error.
    /// What decides the advice is that the candidates share a code, not that
    /// the name was typed out in full.
    #[test]
    fn a_substring_tie_under_one_code_asks_for_a_variant_name() {
        let err = resolve_model("all-MiniLM-L12-v2").unwrap_err().to_string();
        assert!(
            err.contains("it resolves to model_code Xenova/all-MiniLM-L12-v2"),
            "{err}"
        );
        assert!(
            err.contains("a full model_code cannot pick between them"),
            "{err}"
        );
        assert!(!err.contains("name one exactly"), "{err}");
        // Variant names alone, since the code is not a usable spelling here.
        assert!(err.ends_with("AllMiniLML12V2, AllMiniLML12V2Q"), "{err}");
    }

    /// Lay a repo out the way hf-hub does: `refs/main` holds a commit hash and
    /// `snapshots/<hash>/<file>` is the pointer `Cache::get` resolves. Real ones
    /// are symlinks into `blobs/`; a plain file resolves identically and keeps
    /// the test off symlink semantics.
    fn seed_snapshot(cache_dir: &std::path::Path, code: &str, file: &str) {
        let repo = cache_dir.join(format!("models--{}", code.replace('/', "--")));
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs").join("main"), b"c0ffee").unwrap();
        let pointer = repo.join("snapshots").join("c0ffee").join(file);
        std::fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        std::fs::write(pointer, b"x").unwrap();
    }

    /// The cached-check is per MODEL, and it asks hf-hub rather than reading the
    /// directory listing. A volume that has only ever run the int8 build is NOT
    /// cached for fp32, and neither is one holding an interrupted download —
    /// which is the case the listing got wrong, because hf-hub creates `blobs/`
    /// and takes a `.lock` in it before the first byte lands, so the retry that
    /// really did pull 126 MB printed nothing.
    #[test]
    fn cached_check_asks_hf_hub_for_the_resolved_models_own_file() {
        const FILE: &str = "onnx/model.onnx";
        let dir = std::env::temp_dir().join(format!("squelch-embed-cache-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing there at all.
        assert!(!model_appears_cached(&dir, DEFAULT_MODEL_CODE, FILE));

        // The int8 build, fully cached. Still not the fp32 build.
        let int8 = "Qdrant/bge-small-en-v1.5-onnx-Q";
        seed_snapshot(&dir, int8, FILE);
        assert!(!model_appears_cached(&dir, DEFAULT_MODEL_CODE, FILE));
        assert!(model_appears_cached(&dir, int8, FILE));

        // An interrupted download: `blobs/` exists and holds the lock file, and
        // no snapshot pointer was ever written.
        let fp32 = dir.join(format!("models--{}", DEFAULT_MODEL_CODE.replace('/', "--")));
        std::fs::create_dir_all(fp32.join("blobs")).unwrap();
        std::fs::write(fp32.join("blobs").join("deadbeef.lock"), b"").unwrap();
        assert!(!model_appears_cached(&dir, DEFAULT_MODEL_CODE, FILE));
        // And this is the fallback that used to be the whole check, still
        // calling that same wreckage cached. It stands in only when nobody can
        // say which file to ask about.
        assert!(model_appears_cached(&dir, DEFAULT_MODEL_CODE, ""));

        // The pointer written, and resolving.
        seed_snapshot(&dir, DEFAULT_MODEL_CODE, FILE);
        assert!(model_appears_cached(&dir, DEFAULT_MODEL_CODE, FILE));
        // A pointer for some other file in the same repo is not this model's.
        assert!(!model_appears_cached(
            &dir,
            DEFAULT_MODEL_CODE,
            "onnx/model_quantized.onnx"
        ));

        // A cache dir that does not exist reads as not cached, never a panic.
        std::fs::remove_dir_all(&dir).ok();
        assert!(!model_appears_cached(&dir, DEFAULT_MODEL_CODE, FILE));
    }

    /// The `model_file` the cached-check asks about has to be the one fastembed
    /// will load, so `resolve_model` carries it out of the same `ModelInfo` the
    /// code and dim come from.
    #[test]
    fn resolve_carries_the_models_file() {
        assert_eq!(
            resolve_model(DEFAULT_MODEL_CODE).unwrap().file,
            "onnx/model.onnx"
        );
    }
}
