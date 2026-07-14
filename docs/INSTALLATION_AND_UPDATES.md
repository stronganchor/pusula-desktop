# Pusula Desktop Installation and Updates

Pusula Desktop is installed for one Windows user on one designated Windows
10/11 x64 computer. It does not need WordPress, a local web server, a browser
login, or administrator rights. The local SQLite database remains usable when
every network adapter is disconnected.

## Initial installation

1. Obtain `Pusula_<version>_x64_offline-setup.exe` from the private handoff or
   the official `stronganchor/pusula-desktop` GitHub release.
2. In File Explorer, open **Properties -> Digital Signatures** and require a
   valid Strong Anchor publisher signature and timestamp. Stop if Windows
   reports an unknown publisher, an invalid signature, or no signature.
3. Disconnect the computer from the internet for the offline-install test.
4. Run the installer. The offline package includes the WebView2 runtime and
   installs only for the current Windows user.
5. Start Pusula. Import the final WordPress JSON export, or use the twice-
   confirmed empty start only for a genuinely new database.
6. Complete `OFFLINE_ACCEPTANCE_TEST.md` before entering production records.

The database and encrypted backup queue live under the current user's local
Pusula application-data directory. Do not move, rename, synchronize, or edit
those files manually. OneDrive and shared network folders are not supported as
the live database location.

## Enable encrypted off-machine backup

An administrator issues a short-lived, one-time enrollment code. With the
internet connected, open **VERİ VE YEDEK**, enter that code and a recognizable
computer name, and select **YEDEĞİ ETKİNLEŞTİR**. The code is discarded after
enrollment and the device credential is stored in Windows Credential Manager.

Select **ŞİMDİ YEDEKLE** once and require **Uzak yedek doğrulandı**. The desktop
always creates and durably queues an age-encrypted SQLite snapshot before it
tries the network. If the gateway is unavailable, local business writes keep
working and the ciphertext is retried automatically after reconnection.

The age recovery private key is not on the customer computer or gateway. Keep
at least two access-controlled administrator copies. Losing every copy makes
the encrypted off-machine backups unrecoverable.

## Normal signed updates

Pusula checks the official GitHub update manifest shortly after launch and
every six hours while it is running. No internet connection is required until
an update is available.

Tauri downloads the lean Authenticode-signed NSIS installer directly and
verifies its detached `.exe.sig` updater signature before Pusula asks whether
to install. This is the same `Pusula_<version>_x64-setup.exe` published with the
release; it is not wrapped in or extracted from an updater ZIP. If accepted,
the Rust backend waits for active database operations, blocks new business
writes, and creates a consistent encrypted snapshot while that exclusive
maintenance gate remains held. Only then is the verified installer run and the
app relaunched. A signature, backup, or installer failure stops the appropriate
phase; an install failure releases the maintenance gate so the current version
can continue. The larger `Pusula_<version>_x64_offline-setup.exe` remains a
separate disconnected-install option containing the offline WebView2 runtime.
The release workflow separately refuses to publish installers whose Windows
publisher signature or timestamp is invalid.

For remote support, the operator normally only needs to connect the computer
to the internet, leave Pusula open, and approve the update prompt. If the
in-app route is unavailable, the signed offline installer can be run over the
existing installation; take and verify a backup first.

The Windows installer refuses to replace Pusula with an older version. SQLite
schema migrations move forward, so an older retained installer must never be
used as an application rollback against a database opened by a newer release.
Recover records through the documented backup/restore procedure and install a
same-or-newer signed version instead.

## Before repair, replacement, or uninstall

1. Open **VERİ VE YEDEK** and require a fresh verified remote backup.
2. Create a manual JSON export to approved encrypted removable media.
3. Record the application version, database counts/totals, export SHA-256, and
   latest gateway backup ID in the private handoff worksheet.
4. Follow `BACKUP_RESTORE_RUNBOOK.md` for a replacement computer. Never create
   records independently on both old and replacement computers.

Do not rely on uninstall behavior as a backup policy. Preserve verified
exports and encrypted off-machine backups before changing the Windows profile
or application installation.
