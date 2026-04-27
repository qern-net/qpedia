//! Weaviate client wrapper: WikiPage / Chunk / Source / Folder classes.
//! See DESIGN.md §2.4.

use async_trait::async_trait;
use qpedia_core::{wiki::WikiPage, PageId, Result};

#[async_trait]
pub trait WikiIndex: Send + Sync {
    async fn upsert_page(&self, page: &WikiPage, vector: Vec<f32>) -> Result<()>;
    async fn delete_page(&self, id: &PageId) -> Result<()>;
    async fn hybrid_search(&self, query: &str, vector: &[f32], k: usize) -> Result<Vec<PageId>>;
    async fn follow_links(&self, from: &PageId) -> Result<Vec<PageId>>;
}

pub struct WeaviateStore {
    // http: reqwest::Client,  // wired in week 3
    // base_url: url::Url,
}

impl WeaviateStore {
    pub async fn connect(_url: &str) -> Result<Self> {
        Ok(Self {})
    }
}
