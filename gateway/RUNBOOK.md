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
   Confirm the B2 key is bucket- and prefix-restricted before starting.
4. Run `systemctl daemon-reload`, then
   `systemctl enable --now pusula-backup-gateway.service`. The unit creates the
   state directory and runs migrations before every start.
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
Do not create a ModSecurity bypass. Rebuild and validate before a graceful
reload:

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
4. If the binary fails before a migration, restore the old binary and restart.
   If a future release applies a schema migration, follow that release's
   explicit compatibility note; never improvise a SQLite down-migration.

The initial migration is additive and idempotent. `schema_migrations` stores a
checksum and refuses a modified already-applied migration.

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
  any unexpected object with that backup ID.

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

Loss of the metadata database requires re-enrollment. Existing encrypted B2
objects remain recoverable by an administrator holding the separate age
recovery private key; that key must never be placed on this gateway.

## Monitoring

- Monitor systemd state, repeated restart loops, HTTP `5xx`, `429` volume, and
  absence of recent verified backup timestamps.
- `/healthz` failure is warning-level. It must not page as a business-critical
  outage because the Windows app is deliberately local-first and must continue
  saving while disconnected.
- Alert separately when a device that normally connects has no verified remote
  backup within the agreed window. This is the actionable durability signal.
- B2 lifecycle and bucket encryption are external controls; audit them after
  any Backblaze configuration change.
