-- The blob filename and source slug remain immutable when the host refiles a document.
ALTER TABLE sources
  ADD COLUMN filing_name text,
  ADD COLUMN filing_revision integer NOT NULL DEFAULT 0,
  ADD CHECK ((filing_revision = 0 AND filing_name IS NULL) OR
             (filing_revision > 0 AND filing_name IS NOT NULL));
