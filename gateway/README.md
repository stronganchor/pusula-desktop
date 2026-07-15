# Pusula backup gateway

This service receives age-encrypted Pusula Desktop snapshots over an
authenticated HTTPS relay and stores immutable ciphertext objects on the VPS.
It never receives SQLite plaintext or the age recovery identity. Pusula's
local SQLite database remains the production source of truth, and gateway,
network, or storage failures never block local business writes.

The production target is AlmaLinux 9 with cPanel Apache:

- public host: `pusula-backup.stronganchortech.com`;
- loopback listener: `127.0.0.1:12741` (never open this port in CSF);
- binary: `/usr/local/lib/pusula-backup-gateway/pusula-backup-gateway`;
- metadata: `/var/lib/pusula-backup-gateway/gateway.sqlite3`;
- encrypted objects: `/var/lib/pusula-backup-gateway/objects`;
- temporary relay spool: `/var/lib/pusula-backup-gateway/relay-spool`;
- configuration: `/etc/pusula-backup-gateway.env`, `root:root`, mode `0600`;
- service account: `pusula-backup`, with no login shell.

No cloud-storage account, bucket, API key, or outbound storage connection is
required. The only service secret is the stable token pepper.

## Security and durability model

- One-time enrollment codes and device bearer tokens are random and stored
  only as peppered HMAC hashes. Revocation takes effect on the next request.
- Every object key is server-generated beneath
  `backups/{rolling|daily|monthly}/{device UUID}/{YYYY}/{MM}/{DD}/`.
  Client input cannot select a filesystem path.
- The relay accepts only an authenticated, device-owned reservation with an
  exact `Content-Length` and `application/octet-stream` body. It streams at
  most the reserved bytes into a mode-`0600` private spool and verifies the
  ciphertext SHA-256 before publication.
- The spool and object root must share one filesystem. After `sync_all`, the
  gateway publishes the complete spool with an atomic, no-overwrite hard link.
  A crash can therefore expose either no object or one complete immutable
  object, never a partial final object. Retrying the same reservation verifies
  and reuses the existing bytes.
- Completion reopens the stored regular file without following symlinks,
  bounds the read by the reservation, hashes the actual body, and persists the
  exact size, SHA-256, verification time, and deterministic version ID
  `fs-sha256-<ciphertext SHA-256>` before returning `completed`.
- The object root and every generated directory are private. Objects are
  mode `0600` and directories mode `0700` on Unix. The systemd unit also uses
  `UMask=0077`, an empty capability set, and a strict writable-path allow-list.
- Before reading a relay body, available storage must cover its full size plus
  `PUSULA_GATEWAY_MIN_FREE_BYTES` (1 GiB by default).
- Request concurrency, SQLite concurrency, aggregate request rate, per-device
  token bucket, pending-record ceiling, byte quota, and daily/monthly cadence
  are independently bounded. A quota rejection happens before body streaming.
- API errors and logs never include device tokens, enrollment codes, object
  bodies, or filesystem contents.

The VPS object is an encrypted off-machine copy, but it is not an independent
storage-provider copy. Do not describe it as full 3-2-1 protection. Keep the
age recovery identity off both the Windows production PC and this VPS, with at
least two separately protected recovery copies.

## Retention

The gateway applies these defaults itself:

| Class | Purpose | Retention |
| --- | --- | ---: |
| `rolling` | manual, update, and frequent recovery points | 14 days |
| `daily` | one admitted snapshot per day | 60 days |
| `monthly` | one admitted snapshot per month | 400 days |

Cleanup is bounded to 100 objects per run by default and always preserves the
newest completed object for every device/class, even when it is older than the
nominal window. A database claim excludes an object from status and recovery
before unlinking it. If the process stops between those steps, startup or the
next reservation resumes that exact unfinished claim. Missing files are
treated as already unlinked and the claim is completed; unrelated files are
never scanned or deleted.

Pending reservations older than the configured 30-day ceiling use the same
claim-then-unlink sequence. This covers a crash after immutable publication but
before the completion row commits, without leaving an untracked object.

Retention runs before the listener starts and opportunistically before an
authenticated reservation. Root can also run it explicitly while the listener
is stopped, so stale-pending cleanup cannot race an active relay:

```sh
systemctl stop pusula-backup-gateway.service
runuser -u pusula-backup -- \
  /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway prune-storage
systemctl start pusula-backup-gateway.service
```

## Configuration

