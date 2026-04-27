//! Ingest pipeline driver: state machine over SourceStatus. See DESIGN.md §5.1.

use anyhow::Result;
use qpedia_core::SourceId;

pub struct IngestPipeline {
    // dependencies wired in week 6
}

impl IngestPipeline {
    pub fn new() -> Self { Self {} }

    pub async fn run_one(&self, _source_id: &SourceId) -> Result<()> {
        // Extract → Classify → AgentDistill → Validate → Commit → Embed
        Ok(())
    }
}

impl Default for IngestPipeline {
    fn default() -> Self { Self::new() }
}
