# Pusula backup gateway

This service gives one enrolled Pusula Desktop installation short-lived,
single-object Backblaze B2 upload URLs. The normal path sends the encrypted
backup body from Windows directly to B2. If that direct connection fails before
an HTTP response because the local network blocks or breaks B2 transport, the
desktop can relay the same age-encrypted ciphertext through this gateway. The
gateway never receives SQLite plaintext or the age recovery identity. B2
credentials remain only in the root-owned service environment on the VPS.

The production target is AlmaLinux 9.8 with cPanel Apache:

- public host: `pusula-backup.stronganchortech.com`;
- loopback listener: `127.0.0.1:12741` (never open this port in CSF);
- binary: `/usr/local/lib/pusula-backup-gateway/pusula-backup-gateway`;
- metadata: `/var/lib/pusula-backup-gateway/gateway.sqlite3`;
- configuration: `/etc/pusula-backup-gateway.env`, `root:root`, mode `0600`;
- service account: `pusula-backup`, with no login shell.

## Security model

- Enrollment codes are random, peppered HMAC hashes at rest, expire, and can be
  used exactly once.
- Device bearer tokens are random, displayed once, and stored only as peppered
  HMAC hashes. Revocation takes effect on the next request.
- Upload reservations use immutable server-generated object keys. Admission is
  authoritative and persistent in SQLite: a device gets at most eight pending
  records, at most 1 GiB of authorized B2 bytes in a rolling 24-hour window,
  one new daily reservation per 20 hours, and one new monthly reservation per
  25 days by default. Pending and completed reservations both count for
  cadence.
- An exact same-device pending retry (`retention_class`, size, and SHA-256 all
  match) reuses its backup ID/object key and receives a newly signed URL. It
  bypasses cadence and max-pending checks but consumes a fresh persisted token
  and adds its full size to the rolling 24-hour authorization ledger. A
  different daily/monthly snapshot remains subject to the original
  reservation's cadence.
- A persistent device token bucket allows a burst of five storage exposures and
  refills one per minute by default. Every presigned direct PUT and every
  admitted relay write consumes a token and a full-size ledger entry, including
  the first relay. The 1 GiB default deliberately permits a normal 256 MiB
  direct-failure-plus-relay fallback. Bounded cleanup deletes at most 100 stale
  pending rows and 500 expired authorization rows per successful admission; it
  never deletes B2 objects.
- The authenticated relay accepts only an existing device-owned reservation,
  exact `Content-Length`, and `application/octet-stream`. It keeps at most one
  relay in flight, spools with a hard reservation-size bound, verifies the
  ciphertext SHA-256 before B2, and removes the mode-`0600` encrypted spool on
  success or failure. Startup removes only stale relay-part files left by a
  process or host crash before the listener is bound.
- Before accepting relay ingress, the gateway checks B2 for a correct object
  that may have landed despite a lost client response, then admits/charges the
  relay transactionally. A quota/token rejection occurs before body streaming
  or spool creation. Immediately before its PUT, the relay checks B2 again;
  only an authenticated 404 permits a new version. An authenticated pending
  relay remains usable after its presigned direct URL expires.
- A grant expires after 15 minutes, covers one exact path and content length,
  and requires ciphertext SHA-256 metadata plus `AES256` SSE-B2.
- Completion performs an authenticated B2 `GET` of that unique object key,
  requires the expected content length and SSE-B2 response, streams no more
  than the reserved bytes through SHA-256, and requires the actual body hash to
  match. The returned `x-amz-version-id`, actual size/hash, and verification
  time are persisted before status becomes `completed`. Direct and relay paths
  share this verifier; caller-supplied object metadata is not integrity proof.
- Non-health requests pass process-wide fail-fast request and SQLite
  concurrency gates plus an aggregate token bucket before Axum buffers or
  extracts a JSON body. Saturation returns `503`; aggregate/device admission
  returns `429`. `/healthz` remains a minimal bypassed liveness response.
