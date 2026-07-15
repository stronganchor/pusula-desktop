# Pusula backup gateway operations

The gateway stores only age-encrypted Pusula snapshots. The Windows SQLite
database remains the production source of truth. Gateway downtime, a full VPS
disk, or an HTTPS failure must be reported as delayed backup and must never be
treated as a reason to block local Pusula writes.

## Install without activation

1. Create the non-login account and private state directory:

   ```sh
   useradd --system --home-dir /var/lib/pusula-backup-gateway \
     --shell /sbin/nologin pusula-backup
   install -d -o root -g root -m 0755 /usr/local/lib/pusula-backup-gateway
   install -d -o root -g root -m 0755 /usr/local/share/doc/pusula-backup-gateway
   install -d -o pusula-backup -g pusula-backup -m 0700 /var/lib/pusula-backup-gateway
   ```

2. Install the controlled AlmaLinux release binary as `root:root 0755`, copy
   the unit and docs, and verify their recorded source/SHA evidence.

3. Create `/etc/pusula-backup-gateway.env` as `root:root 0600`. The only secret
   is `PUSULA_GATEWAY_TOKEN_PEPPER`; generate at least 32 random bytes and never
   print or record the production value in a ticket or repo. No bucket or
   external cloud-storage credential is used.

4. Keep both paths beneath the same state filesystem:

   ```text
   PUSULA_GATEWAY_DATABASE=/var/lib/pusula-backup-gateway/gateway.sqlite3
   PUSULA_GATEWAY_OBJECT_ROOT=/var/lib/pusula-backup-gateway/objects
   ```

   Atomic immutable publication relies on a hard link from the private relay
   spool to the object tree. A cross-filesystem object root is unsupported and
   fails closed.

5. Install the Apache userdata includes. Only `/v1/backups/relay/` receives the
   256 MiB request limit and 900-second upload window. Keep its specific
   `ProxyPass` before the catch-all route. Rebuild Apache configuration and
   require `apachectl -t` success before reload.

6. Run migration while the service remains stopped:

   ```sh
   /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway migrate
   systemctl is-enabled pusula-backup-gateway.service
   systemctl is-active pusula-backup-gateway.service
   ```

   Disabled staging must still read `disabled` and `inactive`.

## Activation and smoke test

Only after the release checklist authorizes activation:

```sh
systemctl daemon-reload
systemctl enable --now pusula-backup-gateway.service
systemctl status --no-pager pusula-backup-gateway.service
ss -lntp | grep 127.0.0.1:12741
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:12741/healthz
curl -fsS -o /dev/null -w '%{http_code}\n' https://pusula-backup.stronganchortech.com/healthz
```

Both health requests should return `204`; there must be no non-loopback gateway
listener. Health is minimal process liveness and is not backup durability proof.

Create one short-lived enrollment code as the service account, enter it once
in the installed desktop, and do not preserve the code afterward:

```sh
(
  set -a
  . /etc/pusula-backup-gateway.env
  set +a
  exec runuser -u pusula-backup --preserve-environment -- \
    /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway \
    issue-enrollment --label 'Initial Windows PC' --expires-hours 1
)
```

The protected file is loaded only inside a subshell, the literal pepper never
enters shell history or process arguments, and the CLI runs as the service user
so it cannot leave root-owned SQLite side files.

## Prove a completed backup

After the desktop reports success:

```sh
pusula-backup-gateway list-backups --limit 10
pusula-backup-gateway lookup-backup --backup-id UUID
find /var/lib/pusula-backup-gateway/relay-spool -mindepth 1 -maxdepth 1 -print
```

Require:

- a completed record with matching reserved and verified size/SHA-256;
- `version_id` exactly `fs-sha256-<sha256>`;
- a non-null verification time in the acceptance interval;
- an empty relay spool;
- a regular mode-`0600` object below the expected class/device/date key;
- no symlink anywhere in the object path.

Independently download and hash the exact record:

```sh
install -d -o root -g root -m 0700 /root/pusula-recovery
pusula-backup-gateway download-backup \
  --backup-id UUID \
  --output /root/pusula-recovery/backup.sqlite3.age
sha256sum /root/pusula-recovery/backup.sqlite3.age
```

The CLI refuses overwrite and re-hashes the stored object. Move ciphertext to
the recovery workstation, then follow the desktop restore harness. Never put
the age recovery identity on this VPS.

