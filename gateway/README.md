# Pusula backup gateway

This service gives one enrolled Pusula Desktop installation short-lived,
single-object Backblaze B2 upload URLs. The encrypted backup body travels from
the Windows app directly to B2; the gateway never receives or proxies it. B2
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
- Upload reservations use immutable server-generated object keys. A persistent
  token bucket allows a burst of five URL grants and refills one grant per
  minute by default.
- A grant expires after 15 minutes, covers one exact path and content length,
  and requires ciphertext SHA-256 metadata plus `AES256` SSE-B2.
- Completion performs a signed B2 `HEAD`; size, checksum metadata, and SSE must
  all match before the gateway marks a backup complete. Object bytes are never
  downloaded.
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
`writeFiles`. The service needs `readFiles` only for metadata `HEAD` checks; it
never reads an object body.

Configure and test B2 lifecycle rules for these independent prefixes:

| Prefix | Desktop use | Retention target |
| --- | --- | --- |
| `backups/rolling/` | frequent changed-data snapshots | 14 days |
| `backups/daily/` | one retained snapshot per day | 60 days |
| `backups/monthly/` | one retained snapshot per month | 400 days |

The gateway chooses the prefix from the allow-listed `retention_class`; it
does not delete objects. Lifecycle rules are therefore an explicit B2
deployment prerequisite. Do not give the runtime key `deleteFiles`.

Backblaze references: [S3-compatible API and presigned
URLs](https://www.backblaze.com/docs/cloud-storage-s3-compatible-api), [S3 Put
Object](https://www.backblaze.com/apidocs/s3-put-object), [S3 Head
Object](https://www.backblaze.com/apidocs/s3-head-object), [application-key
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

### `POST /v1/backups/complete`

```json
{ "backup_id": "server-generated-uuid" }
```

Returns the verified completion timestamp and optional B2 ETag/version. The
request is idempotent after successful verification. A missing or mismatched B2
object returns `502` and remains pending.

### `GET /v1/backups/status`

Returns the latest verified backup, active and expired pending counts, device
ID, and server time. It never returns an object key or storage credential.

## Administration

The CLI supports `migrate`, `issue-enrollment`, `revoke-enrollment`,
`issue-device`, and `revoke-device`. Secret-issuing commands print JSON to
stdout exactly once. Capture it only in the intended secure support channel.

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

See `RUNBOOK.md` for installation, Apache/cPanel integration, verification,
rotation, and rollback.
