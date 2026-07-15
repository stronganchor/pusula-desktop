ALTER TABLE backups ADD COLUMN retention_class TEXT NOT NULL DEFAULT 'rolling'
    CHECK(retention_class IN ('rolling', 'daily', 'monthly'));

UPDATE backups
SET retention_class = CASE
    WHEN object_key LIKE '%/daily/%' THEN 'daily'
    WHEN object_key LIKE '%/monthly/%' THEN 'monthly'
    ELSE 'rolling'
END;

ALTER TABLE backups ADD COLUMN verified_size_bytes INTEGER
    CHECK(verified_size_bytes IS NULL OR verified_size_bytes > 0);
ALTER TABLE backups ADD COLUMN verified_sha256 TEXT
    CHECK(verified_sha256 IS NULL OR length(verified_sha256) = 64);
ALTER TABLE backups ADD COLUMN verified_at INTEGER;

CREATE INDEX backups_admission_cadence_idx
    ON backups(device_id, retention_class, created_at DESC);

CREATE INDEX backups_pending_reuse_idx
    ON backups(device_id, status, retention_class, size_bytes, sha256, created_at DESC);

CREATE INDEX backups_stale_pending_idx
    ON backups(status, upload_expires_at);

CREATE TABLE upload_authorizations (
    id INTEGER PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    backup_id TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
    authorized_at INTEGER NOT NULL
) STRICT;

CREATE INDEX upload_authorizations_device_time_idx
    ON upload_authorizations(device_id, authorized_at);

CREATE INDEX upload_authorizations_cleanup_idx
    ON upload_authorizations(authorized_at);
