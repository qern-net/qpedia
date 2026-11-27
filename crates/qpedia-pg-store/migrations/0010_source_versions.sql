-- History inherits the source row's tenant policy, ACL and deletion lifecycle.
ALTER TABLE sources ADD COLUMN prior_versions jsonb NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN version_uploaded_at timestamptz,
  ADD COLUMN restart_version_ingest boolean NOT NULL DEFAULT false;
CREATE INDEX sources_upload_name ON sources (tenant_id,md5(folder_path),md5(filename))
  WHERE filing_revision=0;
CREATE INDEX jobs_pending_source ON jobs (tenant_id,(payload->>'source_id'),id)
  WHERE kind='ingest' AND state IN ('queued','running');
