# Pusula Encrypted Backup and Restore Runbook

This is an administrator procedure. A restore replaces the single production
SQLite source of truth, so keep Pusula closed and preserve rollback artifacts.
Never decrypt customer data in a web root, repository, OneDrive, email, or a
shared support folder.

## Backup design

Pusula creates a consistent SQLite online-backup snapshot in a temporary file
and encrypts it with the fixed age X25519 recovery recipient before anything
enters the persistent queue or is sent to the network. The temporary plaintext
snapshot is removed when encryption finishes. The Windows queue is:

```text
%LOCALAPPDATA%\com.stronganchor.pusula\backup-queue\
  backup-YYYYMMDDTHHMMSSZ-<uuid>-<rolling|daily|monthly>.sqlite3.age
  backup-YYYYMMDDTHHMMSSZ-<uuid>-<rolling|daily|monthly>.sqlite3.age.json
```

The JSON sidecar is nonsecret bookkeeping: format version, timestamp,
retention class, ciphertext size/SHA-256, and optional upload stage/backup ID.
It contains no token, presigned URL, object key, customer data, or local path.

The desktop creates rolling backups for pre-operation and manual protection and
uses daily/monthly retention classes for scheduled protection. While offline,
the local queue keeps at most 14 rolling, eight daily, and four monthly pending
artifacts. A local artifact is removed only after the gateway verifies its
remote object. The scheduler wakes every six hours and retries queued
ciphertext on the next due pass after reconnection. Backblaze and the gateway
never possess the age private identity and cannot decrypt a backup.

## Recovery prerequisites

- An access-controlled copy of `pusula-recovery.agekey`, obtained through the
  approved administrator key-custody channel.
- `rage.exe` from the official rage project.
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

3. If a target exists, use SQLite's online `.backup` command to create a
   timestamped rollback database outside the live `data` directory, then run
   the same integrity/count/total checks on that rollback.
4. Move the old database plus any `-wal` and `-shm` siblings into the protected
   incident rollback directory. Do not delete them.
5. Copy the validated staged database to a temporary sibling in the live
   `data` directory, then rename it to `pusula.sqlite3`.
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
