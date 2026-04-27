-- Per-source classifier output (doc_type, language, sensitivity, hints, ...).
-- Stored as JSON for forward-compat; queryable via SQLite JSON1 functions.

ALTER TABLE sources ADD COLUMN classification_json TEXT;

-- Functional index over doc_type for folder/list filters.
CREATE INDEX IF NOT EXISTS sources_doctype
    ON sources(json_extract(classification_json, '$.doc_type'))
    WHERE classification_json IS NOT NULL;
