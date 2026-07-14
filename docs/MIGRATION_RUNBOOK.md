# Pusula Lite to Pusula Desktop Migration

This is a one-way cutover from the WordPress Pusula Lite database to one
Windows PC running Pusula Desktop. After acceptance, the local SQLite database
is the only production source of truth. Pusula Desktop does not synchronize
changes back to WordPress.

Customer exports contain personal and financial data. Keep them outside web
roots, repositories, shared folders, and email. Delete transfer copies after
the migration and retain only the approved protected archive.

## 1. Prepare and freeze WordPress

1. Confirm that the current Pusula Lite interface and database are healthy.
2. Take a complete WordPress/database backup and verify that it can be restored.
3. Arrange a short cutover window and stop entering records in Pusula Lite.
4. Keep that write freeze in place through export and desktop acceptance.

The exporter is opt-in and does not add a WordPress route. Use the version in
the `stronganchor/pusula-lite` repository at
`tools/pusula-desktop-export.php`. Run it with WP-CLI on the WordPress host,
using a destination outside the public web root:

```bash
wp --path=/path/to/wordpress \
  --require=/secure/path/pusula-desktop-export.php \
  pusula desktop-export \
  --output=/secure/path/pusula-desktop-export.json \
  --dry-run
```

The dry run writes no customer data. Record its row counts, financial totals in
kuruş, and SHA-256 value in the private migration worksheet. If it reports an
orphan, duplicate ID, invalid amount, or database error, stop and repair or
explain the source data before continuing.

Create the final file only after the dry run succeeds:

```bash
wp --path=/path/to/wordpress \
  --require=/secure/path/pusula-desktop-export.php \
  pusula desktop-export \
  --output=/secure/path/pusula-desktop-export.json
```

An existing destination is never overwritten unless `--force` is explicitly
supplied. The file is written to a temporary sibling and renamed only after the
entire export succeeds.

## 2. Install and import

1. Install the Authenticode-signed `Pusula_<version>_x64_offline-setup.exe` on
   the designated Windows PC. The offline installer includes WebView2.
2. Copy the final JSON export to that PC using an approved encrypted transfer.
3. Start Pusula. On the first-run screen select **JSON İÇE AKTAR** and choose the
   export.
4. Wait for the success message. The application validates the format, source,
   checksum, row counts, financial totals, IDs, dates, and all relationships
   before beginning one SQLite transaction. Any failure leaves the empty local
   database unchanged.
5. Open **VERİ VE YEDEK** and compare the desktop row counts with the recorded
   export summary. Require all five counts, all three integer-kuruş totals, and
   the persisted final SHA-256 to match.

Do not choose **BOŞ BAŞLA** for a migration. That choice exists only for a
genuinely new installation and requires a second confirmation.

## 3. Acceptance drill

Before allowing production entry, complete the checklist in
`OFFLINE_ACCEPTANCE_TEST.md`. At minimum:

- compare representative customers, contacts, sales, installments, and
  payments with WordPress;
- compare total sales, installment amounts, and payments with the export;
- print one sale receipt and one payment receipt;
- disconnect every network adapter and complete a new sale, installment
  payment, report, and restart; and
- create an encrypted backup, restore it into the test profile, and compare
  counts and financial totals.

If acceptance fails, stop using the desktop database, preserve its files for
diagnosis, and return temporarily to the frozen WordPress backup. Never merge
records entered independently in both systems.

## 4. Cut over

1. Record the acceptance time and final export checksum.
2. Mark WordPress Pusula Lite read-only or remove operator access to its entry
   interface. Keep its backup for the agreed retention period.
3. Enroll the desktop backup client and confirm that an encrypted object reaches
   Backblaze B2.
4. Securely delete temporary JSON copies from the server and transfer media.
5. Resume entry only in Pusula Desktop.

After the first production write in Pusula Desktop, the WordPress export is no
longer a valid rollback source.

## Manual JSON export and replacement

**VERİ VE YEDEK → JSON DIŞA AKTAR** creates the same checksummed versioned
bundle for support or recovery. It does not require internet access.

Manual import into a populated desktop database is destructive by design. The
operator must type `DEĞİŞTİR`. Before replacement begins, the Rust backend
must create and durably retain a consistent age-encrypted `local-recovery`
snapshot. This succeeds offline and cannot be bypassed by the interface or
removed by a successful remote upload. The candidate file is then fully
validated and all records are replaced in one transaction while normal app
writes remain locked out. If snapshot creation, validation, or the transaction
fails, replacement does not begin or the current database remains intact. The
maintenance screen keeps the last import counts, totals, and checksum for
reconciliation.
