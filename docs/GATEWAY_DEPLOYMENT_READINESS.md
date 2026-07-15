# Gateway deployment readiness

Snapshot date: 2026-07-15

This checklist tracks the production deployment of the Pusula encrypted-backup
gateway. It does not change the local-first contract: gateway, network, or
storage failure must never prevent SQLite business writes.

## Architecture selected for the one-machine release

- The Windows PC encrypts every remote backup with the embedded age recipient
  before it leaves the machine.
- The authenticated HTTPS gateway relays the ciphertext to immutable local
  storage on the existing VPS.
- The VPS stores no SQLite plaintext and no age recovery identity.
- No external object-store account, bucket, API key, lifecycle configuration,
  or desktop upload URL is required.
- The gateway generates object paths, verifies exact length and SHA-256,
  publishes atomically without overwrite, and records version
  `fs-sha256-<ciphertext SHA-256>`.
- Gateway retention is rolling 14 days, daily 60 days, and monthly 400 days;
  cleanup is bounded, resumable, and preserves the newest completed object for
  every device/class.

This deliberately trades an independent storage-provider copy for a much
simpler initial deployment that Codex can operate on the already-accessible
VPS. The encrypted VPS object is off-machine continuity, not complete 3-2-1
protection. The object tree and metadata must also remain in the VPS operator's
server-backup system.

## Existing host state before activation

- Public host: `pusula-backup.stronganchortech.com`.
- Authoritative/public DNS resolves to `69.167.167.14`.
- An exact-host AutoSSL certificate is installed.
- cPanel ownership is isolated under user `satbiz5`.
- Gateway listener target is loopback-only `127.0.0.1:12741`; do not open that
  port in CSF.
- The previously staged gateway is disabled and inactive. It is superseded by
  the local-object-storage build and is not an approved rollback target.

The production environment file, migration 4, final binary, Apache includes,
service activation, enrollment, and end-to-end recovery proof remain gated
until the exact merged release commit is available. A minimal `/healthz`
response proves only process liveness.

## Activation gate

- [ ] Merge the reviewed source and require green desktop, gateway, release
  policy, recovery-custody, migration, and frontend tests at the exact commit.
- [ ] Build the locked AlmaLinux release binary from that exact source and
  record source commit, Rust version, binary SHA-256, build UTC, migration
  hashes, and rollback paths in root-only provenance.
- [ ] Archive the old disabled binary, unit, docs, and evidence before install.
- [ ] Install the new binary, hardened systemd unit, and docs while the service
  remains stopped.
- [ ] Create `/etc/pusula-backup-gateway.env` as `root:root 0600`. Generate the
  token pepper without printing it. No other service secret is needed.
- [ ] Require database and object root below
  `/var/lib/pusula-backup-gateway`, owned by the non-login service account with
  private modes, and require relay spool/object root on the same filesystem.
- [ ] Run migration 4 and prove migration checksums, SQLite integrity, and zero
  foreign-key failures.
- [ ] Resolve the cPanel userdata include locations from live configuration,
  install the specific relay and catch-all proxy rules, rebuild Apache, and
  require `Syntax OK` before a graceful reload.
- [ ] Enable/start the service and prove only `127.0.0.1:12741` listens.
- [ ] Require loopback and public `/healthz` to return `204` while existing
  monitored sites remain healthy.
- [ ] Issue a one-hour enrollment code, enroll a disposable Windows test
  device without logging the code/token, and revoke it after acceptance.
- [ ] Relay a real age-encrypted SQLite fixture from the Windows network and
  require a completed record with matching reserved and verified length/hash,
  nonempty verification time, and exact `fs-sha256-*` version.
- [ ] Require an empty relay spool and a regular mode-`0600` immutable object
  beneath the server-generated device/class/date path.
- [ ] Download that exact object with the root-only recovery command, copy only
  the ciphertext to the controlled Windows recovery directory, decrypt with
  the separately held identity, and prove SQLite integrity, counts, and
  integer-kuruş totals.
- [ ] Prove a truncated body, wrong checksum, wrong content length, unknown
  backup ID, and symlink/non-regular object are rejected without publication.
- [ ] Run one bounded retention pass and prove it neither scans nor deletes an
  unrelated file and does not remove the newest device/class backup.
- [ ] Recheck shared-host load, memory, listen queues, disk free space, service
  logs, and several existing sites after acceptance.

Do not call the desktop release backup-complete until every applicable item is
evidenced. Never include environment contents, enrollment codes, device
tokens, the token pepper, recovery passphrase, private keys, production
exports, or customer data in GitHub artifacts or tracked documentation.

## Activation authority and rollback

Use `gateway/RUNBOOK.md` for canonical commands. The deployment must back up
and restore only the exact Pusula Apache userdata includes it changes. On a
failure, stop/disable the gateway, restore the archived binary/unit/docs and
Apache includes as appropriate, rebuild Apache, require `Syntax OK`, and reload
gracefully. Preserve the environment, metadata, object tree, and failure logs.

Schema migration 4 adds resumable local-storage retention state. Do not edit an
applied migration and do not down-migrate SQLite. The former cloud-dependent
binary cannot serve the new local objects and is not a production fallback;
roll forward with a corrected local-storage build or keep the service stopped.
