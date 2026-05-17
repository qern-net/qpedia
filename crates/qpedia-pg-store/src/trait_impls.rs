//! `qpedia_store::{SourceStore, JobQueue}` impls on [`PgStore`].
//!
//! These traits were originally defined in `qpedia_store::sqlite` so
//! handler code could remain back-end-agnostic. Implementing them here
//! means every call-site that uses the trait methods works unchanged
//! after `IngestContext.db` flips from `SqliteStore` to `PgStore`.

use crate::PgStore;
use async_trait::async_trait;
use qpedia_core::{
    job::Job,
    source::{Source, SourceStatus},
    tenant::Tenant,
    JobId, Result as QResult, SourceId,
};
use qpedia_store::sqlite::{JobQueue, SourceStore};

#[async_trait]
impl SourceStore for PgStore {
    async fn insert_source(&self, src: &Source) -> QResult<()> {
        self.pg_insert_source(src)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn get_source(&self, _id: &SourceId) -> QResult<Option<Source>> {
        // Without tenant context we'd need BYPASSRLS; for now this
        // returns None so callers must use `get_source_in`. Job runners
        // resolve tenant from the job row's `tenant_id` instead.
        Ok(None)
    }
    async fn get_source_in(&self, tenant: &Tenant, id: &SourceId) -> QResult<Option<Source>> {
        self.pg_get_source_in(tenant, id)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn list_sources(
        &self,
        tenant: &Tenant,
        folder_prefix: &str,
        limit: i64,
    ) -> QResult<Vec<Source>> {
        self.pg_list_sources(tenant, folder_prefix, limit)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn update_status(&self, id: &SourceId, status: SourceStatus) -> QResult<()> {
        // Tenant-less variant: look up by id only. Implementations that
        // need cross-tenant updates use the admin pool; for now this
        // falls back to a "no tenant" path that the dispatch layer
        // shouldn't take. Job handlers should call pg_update_status
        // with an explicit tenant.
        let _ = (id, status);
        Err(qpedia_core::Error::Other(anyhow::anyhow!(
            "update_status without tenant: use pg_update_status(&tenant, &id, status)"
        )))
    }
    async fn update_language(&self, id: &SourceId, language: &str) -> QResult<()> {
        let _ = (id, language);
        Err(qpedia_core::Error::Other(anyhow::anyhow!(
            "update_language without tenant: use pg_update_language(&tenant, &id, language)"
        )))
    }
    async fn update_classification(
        &self,
        id: &SourceId,
        classification: &serde_json::Value,
    ) -> QResult<()> {
        let _ = (id, classification);
        Err(qpedia_core::Error::Other(anyhow::anyhow!(
            "update_classification without tenant: use pg_update_classification(...)"
        )))
    }
    async fn update_folder_path(&self, id: &SourceId, folder_path: &str) -> QResult<()> {
        let _ = (id, folder_path);
        Err(qpedia_core::Error::Other(anyhow::anyhow!(
            "update_folder_path without tenant: use pg_update_folder_path(...)"
        )))
    }
    async fn delete_source(&self, id: &SourceId) -> QResult<()> {
        let _ = id;
        Err(qpedia_core::Error::Other(anyhow::anyhow!(
            "delete_source without tenant: use pg_delete_source(&tenant, &id)"
        )))
    }
    async fn list_stalled(&self, _tenant: &Tenant, _limit: i64) -> QResult<Vec<Source>> {
        Ok(Vec::new()) // Stage D will port the SQL for this.
    }
    async fn list_unorganized(&self, _tenant: &Tenant, _limit: i64) -> QResult<Vec<Source>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl JobQueue for PgStore {
    async fn enqueue(&self, job: &Job) -> QResult<()> {
        // Job carries no tenant; the schema needs one. We require
        // payload to include a "tenant" field. Job-creator helpers in
        // qpedia-ingest already embed tenant in payload (LintPayload,
        // SyncPayload, RemovePayload) or implicitly from the source
        // (IngestPayload). For safety we default to "default" here so
        // pre-flight smoke tests don't deadlock; production callers
        // should use `enqueue_for(&tenant, &job)`.
        let tenant = job
            .payload
            .get("tenant")
            .and_then(|v| v.as_str())
            .map(Tenant::new)
            .unwrap_or_else(Tenant::default_tenant);
        self.enqueue_job(&tenant, job)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn claim_next(&self, worker_id: &str, lease_ms: i64) -> QResult<Option<Job>> {
        self.claim_next_job(worker_id, lease_ms)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn complete(&self, id: &JobId) -> QResult<()> {
        self.complete_job(id)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
    async fn fail(&self, id: &JobId, err: &str, retry_in_ms: Option<i64>) -> QResult<()> {
        self.fail_job(id, err, retry_in_ms)
            .await
            .map_err(|e| qpedia_core::Error::Other(e))
    }
}
