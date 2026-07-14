CREATE TABLE enrollment_codes (
    id TEXT PRIMARY KEY NOT NULL,
    label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 120),
    code_hash BLOB NOT NULL UNIQUE CHECK(length(code_hash) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    revoked_at INTEGER,
    CHECK(expires_at > created_at)
) STRICT;

CREATE TABLE devices (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 120),
    token_hash BLOB NOT NULL UNIQUE CHECK(length(token_hash) = 32),
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    last_seen_at INTEGER,
    upload_tokens REAL NOT NULL DEFAULT 5.0 CHECK(upload_tokens >= 0.0),
    upload_tokens_updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE backups (
    id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    object_key TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
    sha256 TEXT NOT NULL CHECK(length(sha256) = 64),
    status TEXT NOT NULL CHECK(status IN ('pending', 'completed')),
    created_at INTEGER NOT NULL,
    upload_expires_at INTEGER NOT NULL,
    completed_at INTEGER,
    etag TEXT,
    version_id TEXT,
    CHECK(upload_expires_at > created_at),
    CHECK(
        (status = 'pending' AND completed_at IS NULL)
        OR (status = 'completed' AND completed_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX backups_device_created_idx
    ON backups(device_id, created_at DESC);

CREATE INDEX backups_device_status_idx
    ON backups(device_id, status);