- API errors and logs do not include tokens, enrollment codes, presigned URLs,
  B2 credentials, or object bodies.

Tokens and enrollment codes have enough random entropy for fast HMAC lookup;
they are not user-selected passwords. Keep
`PUSULA_GATEWAY_TOKEN_PEPPER` stable and backed up securely. Changing it
invalidates every outstanding enrollment code and device token.

## Build and test

Use a current stable Rust toolchain:

```sh
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path gateway/Cargo.toml
cargo build --release --locked --manifest-path gateway/Cargo.toml
```

The bundled SQLite build avoids a runtime SQLite library dependency. Build the
production GNU/Linux artifact on AlmaLinux 9 (or a compatible controlled build
runner), not on Windows.

## B2 setup

Create the private bucket `stronganchor-pusula-desktop-backups`, enable SSE-B2,
and create a runtime application key restricted to that bucket and the
`backups/` prefix. Give it only `listBuckets`, `listFiles`, `readFiles`, and
`writeFiles`. The service uses `readFiles` for authenticated full-body
verification and exact-version administrator recovery downloads. It never
decrypts those age-encrypted bytes.

Configure and test B2 lifecycle rules for these independent prefixes:

| Prefix | Desktop use | Retention target |
| --- | --- | --- |
| `backups/rolling/` | frequent changed-data snapshots | 14 days |
| `backups/daily/` | one retained snapshot per day | 60 days |
| `backups/monthly/` | one retained snapshot per month | 400 days |

The gateway chooses the prefix only after its server-side cadence/quota policy
admits the allow-listed `retention_class`; the client cannot create unlimited
daily/monthly object keys. The gateway does not delete objects. Lifecycle rules
are therefore an explicit B2 deployment prerequisite. Do not give the runtime
key `deleteFiles`.

