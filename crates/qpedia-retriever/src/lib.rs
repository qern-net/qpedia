//! Query-time retrieval: hybrid search + bounded graph walk.
//! See DESIGN.md §8.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub user_groups: Vec<String>,
    pub max_pages: usize,
    pub max_depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub markdown: String,
    pub citations: Vec<Citation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Citation {
    pub page_path: String,
    pub source_ids: Vec<String>,
}

pub struct Retriever {
    // dependencies wired in week 8
}

impl Retriever {
    pub fn new() -> Self { Self {} }

    pub async fn answer(&self, _q: Query) -> Result<Answer> {
        Ok(Answer { markdown: String::new(), citations: vec![] })
    }
}

impl Default for Retriever {
    fn default() -> Self { Self::new() }
}
