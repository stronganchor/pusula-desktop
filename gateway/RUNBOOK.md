# Pusula backup gateway operations

## Initial installation

1. Confirm AlmaLinux 9.8 health and that `127.0.0.1:12741` is unused. Do not add
   a CSF allowance for TCP 12741.
2. Create the dedicated identity:

   ```sh
   useradd --system --home-dir /var/lib/pusula-backup-gateway \
     --shell /sbin/nologin pusula-backup
   install -d -o root -g root -m 0755 /usr/local/lib/pusula-backup-gateway
   install -d -o root -g root -m 0755 /usr/local/share/doc/pusula-backup-gateway
   ```

3. Install the release binary mode `0755`, the service unit mode `0644`, the
   README/runbook mode `0644`, and the completed environment file mode `0600`.
   Confirm the B2 key is bucket- and prefix-restricted before starting. Review
   the pending, 24-hour byte, cadence, request, DB, and aggregate-rate settings;
   do not lower the enforced 20-hour daily or 25-day monthly floors. The
   rolling byte quota must cover at least one maximum direct grant plus one
   maximum relay fallback.
4. Run `systemctl daemon-reload`, then
   `systemctl enable --now pusula-backup-gateway.service`. The unit creates the
   state directory and runs migrations before every start. Before binding the
   listener, `serve` removes only stale `.relay-*.sqlite3.age.part` files left
   by an interrupted process from the private relay spool. It logs a count,
   never a filename or object credential.
5. Verify:

   ```sh
   systemctl status --no-pager pusula-backup-gateway.service
   ss -lntp | grep '127.0.0.1:12741'
   curl --fail --silent --show-error -o /dev/null \
     -w '%{http_code}\n' http://127.0.0.1:12741/healthz
   ```

   Expected HTTP status is `204`; there must be no non-loopback listener.

## cPanel Apache

Install the two templates from `deploy/apache/` in the actual cPanel userdata
paths, replacing `__CPANEL_USER__` and `__VHOST__` with the audited account and
Apache vhost identifiers:

```text
/etc/apache2/conf.d/userdata/std/2_4/__CPANEL_USER__/__VHOST__/pusula-backup-gateway.conf
/etc/apache2/conf.d/userdata/ssl/2_4/__CPANEL_USER__/__VHOST__/pusula-backup-gateway.conf
```

The standard vhost redirects to HTTPS; only the SSL vhost proxies to loopback.
The SSL template keeps every JSON request body at 16 KiB and ordinary control
routes at 25 seconds. The completion route gets a 900-second response window
because it streams the full encrypted B2 object through SHA-256, but it retains
the 16 KiB request limit. Only `/v1/backups/relay/` gets the 256 MiB body limit
and 900-second upload window. Keep both specific `ProxyPass` rules before the
general route, and keep the relay `Location` limit after the general `Location`
so it overrides 16 KiB. Do not create a ModSecurity bypass. The `max`/`acquire`
worker parameters bound each Apache child's loopback wait; the gateway's
request/DB semaphores and aggregate
token bucket provide the process-wide fail-fast ceiling. Rebuild and validate
before a graceful reload:

```sh
/scripts/rebuildhttpdconf
/usr/local/apache/bin/apachectl -t
/scripts/restartsrv_httpd --graceful
curl --fail --silent --show-error -o /dev/null -w '%{http_code}\n' \
  https://pusula-backup.stronganchortech.com/healthz
```

Expected public status is `204`. Recheck the rest of the server health gate
after the Apache reload.

## Release update and rollback

1. Build and test with the commands in `README.md`; retain the old binary.
2. Copy the new binary beside the live path, verify its checksum and ownership,
   stop the service, atomically rename it into place, and start the service.
3. Confirm migration, service status, loopback/public `204`, then perform a test
   enrollment and a small encrypted upload through completion and status.
   Confirm the completed row has `version_id`, `verified_size_bytes`,
   `verified_sha256`, and `verified_at`, then exercise exact-version download
   into a new mode-`0600` recovery file.
4. If the binary fails before a migration, restore the old binary and restart.
   If a future release applies a schema migration, follow that release's
   explicit compatibility note; never improvise a SQLite down-migration.

Migrations 1 through 3 are additive and idempotent. Migration 2 adds the
persisted relay-attempt marker. Migration 3 backfills `retention_class`, adds
actual-body verification evidence, creates the persistent
`upload_authorizations` rolling-24-hour byte ledger, and adds
admission/cleanup indexes. Every newly issued direct URL, exact re-sign, and
admitted relay attempt consumes one device token plus the full reservation
size in that ledger. Expired ledger and stale pending cleanup are independently
bounded by their configured limits.
`schema_migrations` stores each immutable SQL checksum and refuses a modified
already-applied migration. Never edit an applied migration; add a new numbered
file and verify `SELECT version,name,checksum FROM schema_migrations ORDER BY
version;` after rollout. Checksums canonicalize CRLF/LF so the same immutable
SQL has the same evidence in Windows and AlmaLinux checkouts.

Migration 3 cannot retroactively prove bodies for rows completed by an older
metadata-only verifier. A pre-migration completed row without a nonempty exact
`version_id` and matching `verified_size_bytes`/`verified_sha256`/`verified_at`
is intentionally excluded from API latest status and root
list/lookup/download. Recover it through exact-version inventory and the
fail-closed procedure below; do not manufacture verification columns.