Copy `pusula-backup-gateway.env.example` to
`/etc/pusula-backup-gateway.env`, generate one random token pepper, and enforce:

```sh
chown root:root /etc/pusula-backup-gateway.env
chmod 0600 /etc/pusula-backup-gateway.env
```

`PUSULA_GATEWAY_OBJECT_ROOT` defaults to an `objects` sibling of the configured
database. Keep that root and the relay spool on the same filesystem. The
service refuses relative or parent-traversing paths.

Important defaults:

| Setting | Default | Effect |
| --- | ---: | --- |
| `PUSULA_GATEWAY_MAX_BACKUP_BYTES` | 256 MiB | Exact reservation/relay ceiling |
| `PUSULA_GATEWAY_MIN_FREE_BYTES` | 1 GiB | Free space preserved after ingress |
| `PUSULA_GATEWAY_RESERVATION_TTL_SECONDS` | 900 | Reservation refresh interval |
| `PUSULA_GATEWAY_MAX_PENDING_PER_DEVICE` | 8 | Pending metadata ceiling |
| `PUSULA_GATEWAY_DEVICE_24H_BYTE_QUOTA` | 1 GiB | Persisted reservation/relay byte ledger |
| `PUSULA_GATEWAY_DAILY_MIN_INTERVAL_SECONDS` | 20 hours | New daily reservation cadence |
| `PUSULA_GATEWAY_MONTHLY_MIN_INTERVAL_SECONDS` | 25 days | New monthly reservation cadence |
| `PUSULA_GATEWAY_MAX_REQUEST_CONCURRENCY` | 8 | Non-health request slots |
| `PUSULA_GATEWAY_MAX_DB_CONCURRENCY` | 4 | Blocking SQLite slots |

Keep the byte quota at least twice the maximum backup size because the current
conservative ledger charges both a reservation and its admitted relay.

## HTTP contract

All JSON bodies reject unknown fields. Protected routes require
`Authorization: Bearer <device_token>`. Responses use `Cache-Control: no-store`.

### `GET /healthz`

Returns `204`. It is process liveness only and deliberately does not touch the
database or object store.

### `POST /v1/enroll`

Consumes one one-time enrollment code and returns the canonical `device_id`,
one-time `device_token`, and `created_at`.

### `POST /v1/backups/upload-url`

The historical route name is retained for protocol continuity; it now creates
only a gateway-relay reservation and never returns an external URL:

```json
{
  "content_length": 123456,
  "sha256": "64_lowercase_hex_characters",
  "retention_class": "rolling"
}
```

```json
{
  "backup_id": "server-generated-uuid",
  "retention_class": "rolling",
  "transport": "gateway_relay",
  "expires_at": "2026-07-15T12:00:00Z"
}
```

An identical still-pending reservation is reused. A stale desktop binding gets
`404 not_found`, which permits the desktop to clear only that binding and
reserve the same ciphertext again.

### `PUT /v1/backups/relay/{backup_id}`

Requires the device bearer token, `Content-Type: application/octet-stream`,
and the exact reserved `Content-Length`. Success means the local object body
was independently verified and the database completion committed. The response
includes the immutable storage version ID. A lost success response is safe:
the next `/complete` probe returns the same completed record.

### `POST /v1/backups/complete`

Accepts `{"backup_id":"..."}`. It verifies an existing local object and
commits completion, or returns `409 object_not_present` when the relay has not
published it. Completed calls are idempotent.

### `GET /v1/backups/status`

Returns authenticated device identity, active/expired pending counts, server
time, and the newest retained, body-verified completed backup. Objects already
claimed for retention are excluded.

## Build and test

```sh
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path gateway/Cargo.toml
cargo build --release --locked --manifest-path gateway/Cargo.toml
```

The bundled SQLite build avoids a runtime SQLite dependency. Build the GNU/Linux
production artifact on AlmaLinux 9 or a compatible controlled runner.

## Root-only recovery commands

```sh
pusula-backup-gateway list-backups --limit 50
pusula-backup-gateway lookup-backup --backup-id UUID
pusula-backup-gateway download-backup --backup-id UUID --output /secure/path/backup.sqlite3.age
```

`download-backup` creates a new mode-`0600` output, refuses overwrite, validates
the recorded `fs-sha256-*` version, and re-hashes the exact stored body. Decrypt
and validate SQLite only on the designated recovery workstation. Never copy
the age recovery identity to the gateway.
