-- Both adapters supply authorized_files and file_scope. parent_folder and folder
-- are canonical directory prefixes ending in a separator (or the root prefix).
-- LIMIT includes a sentinel row so oversized exports and later pages are detectable.
SELECT f.*
FROM authorized_files f CROSS JOIN file_scope s
WHERE (s.folder IS NULL OR f.parent_folder = s.folder
       OR (s.recursive AND starts_with(f.parent_folder, s.folder)))
  AND (NOT s.ready_only OR f.downloadable)
ORDER BY f.sort_key, f.item_key
LIMIT (SELECT take_count FROM file_scope)
OFFSET (SELECT skip_count FROM file_scope)
