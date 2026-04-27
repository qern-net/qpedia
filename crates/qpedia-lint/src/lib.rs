//! Periodic wiki health passes: orphans, broken links, stale claims,
//! contradictions, duplicates, index drift. See DESIGN.md §9.

use anyhow::Result;

pub struct Linter {
    // dependencies wired in week 10
}

impl Linter {
    pub fn new() -> Self { Self {} }
    pub async fn run(&self) -> Result<LintReport> {
        Ok(LintReport::default())
    }
}

impl Default for Linter {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Default)]
pub struct LintReport {
    pub orphans: Vec<String>,
    pub broken_links: Vec<(String, String)>,
    pub stale: Vec<String>,
    pub duplicates: Vec<(String, String, f32)>,
    pub index_drift: Vec<String>,
}
