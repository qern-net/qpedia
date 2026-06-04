//! Embedding generation. Local via fastembed-rs (ONNX Runtime).
//! See DESIGN.md §4.
//!
//! The model is downloaded from HuggingFace on first use into
//! `cache_dir`. Subsequent runs load from cache. ~130MB for bge-small,
//! ~430MB for bge-base.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::info;

mod rerank;
pub use rerank::{reranker_from_env, FastEmbedReranker, Reranker, DEFAULT_RERANKER};

#[async_trait]
pub trait Embedder: Send + Sync {
    fn name(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

pub const DEFAULT_MODEL: &str = "bge-small-en-v1.5";

pub struct FastEmbedder {
    cache_dir: PathBuf,
    model_name: String,
    inner: OnceCell<Arc<TextEmbedding>>,
}

impl FastEmbedder {
    pub fn new(cache_dir: impl Into<PathBuf>, model_name: impl Into<String>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            model_name: model_name.into(),
            inner: OnceCell::new(),
        }
    }

    fn pick_model(name: &str) -> Result<EmbeddingModel> {
        match name {
            "bge-small-en-v1.5" | "bge-small" => Ok(EmbeddingModel::BGESmallENV15),
            "bge-base-en-v1.5"  | "bge-base"  => Ok(EmbeddingModel::BGEBaseENV15),
            other => Err(anyhow!("unsupported embed model: {other}")),
        }
    }

    async fn ensure(&self) -> Result<Arc<TextEmbedding>> {
        let arc = self
            .inner
            .get_or_try_init(|| async {
                let cache = self.cache_dir.clone();
                let name = self.model_name.clone();
                std::fs::create_dir_all(&cache)?;
                let model = Self::pick_model(&name)?;
                info!(model = %name, cache = %cache.display(), "loading embedder");
                let te = tokio::task::spawn_blocking(move || {
                    let opts = InitOptions::new(model).with_cache_dir(cache);
                    TextEmbedding::try_new(opts)
                })
                .await
                .map_err(|e| anyhow!("embed init join: {e}"))?
                .map_err(|e| anyhow!("embed init: {e}"))?;
                Ok::<Arc<TextEmbedding>, anyhow::Error>(Arc::new(te))
            })
            .await?;
        Ok(arc.clone())
    }
}

#[async_trait]
impl Embedder for FastEmbedder {
    fn name(&self) -> &str { &self.model_name }
    fn dimensions(&self) -> usize {
        match self.model_name.as_str() {
            "bge-small-en-v1.5" | "bge-small" => 384,
            "bge-base-en-v1.5"  | "bge-base"  => 768,
            _ => 384,
        }
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.ensure().await?;
        let owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();
        tokio::task::spawn_blocking(move || model.embed(owned, None))
            .await
            .map_err(|e| anyhow!("embed join: {e}"))?
            .map_err(|e| anyhow!("embed: {e}"))
    }
}

/// Build an Embedder from env. Returns None on init failure (model can't
/// download, no disk space, etc.) so the app degrades gracefully.
///
/// Env:
///   QPEDIA_EMBED_MODEL      = bge-small-en-v1.5 (default)
///   QPEDIA_EMBED_CACHE_DIR  = ./data/models or /data/models
pub fn embedder_from_env(default_cache: impl Into<PathBuf>) -> Arc<dyn Embedder> {
    let model = std::env::var("QPEDIA_EMBED_MODEL")
        .unwrap_or_else(|_| DEFAULT_MODEL.into());
    let cache = std::env::var("QPEDIA_EMBED_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_cache.into());
    Arc::new(FastEmbedder::new(cache, model))
}
