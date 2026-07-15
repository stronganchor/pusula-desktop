# Gateway deployment readiness

Snapshot date: 2026-07-15

This checklist tracks the production deployment of the optional Pusula backup
gateway. It does not change the desktop's local-first contract: backup or
gateway failure must never prevent SQLite business writes.

## Final disabled staging on the AlmaLinux host

- Gateway Git tree: `12bd31eb664fc496a1c5966f0e78b34040004ebf`
- Rust toolchain: `1.92.0-x86_64-unknown-linux-gnu`
- Gateway version: `0.1.0`
- Validation: formatting, clippy with warnings denied, all unit/integration/doc
  tests plus the 30-name security-critical audit, locked release build,
  ELF/dependency checks, source-archive safety checks, and immutable SQLite
  v1-to-v3 migration verification passed on AlmaLinux 9.8.
- Migration evidence: pinned SQLite 3.53.2 reported integrity `ok`; migrations
  1 `initial`, 2 `relay_attempted_at`, and 3
  `backup_admission_and_verification` matched their exact blobs and normalized
  SQL hashes; migration 3 schema checks passed and foreign-key failures were
  zero.
- Installed binary:
  `/usr/local/lib/pusula-backup-gateway/pusula-backup-gateway`
- Installed unit: `/etc/systemd/system/pusula-backup-gateway.service`
- Installed state directory: `/var/lib/pusula-backup-gateway`, owned by the
  non-login `pusula-backup` account and mode `0700`.

The exact installed commit, build UTC, binary SHA-256, migration/evidence
hashes, release/archive paths, and rollback paths are authoritative in the
root-owned installed `BUILD_PROVENANCE`, immutable versioned evidence tree, and
operator handoff record. They are intentionally not duplicated as a
self-referential release-commit value in this tracked file: a documentation-only
commit changes the candidate SHA while leaving the gateway Git tree above
unchanged. Before activation, independently read those live values and require
the installed gateway tree to equal the exact candidate commit's `gateway/`
tree.

The service is intentionally `disabled`, `inactive`, and `dead`, with no
production environment file, database entry, process, listener on TCP 12741,
or Apache userdata proxy. The audited cPanel subdomain, authoritative/public
DNS, and exact-host AutoSSL certificate are complete. The Backblaze resource,
enrollment code, and device token remain absent.

The installed binary includes migration 3, authenticated full-body B2
verification, exact-version recovery downloads, persistent byte admission,
bounded database/request concurrency, and ciphertext relay. It is the final
integrated gateway tree, but it must remain disabled until every remaining
Backblaze, recovery-identity, activation, and live acceptance gate below passes.

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
hashes a private spool, uploads it to the reserved B2 object, then verifies the
actual authenticated B2 response body, exact size, SHA-256, and nonempty object
version ID before completion. The spool is removed on every ordinary path, and
startup removes crash-left relay parts before binding. One relay is allowed at
a time. Every issued direct authorization, exact re-sign, and admitted relay
attempt consumes one device token plus the full reservation size in the
persistent 24-hour ledger. Pending relay retries remain valid after the
presigned direct URL expires.

The installed VPS binary is the final reviewed relay implementation. Live,
versioned, and root-only handoff copies matched exactly; provenance format 2
and the exact 13-file evidence set passed independent readback. The superseded
binary and documentation remain in durable archives and intentional rollback
copies. The service is still `disabled`, `inactive`, and `dead`; the production
environment, listener, process, Apache proxy, B2 resources, enrollment, and
gateway state remain absent. Successful disabled staging is not authorization
to activate the service.

## Outstanding production gates

- [x] Build and install the final gateway tree while the unit remains
  disabled/inactive/dead. Formatting, clippy with warnings denied, all gateway
  tests, locked release build, hardened unit and Apache-template checks,
  immutable v1-to-v3 migration verification, exact migration and evidence
  hashes, binary SHA-256, archive/rollback preservation, cleanup, and
  independent provenance readback passed. Re-read the live authority described
  above against the exact candidate commit before activation.
- [ ] Create the private B2 bucket with SSE-B2 in Backblaze region
  `us-west-004`. The current desktop upload allow-list requires endpoint
  `https://s3.us-west-004.backblazeb2.com`, bucket
  `stronganchor-pusula-desktop-backups`, and object prefix `backups/`; creating
  the bucket in another region will make the desktop reject every upload URL.
  A credential-safe local probe found one existing authenticated `rclone`
  remote, but it authorizes against `api002.backblazeb2.com`, is read-only and
  restricted to one unrelated existing bucket, and cannot create or administer
  the required resource. Do not weaken the endpoint check or reuse that key.
  Backblaze assigns region at account creation and does not let an existing
  account change regions, so an owner-authorized region-004 account may be
  required.
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
  Windows network through the ciphertext relay, authenticated full-body B2
  `GET` verification, exact nonempty version ID, verified size/SHA-256/time, and
  status. Require the relay spool to be empty afterward. Also re-test the
  preferred direct B2 route; if the ISP sinkhole remains, record relay as the
  expected durability path for that network.
- [ ] Restore that ciphertext with the separately held age identity and prove
  SQLite integrity, counts, and financial totals.

Do not publish an initial desktop release as backup-complete until every item
above is evidenced. A minimal `/healthz` response proves only process
liveness; it does not prove B2 permissions, lifecycle, encryption recovery, or
desktop upload behavior.

Migration 3 cannot retroactively prove the bytes or exact object version for a
row completed by an older binary. Legacy completed rows whose `version_id` or
`verified_size_bytes`, `verified_sha256`, or `verified_at` is missing are
therefore excluded from verified list/lookup/download operations. Preserve
them as incident/history data; recover through an independently inventoried
exact B2 version and the restore harness as documented in `gateway/RUNBOOK.md`,
not by fabricating verification fields.

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
