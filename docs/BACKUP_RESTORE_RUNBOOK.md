# Pusula Encrypted Backup and Restore Runbook

This is an administrator procedure. A restore replaces the single production
SQLite source of truth, so keep Pusula closed and preserve rollback artifacts.
Never decrypt customer data in a web root, repository, OneDrive, email, or a
shared support folder.

## Backup design

Pusula creates a consistent SQLite online-backup snapshot in an in-memory
SQLite connection, verifies it, and streams its serialized bytes directly into
age encryption. No plaintext snapshot is written to a filesystem temporary
file. Only the encrypted result enters the persistent queue or network. The
Windows queue is:

```text
%LOCALAPPDATA%\com.stronganchor.pusula\backup-queue\
  backup-YYYYMMDDTHHMMSSZ-<uuid>-<rolling|daily|monthly>.sqlite3.age
  backup-YYYYMMDDTHHMMSSZ-<uuid>-<rolling|daily|monthly>.sqlite3.age.json
  backup-YYYYMMDDTHHMMSSZ-<uuid>-local-recovery.sqlite3.age
  backup-YYYYMMDDTHHMMSSZ-<uuid>-local-recovery.sqlite3.age.json
```

The JSON sidecar is nonsecret bookkeeping: format version, timestamp,
retention class, ciphertext size/SHA-256, optional local-business schedule
period, and optional upload stage/backup ID.
It contains no token, presigned URL, object key, customer data, or local path.

The desktop creates rolling backups for manual and pre-update protection. The
pre-update path creates and flushes only a local encrypted snapshot; it never
contacts the gateway while business writes are held, and the existing queue is
uploaded after relaunch. Scheduled daily and monthly periods use the Windows
local business date. The scheduler catches up a monthly period even when the
computer was off on the first day of the month, and the durable period marker
plus queue sidecar permit at most one scheduled artifact per period. While offline,
the local queue keeps at most 14 rolling, eight daily, and four monthly pending
artifacts. A normal queued artifact is removed only after the gateway verifies
its remote object. A destructive import creates a separate `local-recovery`
artifact that is never uploaded or removed by queue flushing; the newest three
are retained on the PC for rollback. The scheduler wakes every six hours;
remote retry passes flush existing ciphertext only and do not manufacture
additional daily or monthly snapshots for the same period.
Backblaze and the gateway never possess the age private identity and cannot
decrypt a backup.

The desktop normally uploads an artifact directly to the single presigned B2
object URL returned for its reservation. If that direct `PUT` fails before an
HTTP response because of a connection, TLS, transport, or timeout error, the
desktop retries the **same reservation and backup ID** through the fixed Pusula
gateway at `PUT /v1/backups/relay/<backup-id>`. The relay request has the device
bearer token, `application/octet-stream`, the reservation's exact ciphertext
length, and the raw `.age` bytes as its entire body. It never contains the
SQLite plaintext or the age recovery identity. The gateway enforces its
configured backup-size ceiling, spools with a hard reservation-size bound,
verifies the reserved SHA-256, uploads to B2, and marks the reservation complete
only after remote verification.

Direct and relay upload requests have a bounded 15-minute total timeout aligned
with Apache's 900-second relay window. A timeout never removes the local
ciphertext; it remains queued for the next eligible retry. The gateway relay
spool contains only age ciphertext, is private to the service account, is
removed on normal success/error paths, and is cleaned of crash-left relay parts
at the next startup before any request is accepted.

A failed relay remains in the sidecar as `relay_pending` with the same backup
ID and survives restart. Before any repeated direct PUT or relay, the desktop
asks the authenticated completion endpoint to verify the exact stored object.
A verified `200` must include the canonical backup ID, completion timestamp,
and a nonempty object version ID before the local queue item is removed. A
gateway `409 object_not_present` keeps the same reservation and permits its
intended PUT/relay retry; `404 not_found` alone clears the stale binding and
re-reserves the byte-identical ciphertext. A `502 storage_verification_failed`
is indeterminate, so the sidecar stage is preserved and Pusula confirms again
later without issuing another PUT. This prevents ambiguous direct or lost relay
responses from creating a second B2 object version.

The desktop does **not** relay after B2 returns an HTTP status, and it does not
use relay to bypass a malformed reservation, failed authentication,
size/checksum mismatch, or another gateway policy response. A definitive
gateway `401` or `403` deletes the rejected device token and shows **Yeniden
kurulum gerekli** in **VERİ VE YEDEK**; issue a new one-time enrollment code.
An object-store HTTP status is not treated as a gateway device rejection.

Malformed sidecars are isolated under `backup-queue\quarantine` and rebuilt
when safe; ciphertext whose recorded size or SHA-256 no longer matches is also
quarantined. The maintenance screen reports this degraded state. Do not delete
quarantined evidence until an administrator has investigated and obtained a
new verified backup.

## Recovery prerequisites

- An access-controlled copy of `pusula-recovery.agekey`, obtained through the
  approved administrator key-custody channel.
- `rage.exe` from the official rage project.
- Python 3. The guarded restore script uses Python's standard-library SQLite
  support for read-only validation and SQLite online rollback backups. If
  `python.exe` is not on `PATH`, pass its exact local path with `-PythonPath`.
- `sqlite3.exe` from SQLite for integrity and reconciliation queries.
- The ciphertext and its recorded SHA-256/size, downloaded from the private B2
  bucket or copied from the local encrypted queue.