## Credential rotation and incident response

- Lost device: run `revoke-device` immediately, issue a new enrollment code,
  enroll again, and confirm an upload. Revocation does not delete prior B2
  objects.
- Exposed enrollment code: revoke its ID. If it was already consumed, revoke
  the resulting device instead.
- Exposed B2 key: revoke it in Backblaze, create another key with the same narrow
  bucket/prefix capabilities, update the root-only environment, restart, and
  complete a test upload. Desktop installations do not change.
- Exposed token pepper: replace it, restart, and re-enroll every device. All old
  device tokens and unused enrollment codes become invalid.
- Suspected presigned URL exposure: it is path-, header-, size-, and time-bound.
  Do not mark the backup complete; after 15 minutes, check for and quarantine
  any unexpected object/version with that backup ID. Completion hashes the
  actual downloaded body, so matching caller-supplied metadata alone cannot
  turn a mutated object into a completed record.

For a relay retry, first use completion to establish storage state. HTTP `409`
with `object_not_present` is the definitive missing-object result and leaves
the reservation pending/relayable. HTTP `404` with `not_found` means the
device-owned gateway binding is stale and must be re-reserved. HTTP `502`
means storage state or content is indeterminate/mismatched: reconfirm it and do
not issue a blind replacement PUT. The gateway performs its own authenticated
B2 precheck before relay admission/body ingress, charges the relay before
polling or spooling the body, then prechecks again immediately before PUT.
Transport failures and HTTP `408`/`429`/`5xx` PUT responses receive a
confirmation GET; only an authoritative `404` permits any later PUT attempt.

Do not log or paste secret-bearing CLI output, environment contents, bearer
tokens, or presigned URLs into tickets or repository documentation.

## Metadata protection and recovery

The SQLite database contains hashes and operational metadata, not plaintext
device tokens or backup bodies. Preserve it and the token pepper together in
the administrator's secure server backup. Use SQLite's online backup command or
stop the service before a filesystem copy; never copy only the main file while
WAL mode is active.

After restoring metadata:

1. restore owner `pusula-backup:pusula-backup` and state mode `0700`;
2. restore the root-only environment separately at mode `0600`;
3. run `migrate`, start the service, and verify health;
4. confirm an existing device can read status and upload a new encrypted test;
5. compare the latest completed IDs with the B2 prefix inventory.

For a normal recovery with intact metadata, use the root-only CLI. `list-backups`
and `lookup-backup` read only completed records with exact actual-body
verification evidence without running migrations;
`download-backup` refuses rows without a stored version ID, signs the exact
`versionId` query, creates a new output without replacement, and verifies the
streamed size/SHA-256 before leaving the file in place. Run it through a
root-owned transient unit as shown in `README.md`. Never paste its environment,
an object URL, or recovery private key into a command line or ticket.

### Gateway database loss: B2 inventory fallback

Loss of `gateway.sqlite3` requires device re-enrollment. Do **not** fabricate
completed rows or weaken `download-backup` to use the current object: the lost
database also contained the expected hash and exact completed version ID.
Instead:

1. Preserve the private bucket and lifecycle configuration; create an inventory
   of **all object versions** beneath `backups/rolling/`, `backups/daily/`, and
   `backups/monthly/` with trusted Backblaze/S3 administrator tooling. Record
   object key, version/file ID, upload time, and size in a protected incident
   artifact. Do not use a public URL or the runtime key in a ticket.
2. Use the key taxonomy (retention class, device UUID, UTC date, backup UUID)
   plus any surviving desktop/local-recovery manifest to choose a candidate.
   If one key has multiple versions, keep the exact IDs distinct; never assume
   the newest version is the verified one.
3. Download the chosen exact version with the trusted B2 administrator tool
   into a create-new file under a mode-`0700` directory. Verify its SHA-256
   against a surviving manifest/sidecar when available. If no independent hash
   survives, retain the inventory and treat decryption plus the restore harness
   as the recovery evidence rather than claiming gateway verification.
4. Move the ciphertext to the offline recovery workstation, decrypt only with
   the separately held age identity, and run the documented SQLite restore
   integrity/row-count/financial-total checks. The recovery identity must never
   be copied to this gateway or VPS.
5. Initialize fresh gateway metadata through normal migrations and
   re-enrollment. Leave prior B2 objects under lifecycle control; do not insert
   reconstructed rows into the new gateway database.

The root CLI intentionally cannot perform this fallback without metadata. That
fail-closed boundary prevents an operator from silently downloading a later,
unverified overwrite as though it were the completed backup.

## Monitoring

- Monitor systemd state, repeated restart loops, HTTP `5xx`, `429`/`503` volume, and
  absence of recent verified backup timestamps.
- Repeated relay traffic indicates that the customer network cannot use the
  preferred direct B2 route. Investigate it, but do not disable the encrypted
  fallback while it is the only working durability path.
- With no relay active, `/var/lib/pusula-backup-gateway/relay-spool` must be
  empty. A startup stale-spool cleanup warning means a prior relay was
  interrupted and its reservation still needs a verified retry.
- `/healthz` failure is warning-level. It must not page as a business-critical
  outage because the Windows app is deliberately local-first and must continue
  saving while disconnected.
- Alert separately when a device that normally connects has no verified remote
  backup within the agreed window. This is the actionable durability signal.
- B2 lifecycle and bucket encryption are external controls; audit them after
  any Backblaze configuration change.
