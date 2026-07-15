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
file is absent and TCP 12741 has no listener. The audited cPanel subdomain,
authoritative/public DNS, and exact-host AutoSSL certificate are complete. The
Apache proxy, Backblaze resource, enrollment code, and device token remain
absent.

## 2026-07-15 relay compatibility hold

The current Windows network cannot establish TLS to any advertised
`s3.us-west-004.backblazeb2.com` IPv4 address. A raw TLS probe received a
plaintext Türk Telekom `307` sinkhole response from `88.255.216.16`, while the
VPS reached the same B2 endpoint with a verified TLS connection. Because the
desktop normally uploads ciphertext directly to B2, this is a production
transport blocker rather than a cosmetic test failure.

The merged source now contains a narrowly scoped fallback: only after a direct
B2 transport/TLS failure, the enrolled desktop can send the same age-encrypted
ciphertext through `PUT /v1/backups/relay/{backup_id}`. The gateway bounds and
hashes a private spool, uploads it to the reserved B2 object, performs the same
strict `HEAD` verification, and removes the spool on every ordinary path.
Startup removes crash-left relay parts before binding. One relay is allowed at
a time; the reservation pays for the first fallback and later retries consume
the persisted device token bucket. Pending relay retries remain valid after the
presigned direct URL expires.

The installed VPS binary and unit are still the earlier disabled staging build.
They do not contain the relay endpoint or immutable migration 2. Do not activate
that binary for this release. Rebuild the final reviewed commit on AlmaLinux,
pass formatting, clippy, all gateway tests, and a locked release build, then
replace the disabled staged binary and re-record its source commit and SHA-256
before creating the production environment file.

## Outstanding production gates

- [ ] Build and install the final relay-capable gateway binary from the exact
  release commit while the unit remains disabled/inactive. Verify the binary,
  hardened unit, Apache templates, immutable v1-to-v2 migration test, and
  checksum/provenance readback before activation.
- [ ] Create the private B2 bucket with SSE-B2 in Backblaze region
  `us-west-004`. The current desktop upload allow-list requires endpoint
  `https://s3.us-west-004.backblazeb2.com`, bucket
  `stronganchor-pusula-desktop-backups`, and object prefix `backups/`; creating
  the bucket in another region will make the desktop reject every upload URL.
- [ ] Add lifecycle rules with `daysFromUploadingToHiding` set to 14 for
  `backups/rolling/`, 60 for `backups/daily/`, and 400 for
  `backups/monthly/`, plus `daysFromHidingToDeleting` set to 1 on every rule.
  Gateway object names are unique, so a hidden-version-only rule would never
  retire these current objects. Read the three exact prefixes and values back
  before activation.
- [ ] Create a `backups/`-restricted runtime key with only `listBuckets`,
  `listFiles`, `readFiles`, and `writeFiles`. If the web console cannot express
  that exact set, use the Native API; verify the resulting key metadata does
  not include `deleteFiles`, `listAllBucketNames`, or bucket-management access.
- [ ] Confirm at least two secure, off-device copies of the age recovery
  identity. The identity must not be stored on the gateway.
- [x] Create and audit the cPanel domain for
  `pusula-backup.stronganchortech.com`. It is isolated under cPanel user
  `satbiz5` with the exact public server name.
- [x] Add and verify authoritative/public DNS, then issue AutoSSL for the exact
  public host. Authoritative, Google, and Cloudflare resolvers return
  `69.167.167.14`; the publicly verified certificate covers only the exact
  gateway host and is valid through 2026-10-12.
- [ ] Create `/etc/pusula-backup-gateway.env` as `root:root 0600` without
  printing or logging its values.
- [ ] Start the service and prove that only `127.0.0.1:12741` is listening.
- [ ] Resolve the cPanel userdata templates using the live cPanel user and
  vhost identifiers; rebuild Apache, require `Syntax OK`, and reload
  gracefully.
- [ ] Verify loopback and public `/healthz` return `204` while existing
  monitored sites remain healthy.
- [ ] Enroll a test device and complete one encrypted upload from the actual
  Windows network through the ciphertext relay, B2 `HEAD` verification, and
  status. Require the relay spool to be empty afterward. Also re-test the
  preferred direct B2 route; if the ISP sinkhole remains, record relay as the
  expected durability path for that network.
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
