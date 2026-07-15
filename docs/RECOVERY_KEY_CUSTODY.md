# Pusula recovery-key custody

Pusula encrypts every durable backup to one age X25519 recipient. The matching
identity is the only way to decrypt those backups. The Tauri updater private
key is separate: losing it prevents future in-app updates to installations
that trust its embedded public key.

Neither private key belongs in this repository, a GitHub artifact, the backup
gateway, the production Pusula profile, email, chat, or a support ticket.

## Portable recovery kit

`scripts/New-PusulaRecoveryKit.ps1` creates one passphrase-encrypted OpenPGP
file containing:

- the age recovery identity;
- the password-encrypted Tauri updater private key and matching public key;
- the updater-key password recovered from the current Windows user's DPAPI
  escrow without printing it;
- an inner manifest with exact hashes; and
- the pinned `rage.exe` and `rage-keygen.exe` tools when they are adjacent.

The script first runs `rage-keygen -y` and requires the supplied identity to
derive to the exact recipient embedded in Pusula. It also requires the
escrowed updater public key to byte-match `plugins.updater.pubkey` in
`tauri.conf.json`. Neither private key nor either password is written to the
console.

The inner ZIP exists only in a current-user-only temporary directory. GnuPG
receives the high-entropy recovery passphrase over standard input rather than
as a command-line argument. The wrapper uses AES-256 with iterated-and-salted
S2K mode 3, SHA-512, the maximum encoded S2K count, and OpenPGP integrity
protection. The script decrypts the completed wrapper, requires a byte-exact
ZIP round trip and exact entry allowlist, then removes plaintext staging in a
`finally` block. Normal deletion is not forensic erasure, so run custody work
only on an access-controlled, full-disk-encrypted Windows computer.

7-Zip AES-256 is not used by this automation because its noninteractive CLI
places the password in the child process command line. OpenPGP remains a
portable format that GnuPG/Gpg4win can decrypt on a replacement Windows PC.

## Create and verify the kit

Use only the current owner Windows profile. The following paths are examples;
all secret-source paths are explicit so the script never searches for keys:

```powershell
$recoveryRoot = "$env:USERPROFILE\OneDrive\Documents\Pusula Release Recovery"
$rageRoot = Join-Path $recoveryRoot `
  'tools\rage-v0.12.1-x86_64-windows\rage'

.\scripts\New-PusulaRecoveryKit.ps1 `
  -RecoveryIdentityPath "$env:USERPROFILE\.pusula-secrets\recovery.agekey" `
  -ExpectedAgeRecipient 'age1ht9yu6avu79sxq0w3s68t9gh3u853q7q9aehhjw7h2w68zw0yq2qpyv059' `
  -RageKeygenPath (Join-Path $rageRoot 'rage-keygen.exe') `
  -TauriPrivateKeyPath "$env:USERPROFILE\.pusula-secrets\tauri-updater.key" `
  -TauriPublicKeyPath (Join-Path $recoveryRoot 'tauri-updater-initial.key.pub') `
  -TauriPasswordDpapiPath "$env:USERPROFILE\.pusula-secrets\tauri-updater-password.dpapi" `
  -TauriConfigPath '.\src-tauri\tauri.conf.json' `
  -OutputDirectory $recoveryRoot `
  -GpgPath 'C:\Program Files\Git\usr\bin\gpg.exe'
```

The official rage v0.12.1 Windows ZIP used for this release is
`rage-v0.12.1-x86_64-windows.zip`, SHA-256
`da5b8111c8f097c7822df505ad504696e4891ff8adec06a39171f8d717590b2c`.
Obtain it from the official `str4d/rage` GitHub release and verify that hash
before extracting it.

The command returns only nonsecret paths, hashes, and boolean verification
results. It creates:

```text
Pusula-Recovery-Kit-<UTC>-<id>.zip.gpg
Pusula-Recovery-Kit-<UTC>-<id>.zip.gpg.sha256
Pusula-Recovery-Kit-<UTC>-<id>-public-manifest.json
Pusula-Recovery-Kit-<UTC>-<id>-RECOVERY-SHEET-PRINT-THEN-DELETE.html
```

The public manifest deliberately reports `production_ready: false` and one
encrypted copy. Creating a file is not complete custody.

## One unavoidable owner action

Open and print the recovery-sheet HTML. Compare every group on the paper with
the screen, store the paper somewhere physically separate from the Pusula PC,
then delete the HTML from OneDrive and empty OneDrive's recycle bin. The sheet
is temporarily sensitive because it contains the wrapper passphrase. Until it
is removed from the synchronized folder, the encrypted archive and its
passphrase are co-located and do not form secure escrow.

Keep two off-device copies of the encrypted `.zip.gpg` file. OneDrive can be
one copy. Use a separately controlled USB drive, safe, or second private cloud
location for the other. The paper recovery code must not travel with either
copy. Record only the archive SHA-256 and custody locations in the release
handoff; never record the code.

## Recover on a replacement PC

1. Copy the encrypted archive and `.sha256` file into an access-controlled
   local directory outside every sync root.
2. Verify the archive SHA-256.
3. Install GnuPG/Gpg4win from its official source.
4. Run GnuPG without putting the code on the command line:

   ```powershell
   gpg --output Pusula-Recovery-Kit.zip --decrypt Pusula-Recovery-Kit-<id>.zip.gpg
   ```

   Type the code from the paper only into GnuPG's protected prompt.
5. Extract the ZIP locally and verify every file against `manifest.json`.
6. Use `pusula-recovery.agekey` only with the guarded restore procedure in
   `RESTORE_HARNESS.md`. Use the updater key only in an approved release
   environment.
7. Delete plaintext kit files after the recovery or signing incident closes.

If the paper code or both encrypted copies are lost, preserve any surviving
live SQLite database and stop. If the age identity is exposed, rotate the
embedded recipient before making new backups and retain the old identity until
all old ciphertext has expired or been re-encrypted.
