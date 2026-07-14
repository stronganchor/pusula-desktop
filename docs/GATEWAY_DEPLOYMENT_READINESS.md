# Gateway deployment readiness

Snapshot date: 2026-07-14

This checklist tracks the production deployment of the optional Pusula backup
gateway. It does not change the desktop's local-first contract: backup or
gateway failure must never prevent SQLite business writes.

## Staged on the AlmaLinux host

- Source commit: `0aa68d0d4059d2c3dec5ab251540ee472dbc83c7`
- Rust toolchain: `1.92.0-x86_64-unknown-linux-gnu`
- Gateway version: `0.1.0`
- Release binary SHA-256:
  `6eb765dec66836cc9e06744e3e3bf1d3e294b2221e24c76e3c37ea1c897e5b4c`
- Validation: formatting, clippy with warnings denied, 9 unit tests, 3 API
  tests, and locked release build passed on AlmaLinux 9.8.
- Installed binary:
  `/usr/local/lib/pusula-backup-gateway/pusula-backup-gateway`
- Installed unit: `/etc/systemd/system/pusula-backup-gateway.service`
- Installed state directory: `/var/lib/pusula-backup-gateway`, owned by the
  non-login `pusula-backup` account and mode `0700`.

The service is intentionally disabled and inactive. The production environment
file is absent and TCP 12741 has no listener. No DNS, cPanel domain, Apache
proxy, Backblaze resource, enrollment code, or device token was created during
staging.

## Outstanding production gates

- [ ] Create the private B2 bucket with SSE-B2.
- [ ] Add lifecycle rules for `backups/rolling/` (14 days),
  `backups/daily/` (60 days), and `backups/monthly/` (400 days).
- [ ] Create a `backups/`-restricted runtime key with only `listBuckets`,
  `listFiles`, `readFiles`, and `writeFiles`.
- [ ] Confirm at least two secure, off-device copies of the age recovery
  identity. The identity must not be stored on the gateway.
- [ ] Create and audit the cPanel domain for
  `pusula-backup.stronganchortech.com`.
- [ ] Add and verify authoritative/public DNS, then issue AutoSSL for the exact
  public host.
- [ ] Create `/etc/pusula-backup-gateway.env` as `root:root 0600` without
  printing or logging its values.
- [ ] Start the service and prove that only `127.0.0.1:12741` is listening.
- [ ] Resolve the cPanel userdata templates using the live cPanel user and
  vhost identifiers; rebuild Apache, require `Syntax OK`, and reload
  gracefully.
- [ ] Verify loopback and public `/healthz` return `204` while existing
  monitored sites remain healthy.
- [ ] Enroll a test device and complete one encrypted upload through B2 `HEAD`
  verification and status.
- [ ] Restore that ciphertext with the separately held age identity and prove
  SQLite integrity, counts, and financial totals.

Do not publish an initial desktop release as backup-complete until every item
above is evidenced. A minimal `/healthz` response proves only process
liveness; it does not prove B2 permissions, lifecycle, encryption recovery, or
desktop upload behavior.

## Activation and rollback authority

Use `gateway/RUNBOOK.md` for the canonical service, cPanel Apache, credential
rotation, metadata recovery, and rollback procedure. Keep the environment and
gateway metadata database together for recovery, but back them up through
separate secret-safe mechanisms. Never put production exports, device tokens,
enrollment codes, presigned URLs, the token pepper, B2 credentials, or the age
recovery identity in GitHub artifacts or diagnostics.

If activation fails, disable and stop the unit, remove only the exact Pusula
cPanel userdata includes, rebuild and syntax-check Apache, and then reload
gracefully. Preserve the environment and metadata for diagnosis. Do not delete
encrypted B2 objects and do not improvise a SQLite down-migration.