Backblaze references: [S3-compatible API and presigned
URLs](https://www.backblaze.com/docs/cloud-storage-s3-compatible-api), [S3 Put
Object](https://www.backblaze.com/apidocs/s3-put-object), [S3 Get
Object](https://www.backblaze.com/apidocs/s3-get-object), [application-key
capabilities](https://www.backblaze.com/docs/cloud-storage-application-key-capabilities),
and [S3 lifecycle mapping](https://www.backblaze.com/apidocs/s3-put-lifecycle-configuration).

Copy `pusula-backup-gateway.env.example` to
`/etc/pusula-backup-gateway.env`, replace all placeholders, and enforce:

```sh
chown root:root /etc/pusula-backup-gateway.env
chmod 0600 /etc/pusula-backup-gateway.env
```

`PUSULA_GATEWAY_B2_ENDPOINT` and `PUSULA_GATEWAY_B2_REGION` come from the B2
bucket details. HTTP endpoints are rejected unless the explicit test-only
`PUSULA_GATEWAY_ALLOW_INSECURE_B2_ENDPOINT=true` override is present.

Admission/load defaults in the example environment are release policy, not
desktop suggestions:

| Setting | Default | Effect |
| --- | ---: | --- |
| `PUSULA_GATEWAY_MAX_PENDING_PER_DEVICE` | 8 | Hard pending-record ceiling per device |
| `PUSULA_GATEWAY_DEVICE_24H_BYTE_QUOTA` | 1 GiB | Bytes exposed by direct grants and admitted relay writes per rolling 24 hours |
| `PUSULA_GATEWAY_PENDING_MAX_AGE_SECONDS` | 30 days | Stale cutoff measured from an expired grant |
| `PUSULA_GATEWAY_PENDING_CLEANUP_LIMIT` | 100 | Maximum metadata deletions per admission |
| `PUSULA_GATEWAY_AUTHORIZATION_CLEANUP_LIMIT` | 500 | Maximum expired ledger-row deletions per admission |
| `PUSULA_GATEWAY_DAILY_MIN_INTERVAL_SECONDS` | 20 hours | Minimum new daily cadence; configuration cannot go lower |
| `PUSULA_GATEWAY_MONTHLY_MIN_INTERVAL_SECONDS` | 25 days | Minimum new monthly cadence; configuration cannot go lower |
| `PUSULA_GATEWAY_MAX_REQUEST_CONCURRENCY` | 8 | Non-health requests held process-wide |
| `PUSULA_GATEWAY_MAX_DB_CONCURRENCY` | 4 | Blocking SQLite operations held process-wide |
| `PUSULA_GATEWAY_GLOBAL_REQUEST_BURST` | 60 | Aggregate non-health request burst |
| `PUSULA_GATEWAY_GLOBAL_REQUEST_REFILL_SECONDS` | 1 second | Time to restore one aggregate request token |

Keep the byte quota at least twice `PUSULA_GATEWAY_MAX_BACKUP_BYTES`.
The service rejects unsafe cadence floors and zero/oversized limits at startup.
The aggregate limiter resets on process restart; the authorization ledger/byte
quota, cadence, pending counts, and per-device token bucket remain persisted in
SQLite. Size the quota for at least two maximum-size entries so one normal
direct failure can use the authenticated relay without lowering the policy.

## HTTP contract

All JSON request bodies reject unknown fields. Protected routes require
`Authorization: Bearer <device_token>`. Responses use `Cache-Control: no-store`.

### `GET /healthz`

Returns `204` as a process-liveness check. It deliberately touches no database
or B2 resource and reveals no version, storage, or credential details. Treat failure as a warning: local Pusula writes must
continue even when this service, Apache, the VPS, or B2 is unavailable.

### `POST /v1/enroll`

```json
{
  "enrollment_code": "pen_REDACTED",
  "device_name": "Front Desk"
}
```

Returns `201` with `device_id`, the one-time `device_token`, and `created_at`.
Replay, expiry, or revocation returns `401`.

### `POST /v1/backups/upload-url`

```json
{
  "content_length": 123456,
  "sha256": "64_hex_characters_over_the_encrypted_file",
  "retention_class": "rolling"
}
```

`retention_class` is `rolling`, `daily`, or `monthly` and defaults to
`rolling`. `content_length` must be 1 through 268,435,456 bytes by default.
The response contains:

```json
{
  "backup_id": "server-generated-uuid",
  "retention_class": "rolling",
  "method": "PUT",
  "upload_url": "short-lived-B2-URL",
  "required_headers": {
    "content-length": "123456",
    "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
    "x-amz-meta-sha256": "ciphertext-sha256",
    "x-amz-server-side-encryption": "AES256"
  },
  "expires_at": "RFC3339 timestamp"
}
```

The desktop must send every returned header on the B2 `PUT`, must not send its
gateway bearer token to B2, and must discard the URL after that one attempt.
Never write a presigned URL to diagnostics because it is a temporary
credential.

If an identical pending request is retried, the response carries the existing
`backup_id` and object key with a fresh signature, but that new storage
exposure consumes another token and full-size 24-hour ledger entry. Requests
that exceed pending, byte, daily/monthly cadence, device-token, or aggregate
limits return `429` with `Retry-After`. Request/DB saturation returns fail-fast
`503`.

### `PUT /v1/backups/relay/{backup_id}`

This transport fallback is used only after a failed or ambiguous direct B2
`PUT` has been checked through completion and the gateway returned the
definitive `object_not_present` result. It requires the normal device bearer
token, `Content-Type: application/octet-stream`, an exact `Content-Length`
matching the reservation, and the raw `.sqlite3.age` bytes as the entire body.
It does not accept plaintext exports, recovery keys, a new object name, or a
caller supplied checksum.

Before reading the body, the gateway checks whether the exact object already
exists and is correct, then charges the relay token/byte ledger. Rejection
therefore reads no ciphertext and creates no spool. It then bounds and hashes
the encrypted body, rechecks B2 immediately before any PUT, and uploads only
after an authoritative missing-object response. The same streaming GET verifier
finishes completion. A completed reservation is idempotent: a lost client
response followed by another relay or complete call returns the stored result
and never issues a second PUT. A relay PUT transport failure or HTTP
`408`/`429`/`5xx` response is confirmation-GETed before the gateway returns;
it is never followed by a blind replacement PUT. Indeterminate B2
upload/verification failure returns `502` and leaves the reservation pending;
concurrent or over-rate relays return `429`.

### `POST /v1/backups/complete`

```json
{ "backup_id": "server-generated-uuid" }
```

Returns the verified completion timestamp, B2 ETag when present, and required
exact B2 version ID. The request is idempotent after successful verification.
An authoritative B2 missing-object result returns `409` with code
`object_not_present`; the same reservation remains pending and relayable. A
gateway binding that no longer exists returns `404` with code `not_found` and
may be re-reserved. Missing version/SSE, transport uncertainty, size mismatch,
or actual-body SHA-256 mismatch returns `502`; reconfirm it without issuing a
blind replacement PUT.

### `GET /v1/backups/status`

Returns the latest verified backup, active and expired pending counts, device
ID, and server time. It never returns an object key or storage credential.

## Administration

The CLI supports `migrate`, `issue-enrollment`, `revoke-enrollment`,
`issue-device`, and `revoke-device`. Secret-issuing commands print JSON to
stdout exactly once. Capture it only in the intended secure support channel.
It also provides Unix-root-only `list-backups`, `lookup-backup`, and
`download-backup`. The first two open metadata read-only and return only
completed records with exact actual-body verification evidence. Download signs
an authenticated GET for that exact `versionId`, creates the output without
replacement (mode `0600` on Unix), streams through the stored size/SHA-256, and
deletes a partial on any failure. These commands never print a storage URL or
credential.

Migration does not claim that older metadata-only completed rows were body
verified. Such rows remain unavailable to verified status/list/lookup/download
until they are independently recovered under `RUNBOOK.md`.

The root-owned environment file cannot be sourced by the service account. On
AlmaLinux, run secret-aware admin commands as a short-lived systemd unit so
systemd reads the file and the process still owns SQLite files as
`pusula-backup`:

```sh
systemd-run --quiet --pipe --wait --collect \
  --unit="pusula-backup-admin-$(date +%s)" \
  --property=User=pusula-backup \
  --property=Group=pusula-backup \
  --property=WorkingDirectory=/var/lib/pusula-backup-gateway \
  --property=EnvironmentFile=/etc/pusula-backup-gateway.env \
  /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway \
  issue-enrollment --label "Customer front desk" --expires-hours 24
```

Use the returned public `enrollment_id` or `device_id` with the corresponding
revoke command. `issue-device` is a break-glass alternative to enrollment; the
normal installation flow should use a short-lived enrollment code.

Run recovery commands as root, not as the service user. `systemd-run` can load
the root-only environment without exposing it in the shell or process list:

```sh
install -d -o root -g root -m 0700 /root/pusula-recovery

systemd-run --quiet --pipe --wait --collect \
  --unit="pusula-backup-recovery-$(date +%s)" \
  --property=EnvironmentFile=/etc/pusula-backup-gateway.env \
  /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway \
  list-backups --limit 20

systemd-run --quiet --pipe --wait --collect \
  --unit="pusula-backup-recovery-$(date +%s)" \
  --property=EnvironmentFile=/etc/pusula-backup-gateway.env \
  /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway \
  download-backup --backup-id BACKUP_UUID \
  --output /root/pusula-recovery/BACKUP_UUID.sqlite3.age
```

If the output path already exists, download fails without changing it. Treat
the resulting age ciphertext and the separate recovery identity as sensitive
recovery material even though the gateway never sees plaintext.

See `RUNBOOK.md` for installation, Apache/cPanel integration, verification,
rotation, and rollback.
