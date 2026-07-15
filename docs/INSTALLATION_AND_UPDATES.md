# Installation and updates

Pusula is installed for one Windows user and stores its production SQLite
database on that computer. The full installer contains the WebView2 runtime and
works without internet access. Internet is needed only for optional updates and
encrypted off-machine backup.

## First installation on the managed computer

Use only the stable GitHub release at:

<https://github.com/stronganchor/pusula-desktop/releases/latest>

1. Download `Pusula_<version>_x64_offline-setup.exe` and
   `SHA256SUMS.txt` from the same release.
2. Open PowerShell in the download folder and run:

   ```powershell
   Get-FileHash -Algorithm SHA256 .\Pusula_<version>_x64_offline-setup.exe
   Get-Content .\SHA256SUMS.txt
   ```

3. Confirm the displayed installer hash exactly matches the line for that
   filename. Stop if it does not.
4. Double-click the offline installer.
5. Windows may display **Windows protected your PC** because this one-machine
   installer is intentionally not Authenticode signed. Confirm that the shown
   filename is the file just hashed, select **More info**, then select
   **Run anyway**.
6. Complete the current-user installation. Do not install a certificate, start
   PowerShell as administrator, or disable SmartScreen.
7. Start Pusula while disconnected from the network and complete the first-run
   blank-start or import choice.

That SmartScreen acknowledgement is the only expected manual trust step.
If Windows shows a different filename, says the file is corrupt, or prevents
the hash check, stop and obtain a fresh copy from the official release.

## Why the installer says Unknown publisher

The initial one-machine release deliberately avoids Azure Artifact Signing and
a self-signed certificate. A self-signed certificate is also untrusted on a new
computer and would require adding a permanent local trust anchor. The
first-install SHA-256 check, official immutable GitHub release, and controlled
handoff identify the initial bytes.

This exception applies only to the managed Pusula installer from the official
release. It is not permission to bypass SmartScreen for other programs.

## Normal in-app updates

Pusula checks:

```text
https://github.com/stronganchor/pusula-desktop/releases/latest/download/latest.json
```

The update check never blocks customer, sale, installment, payment, report, or
receipt work. If the computer is offline, Pusula continues normally and checks
again after connectivity returns.

When an update is available:

1. Pusula downloads the direct lean NSIS installer.
2. Tauri verifies the mandatory detached signature with the public key embedded
   in the installed application.
3. Pusula creates and verifies an encrypted pre-update snapshot. Failure stops
   the update without blocking normal local work.
4. The user confirms the Pusula update.
5. The current-user passive installer runs and relaunches Pusula.

The acceptance gate requires this path to complete without importing a
certificate and without another Windows/SmartScreen prompt. Tauri writes the
verified updater to its own temporary directory rather than using a
browser-downloaded file. If a future Windows policy does display a prompt, do
not weaken Windows security; record it and use the full offline installer as
the managed fallback until the release procedure is updated.

The updater signature is separate from Authenticode and cannot be disabled.
The release workflow independently verifies the same `.exe.sig` with pinned
Minisign and refuses to publish an installer whose Authenticode status is
anything other than the documented `NotSigned` state.

## Offline update fallback

If the in-app route is unavailable:

1. On a connected trusted computer, download the new full offline installer and
   `SHA256SUMS.txt` from the official stable release.
2. Copy both files to the Pusula computer with a USB drive.
3. Verify the SHA-256 exactly as in the first-install procedure.
4. Close Pusula and run the installer. Windows may require the same **More
   info** / **Run anyway** acknowledgement because the file arrived through an
   external transfer.
5. Start Pusula and verify the displayed version and existing records.

The installer rejects downgrades. SQLite migrations are forward-only, so never
use an older installer as application rollback after a newer version has opened
the database. Use the guarded data restore procedure with the same or a newer
application version.

## Data locations

The production location is:

```text
%LOCALAPPDATA%\com.stronganchor.pusula\data\pusula.sqlite3
```

Nearby files include encrypted backup queue metadata and recovery markers.
Never copy or replace the live SQLite file while Pusula is running. Use the
in-app export/backup path or `scripts\Restore-PusulaBackup.ps1`.

The gateway stores only encrypted `.age` ciphertext. The recovery private
key is not installed on the customer computer or gateway.
