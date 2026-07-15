# Pusula encrypted backup and restore runbook

This is an administrator procedure. A restore replaces the single production
SQLite source of truth, so keep Pusula closed and preserve rollback artifacts.
Never decrypt customer data in a web root, repository, OneDrive, email, or a
shared support folder.

## Backup design

Pusula creates a consistent SQLite online-backup snapshot in memory, verifies
it, and streams the serialized bytes directly into age encryption. No plaintext
snapshot is written to a filesystem temporary file. Only the encrypted result
enters the persistent queue or network.

The Windows queue is:

```text
%LOCALAPPDATA%\com.stronganchor.pusula\backup-queue\
  backup-<UTC>-<uuid>-<rolling|daily|monthly>.sqlite3.age
  backup-<UTC>-<uuid>-<rolling|daily|monthly>.sqlite3.age.json
  backup-<UTC>-<uuid>-local-recovery.sqlite3.age
  backup-<UTC>-<uuid>-local-recovery.sqlite3.age.json
```

The JSON sidecar is nonsecret bookkeeping: format version, timestamp,
retention class, ciphertext size and SHA-256, schedule period, upload stage,
and optional gateway backup ID. It contains no device token, customer data,
recovery key, or filesystem path.

The desktop creates rolling backups for manual and pre-update protection.
Scheduled daily and monthly periods use the Windows local business date and
catch up after the computer has been off. While offline, the queue retains at
most 14 rolling, eight daily, and four monthly pending artifacts. A destructive
import also creates a `local-recovery` artifact that is never uploaded; the
newest three remain on the PC for rollback.

Every durable remote backup goes through the authenticated gateway at
`https://pusula-backup.stronganchortech.com`:

1. The desktop reserves the exact ciphertext length, SHA-256, and retention
   class. The response must select `transport: gateway_relay`.
2. Before sending bytes, the sidecar durably records `relay_pending` and the
   server-generated backup ID.
3. The desktop sends the raw `.age` ciphertext as the entire authenticated
   body of `PUT /v1/backups/relay/<backup-id>`.
4. The gateway bounds and hashes a private spool, checks free space, and
   publishes the complete mode-`0600` object with an atomic no-overwrite hard
   link on the VPS.
5. The gateway reopens and hashes the stored regular file, then records the
   exact size, SHA-256, verification time, and deterministic version
   `fs-sha256-<ciphertext SHA-256>`.
6. The desktop removes its queued copy only after the authenticated completion
   endpoint verifies that exact object.

The gateway receives neither SQLite plaintext nor the age recovery identity
and therefore cannot decrypt backups. No cloud-storage account, bucket, or API
key is involved.

An interrupted request never removes local ciphertext. After a timeout or a
lost success response, the desktop probes completion before retrying. A
verified completion removes the queue item; `409 object_not_present` retries
the same reservation; `404 not_found` clears only the stale server binding and
re-reserves the byte-identical ciphertext. This prevents ambiguous responses
from creating a second object.

The gateway relay spool contains only age ciphertext. It is private to the
service account, removed on ordinary success and error paths, and cleaned of
crash remnants before the listener starts. A definitive gateway `401` or `403`
deletes the rejected desktop token and shows **Yeniden kurulum gerekli** in
**VERİ VE YEDEK**; issue one new one-time enrollment code.

Malformed sidecars and ciphertext with a mismatched recorded size or SHA-256
are moved under `backup-queue\quarantine`. Do not delete quarantined evidence
until an administrator has investigated and obtained a new verified backup.

## Remote retention and verification

The gateway retains rolling backups for 14 days, daily backups for 60 days,
and monthly backups for 400 days. Cleanup is bounded and resumable and always
preserves the newest completed backup for each device and class. Do not delete
files directly from the object tree; use the gateway retention command.

On the VPS, require all of the following before calling a backup durable:

- the record is completed and reserved/verified size and SHA-256 match;
- `version_id` is exactly `fs-sha256-<sha256>`;
- the object is a regular mode-`0600` file under the generated object tree;
- the relay spool is empty after the request; and
- an exact-version `download-backup` re-hashes successfully.

