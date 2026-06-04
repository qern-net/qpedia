//! Cross-encoder reranking. Local via fastembed-rs (ONNX Runtime), same
//! as the embedder — no sidecar, no extra container.
//!
//! After hybrid search ([`crate`] + pgvector) fuses the dense and lexical
//! candidate lists with RRF, the reranker reorders the top candidates by
//! scoring each `(query, document)` pair *jointly* through a cross-encoder.
//! Unlike the bi-encoder embedder (which encodes query and document
//! independently), this models token-level interaction, which is why it
//! lifts final relevance — especially on hybrid/negation queries where the
//! operationally important distinction is a single token.
//!
//! The model is downloaded from HuggingFace into `cache_dir` on first use
//! (like the embedder), then loaded from cache. `bge-reranker-v2-m3` is
//! ~600 MB and multilingual.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::info;

/// A query-vs-documents reranker. Returns, for each input document, a
/// relevance score; higher is more relevant. Index `i` of the result
/// corresponds to `documents[i]`.
#[async_trait]
pub trait Reranker: Send + Sync {
    fn name(&self) -> &str;
    /// Score each document against the query. Returns one score per input
    /// document, in the **same order** as `documents`.
    async fn scores(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>>;
}

/// Default cross-encoder. A strong **multilingual** reranker that packages
/// cleanly as a single ONNX file (no external-data sidecar download), so it
/// loads reliably in-process via fastembed. Multilingual matches the
/// non-English / RTL content Qpedia ingests.
///
/// `bge-reranker-v2-m3` is also selectable (`QPEDIA_RERANK_MODEL`) and is
/// the strongest open BGE cross-encoder, but its fastembed packaging splits
/// weights into a `model.onnx.data` file that the downloader does not
/// fetch reliably — prefer the default unless you've pre-seeded that cache.
pub const DEFAULT_RERANKER: &str = "jina-reranker-v2-base-multilingual";

pub struct FastEmbedReranker {
    cache_dir: PathBuf,
    model_name: String,
    inner: OnceCell<Arc<TextRerank>>,
}

impl FastEmbedReranker {
    pub fn new(cache_dir: impl Into<PathBuf>, model_name: impl Into<String>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            model_name: model_name.into(),
            inner: OnceCell::new(),
        }
    }

    fn pick_model(name: &str) -> Result<RerankerModel> {
        match name {
            "jina-reranker-v2-base-multilingual" | "jina-reranker-v2" | "jina-v2" => {
                Ok(RerankerModel::JINARerankerV2BaseMultiligual)
            }
            "jina-reranker-v1-turbo-en" | "jina-v1-turbo" => {
                Ok(RerankerModel::JINARerankerV1TurboEn)
            }
            "bge-reranker-v2-m3" | "bge-reranker-v2" | "bge-reranker-m3" => {
                Ok(RerankerModel::BGERerankerV2M3)
            }
            "bge-reranker-base" => Ok(RerankerModel::BGERerankerBase),
            other => Err(anyhow!("unsupported reranker model: {other}")),
        }
    }

    async fn ensure(&self) -> Result<Arc<TextRerank>> {
        let arc = self
            .inner
            .get_or_try_init(|| async {
                let cache = self.cache_dir.clone();
                let name = self.model_name.clone();
                std::fs::create_dir_all(&cache)?;
                let model = Self::pick_model(&name)?;
                info!(model = %name, cache = %cache.display(), "loading reranker");
                let tr = tokio::task::spawn_blocking(move || {
                    let opts = RerankInitOptions::new(model).with_cache_dir(cache);
                    TextRerank::try_new(opts)
                })
                .await
                .map_err(|e| anyhow!("reranker init join: {e}"))?
                .map_err(|e| anyhow!("reranker init: {e}"))?;
                Ok::<Arc<TextRerank>, anyhow::Error>(Arc::new(tr))
            })
            .await?;
        Ok(arc.clone())
    }
}

#[async_trait]
impl Reranker for FastEmbedReranker {
    fn name(&self) -> &str {
        &self.model_name
    }

    async fn scores(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.ensure().await?;
        let q = query.to_string();
        let docs: Vec<String> = documents.iter().map(|s| (*s).to_string()).collect();
        let n = docs.len();
        // fastembed's `rerank` returns results sorted by score desc and
        // carries the original index; we re-scatter into input order so the
        // caller can zip scores back onto its candidates.
        let results = tokio::task::spawn_blocking(move || {
            let doc_refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
            model.rerank(q.as_str(), doc_refs, false, None)
        })
        .await
        .map_err(|e| anyhow!("rerank join: {e}"))?
        .map_err(|e| anyhow!("rerank: {e}"))?;

        let mut scores = vec![f32::MIN; n];
        for r in results {
            if r.index < n {
                scores[r.index] = r.score;
            }
        }
        Ok(scores)
    }
}

/// Build a Reranker from env. The reranker is a **mandatory** stage, so
/// this always returns one (it is not `Option`). Init is lazy — the model
/// downloads on first `scores()` call.
///
/// Env:
///   QPEDIA_RERANK_MODEL     = bge-reranker-v2-m3 (default)
///   QPEDIA_EMBED_CACHE_DIR  = ./data/models or /data/models (shared with the embedder)
pub fn reranker_from_env(default_cache: impl Into<PathBuf>) -> Arc<dyn Reranker> {
    let model =
        std::env::var("QPEDIA_RERANK_MODEL").unwrap_or_else(|_| DEFAULT_RERANKER.into());
    let cache = std::env::var("QPEDIA_EMBED_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_cache.into());
    Arc::new(FastEmbedReranker::new(cache, model))
}