- Expected row counts, financial totals, and application/schema version from
  the release or incident worksheet.

Use a clean administrator workstation or the designated replacement PC. The
recovery identity must not be copied to the customer computer for normal
operation and must never be pasted into a shell transcript or ticket.

## Stage and validate

1. Close Pusula. In Task Manager, require no `pusula-desktop.exe` process.
2. Create a new access-controlled local staging directory outside every sync
   root, for example `C:\Pusula-Restore\<incident-id>`.
3. Verify ciphertext before decrypting:

   ```powershell
   Get-FileHash -Algorithm SHA256 -LiteralPath 'C:\secure\backup.sqlite3.age'
   (Get-Item -LiteralPath 'C:\secure\backup.sqlite3.age').Length
   ```

   Both values must match the gateway/B2 metadata or the queue sidecar.
4. Decrypt to a **new** file. Never target the live database:

   ```powershell
   rage -d `
     -i 'C:\secure\pusula-recovery.agekey' `
     -o 'C:\Pusula-Restore\incident-id\pusula-restored.sqlite3' `
     'C:\secure\backup.sqlite3.age'
   ```

5. Run structural checks:

   ```powershell
   sqlite3 'C:\Pusula-Restore\incident-id\pusula-restored.sqlite3' `
     'PRAGMA integrity_check; PRAGMA foreign_key_check; PRAGMA user_version;'
   ```

   Require `integrity_check` = `ok`, no foreign-key rows, and a schema version
   supported by the candidate Pusula build.
6. Query reconciliation evidence:

   ```powershell
   sqlite3 -header -column 'C:\Pusula-Restore\incident-id\pusula-restored.sqlite3' @'
   SELECT
     (SELECT COUNT(*) FROM customers) AS customers,
     (SELECT COUNT(*) FROM contacts) AS contacts,
     (SELECT COUNT(*) FROM sales) AS sales,
     (SELECT COUNT(*) FROM installments) AS installments,
     (SELECT COUNT(*) FROM installment_payments) AS payments,
     (SELECT COALESCE(SUM(total_kurus), 0) FROM sales) AS sales_kurus,
     (SELECT COALESCE(SUM(amount_kurus), 0) FROM installments) AS installments_kurus,
     (SELECT COALESCE(SUM(amount_kurus), 0) FROM installment_payments) AS payments_kurus;
   '@
   ```

7. Compare all five counts and three integer-kuruş totals with the recorded
   source evidence. Investigate every mismatch before replacement.

## Guarded replacement

1. Keep Pusula closed and disconnect the replacement PC from the network.
2. Locate the target only under:

   ```text
   %LOCALAPPDATA%\com.stronganchor.pusula\data\pusula.sqlite3
   ```

3. Follow `RESTORE_HARNESS.md` and use `scripts\Restore-PusulaBackup.ps1`; do
   not replace the files manually. Supply `-RagePath` and `-PythonPath` when
   those tools are not on `PATH`.
   The app and restore script acquire the same exclusive
   `.pusula-database.lock` file before SQLite access, closing the launch-time
   race that a process-name check alone cannot prevent.
4. Immediately before detaching WAL/SHM or replacing the base, the harness
   durably writes `.pusula-restore-in-progress.json` beside the database.
   Pusula refuses to start while that marker exists, shows the native
   **Pusula başlatılamadı** dialog with the safe marker-path guidance, and exits
   without opening SQLite.
5. The harness creates and verifies a SQLite-online rollback, isolates old
   sidecars, performs the atomic replacement, and reopens the result. It clears
   the marker only after verified success or after a verified automatic
   rollback. If the marker remains after a crash or error, keep Pusula closed
   and run the explicit recovery mode below. It strictly validates the recorded
   rollback hash/schema/counts/totals, restores that exact database (or removes
   a partial clean-machine target), and removes recorded plaintext staging and
   candidate artifacts before clearing the marker. Never delete the marker
   merely to make the app start.

   ```powershell
   .\scripts\Restore-PusulaBackup.ps1 -RecoverInterruptedRestore
   ```

   If the original restore used custom target, staging, or rollback roots, pass
   those same paths to recovery. Normal validation/apply runs refuse to
   overwrite an existing marker.
6. Start the same or newer compatible Pusula version. Open **VERİ VE YEDEK** and
   require a healthy integrity result plus matching counts/totals.
7. Complete the disconnected workflow drill, reconnect, re-enroll the device if
   necessary, and require a new encrypted remote backup.

If any check fails, close Pusula, preserve the failed files, restore the
validated rollback database, and escalate. Never merge two independently
edited Pusula databases.

## Key-loss and device-loss response

- Lost computer: revoke its gateway device ID, restore the latest verified
  backup to the designated replacement, and issue a new one-time enrollment.
- Exposed device token: revoke the device and re-enroll. Backblaze credentials
  are not present on the desktop.
- Exposed enrollment code: revoke it; if consumed, revoke the resulting device.
- Exposed age private identity: treat all retained ciphertext as decryptable,
  establish a new recipient, ship a desktop update, and re-encrypt future
  backups. Do not delete the old identity until retention and restore policy
  have been reconciled.
- All recovery identities lost: existing encrypted backups are unrecoverable.
  Preserve any surviving live SQLite database and escalate immediately.