## Retention operations

Retention defaults are rolling 14 days, daily 60 days, and monthly 400 days.
Each run handles at most 100 objects and preserves the newest completed object
for every device/class. It runs at startup and before authenticated reservation.

Manual bounded run (stop the listener first so stale-pending cleanup cannot
race an active relay):

```sh
systemctl stop pusula-backup-gateway.service
runuser -u pusula-backup -- \
  /usr/local/lib/pusula-backup-gateway/pusula-backup-gateway prune-storage
systemctl start pusula-backup-gateway.service
```

An interrupted cleanup is visible in the private metadata table and resumes
automatically. Do not use `find -delete`, a generic cleanup cron, or manual
directory pruning: those bypass the database claim and can make status and
recovery evidence false.

Monitor free space, not just object age:

```sh
df -h /var/lib/pusula-backup-gateway
du -sh /var/lib/pusula-backup-gateway/objects
```

Ingress is rejected before reading a body unless available space covers the
full reservation plus the configured 1 GiB safety floor. A capacity failure
leaves the desktop ciphertext queued for retry.

## Upgrade and rollback

1. Preserve the current binary, unit, docs, environment, metadata database, and
   object tree. Never expose the environment contents in the handoff.
2. Stop the service and verify no gateway process or relay spool remains.
3. Install the exact candidate binary and docs, run `migrate`, and confirm
   `PRAGMA integrity_check` and migration checksums.
4. Start the service, verify loopback/public `204`, then exercise status and one
   encrypted relay from the installed desktop.
5. Confirm the new object and exact-version recovery download before removing
   the rollback binary.

Schema migration 4 adds `storage_purges`. It does not rewrite completed backup
rows or ciphertext objects. Do not edit an applied migration; ship another
numbered migration.

For rollback after migration 4, the older cloud-dependent binary is not a valid
production fallback because it requires removed credentials and cannot serve
local objects. Roll forward with a corrected local-storage build. Keep the
service stopped if object/metadata invariants cannot be proven.

## Metadata and object protection

Protect these together in the administrator's server-backup system:

- `/var/lib/pusula-backup-gateway/gateway.sqlite3` (use SQLite online backup);
- `/var/lib/pusula-backup-gateway/objects/`;
- the stable token pepper in `/etc/pusula-backup-gateway.env`.

The database contains no plaintext customer records or bearer tokens, but it
maps authenticated devices to recovery-authoritative object hashes and paths.
Changing/loss of the pepper requires device reenrollment. Loss of the database
does not decrypt objects but removes authoritative version mappings.

If metadata is lost, do not fabricate completed rows. Inventory regular files
under the three retention prefixes, hash ciphertext candidates, correlate their
server-generated device/date/backup key with any surviving desktop sidecar or
acceptance record, and restore only through the offline recovery harness. Build
a fresh gateway database and reenroll the device.

## Incident responses

- **Device token exposed:** revoke the device, issue a new one-time enrollment,
  and reenroll. Prior encrypted objects remain under retention.
- **Token pepper exposed:** stop the service, replace the pepper, revoke/recreate
  all enrollment/device credentials through controlled reenrollment, and record
  the incident without the secret value.
- **Relay interrupted:** confirm the mode-`0600` spool is removed. Startup removes
  only `.relay-*.sqlite3.age.part` crash remnants before binding.
- **Object exists but completion failed:** never overwrite it. Retry completion;
  the gateway hashes the exact immutable file and commits only if it matches.
- **Checksum/size conflict at an object key:** stop activation and preserve the
  object, metadata database, and logs. Do not rename another body into its key.
- **Low disk:** free unrelated approved space or expand the filesystem. Do not
  weaken the safety floor or manually delete Pusula objects.
- **Object-root symlink or wrong type:** stop the service and investigate. The
  gateway intentionally rejects symlinks and non-regular objects.
- **Recovery identity exposed:** this is separate from gateway credentials.
  Replace the desktop recovery recipient through a controlled application
  release and preserve old identity access for retained historical objects.

## Routine monitoring

- public and loopback health remain `204`;
- service stays bound only to loopback;
- latest verified backup timestamp advances within the agreed window;
- relay spool is empty when no upload is active;
- object filesystem has ample free space above the configured reserve;
- retention cleanup has no repeatedly unfinished claim;
- gateway logs contain no persistent verification, capacity, or permission
  failures.