See `gateway/RUNBOOK.md` for the root-only commands. The encrypted object tree
and gateway metadata must also be included together in the VPS administrator's
server-backup system. This is an off-machine encrypted copy, but not a complete
independent-provider 3-2-1 backup by itself.

## Recovery prerequisites

- The passphrase-encrypted Pusula recovery kit and its separately stored paper
  recovery sheet, prepared according to `RECOVERY_KEY_CUSTODY.md`.
- `rage.exe` from the pinned official rage package in the recovery kit.
- Python 3. The guarded restore script uses Python's standard-library SQLite
  support for read-only validation and online rollback backups.
- `sqlite3.exe` for integrity and reconciliation queries.
- Ciphertext copied from the Windows encrypted queue or downloaded from the
  VPS with `download-backup`.
- Expected row counts, integer-kuruş totals, schema version, ciphertext size,
  and SHA-256 from the recovery record.

Use a clean administrator workstation or designated replacement PC. The age
identity must not be copied to the customer computer for normal operation and
must never be pasted into a shell transcript or ticket.

## Stage and validate

1. Close Pusula. In Task Manager, require no `pusula-desktop.exe` process.
2. Create an access-controlled directory outside every sync root, for example
   `C:\Pusula-Restore\<incident-id>`.
3. Verify ciphertext before decrypting:

   ```powershell
   Get-FileHash -Algorithm SHA256 -LiteralPath 'C:\secure\backup.sqlite3.age'
   (Get-Item -LiteralPath 'C:\secure\backup.sqlite3.age').Length
   ```

   Both values must match the gateway record or queue sidecar.
4. Decrypt to a new staging file, never the live database:

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

7. Compare all counts and totals with the recorded source evidence. Investigate
   every mismatch before replacement.

## Guarded replacement

1. Keep Pusula closed and disconnect the replacement PC from the network.
2. The live database is only:

   ```text
   %LOCALAPPDATA%\com.stronganchor.pusula\data\pusula.sqlite3
   ```

3. Use `scripts\Restore-PusulaBackup.ps1` as described in
   `RESTORE_HARNESS.md`; do not replace files manually. Supply exact
   `-RagePath` and `-PythonPath` values when the tools are not on `PATH`.
4. The app and restore harness use the same exclusive database lock. The
   harness writes `.pusula-restore-in-progress.json` before detaching SQLite
   sidecars or replacing the database. Pusula refuses to start while that
   marker exists.
5. The harness creates and verifies an online rollback, performs atomic
   replacement, and reopens the result. If interrupted, keep Pusula closed and
   run:

   ```powershell
   .\scripts\Restore-PusulaBackup.ps1 -RecoverInterruptedRestore
   ```

   Pass the same custom target, staging, or rollback roots used by the original
   restore. Never delete the marker just to make the app start.
6. Start the same or newer compatible Pusula version. Open **VERİ VE YEDEK**
   and require a healthy integrity result plus matching counts and totals.
7. Complete the disconnected workflow drill, reconnect, re-enroll if needed,
   and require a new verified encrypted remote backup.

If any check fails, close Pusula, preserve the failed files, restore the
validated rollback database, and escalate. Never merge two independently
edited Pusula databases.

## Key-loss and device-loss response

- Lost computer: revoke its gateway device ID, restore the latest verified
  backup to the replacement PC, and issue a new one-time enrollment.
- Exposed device token: revoke the device and re-enroll. No server credential
  is present on the desktop.
- Exposed enrollment code: revoke it; if consumed, revoke the resulting device.
- Exposed age identity: treat retained ciphertext as decryptable, ship a new
  recipient in a controlled desktop update, and preserve old identity access
  until old retention is reconciled.
- Lost recovery sheet or encrypted recovery kit: preserve any surviving live
  SQLite database and follow `RECOVERY_KEY_CUSTODY.md`. If all identity copies
  are lost, existing encrypted backups cannot be decrypted.
