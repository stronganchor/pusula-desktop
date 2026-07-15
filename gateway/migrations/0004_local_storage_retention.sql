CREATE TABLE storage_purges (
    backup_id TEXT PRIMARY KEY NOT NULL REFERENCES backups(id),
    started_at INTEGER NOT NULL,
    completed_at INTEGER
) STRICT;

CREATE INDEX storage_purges_incomplete_idx
    ON storage_purges(completed_at, started_at);
