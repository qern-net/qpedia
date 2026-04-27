//! Embedding generation. Local via fastembed-rs (bge-m3) by default.
//! See DESIGN.md §4.

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Embedder: Send + Sync {
    fn name(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

pub struct BgeM3 {
    // inner: fastembed::TextEmbedding,  // wired in week 3
}

impl BgeM3 {
    pub fn load(_cache_dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(Self {})
    }
}

#[async_trait]
impl Embedder for BgeM3 {
    fn name(&self) -> &str { "bge-m3" }
    fn dimensions(&self) -> usize { 1024 }
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(Vec::new())
    }
}
