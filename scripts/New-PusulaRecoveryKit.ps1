[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $RecoveryIdentityPath,
    [Parameter(Mandatory = $true)][string] $ExpectedAgeRecipient,
    [Parameter(Mandatory = $true)][string] $RageKeygenPath,
    [Parameter(Mandatory = $true)][string] $TauriPrivateKeyPath,
    [Parameter(Mandatory = $true)][string] $TauriPublicKeyPath,
    [Parameter(Mandatory = $true)][string] $TauriPasswordDpapiPath,
    [Parameter(Mandatory = $true)][string] $TauriConfigPath,
    [Parameter(Mandatory = $true)][string] $OutputDirectory,
    [string] $GpgPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$script:createdOutputPaths = [System.Collections.Generic.List[string]]::new()
$script:stagingDirectory = $null
$script:recoveryPassphrase = $null
$script:tauriPassword = $null

function Resolve-BoundedFile {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Label,
        [long] $MaximumBytes = 65536
    )

    $resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    $item = Get-Item -LiteralPath $resolved -Force -ErrorAction Stop
    if ($item.PSIsContainer -or $item.Length -lt 1 -or $item.Length -gt $MaximumBytes) {
        throw "$Label must be a nonempty file no larger than $MaximumBytes bytes."
    }
    return $item
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string] $Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-PrivateDirectory {
    param([Parameter(Mandatory = $true)][string] $Path)

    [IO.Directory]::CreateDirectory($Path) | Out-Null
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    if ($null -eq $identity.User) {
        throw 'Could not determine the current Windows security identifier.'
    }

    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $inheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $propagation = [Security.AccessControl.PropagationFlags]::None
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
        $identity.User,
        [Security.AccessControl.FileSystemRights]::FullControl,
        $inheritance,
        $propagation,
        [Security.AccessControl.AccessControlType]::Allow
    )
    $security.AddAccessRule($rule)
    [IO.Directory]::SetAccessControl($Path, $security)
    return (Resolve-Path -LiteralPath $Path).Path
}

function Set-PrivateFileAcl {
    param([Parameter(Mandatory = $true)][string] $Path)

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $security = [Security.AccessControl.FileSecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
        $identity.User,
        [Security.AccessControl.FileSystemRights]::FullControl,
        [Security.AccessControl.AccessControlType]::Allow
    )
    $security.AddAccessRule($rule)
    [IO.File]::SetAccessControl($Path, $security)
}

function ConvertTo-SafeProcessArgument {
    param([Parameter(Mandatory = $true)][string] $Value)

    if ($Value.Contains('"')) {
        throw 'A process argument contained an unsupported quote character.'
    }
    if ($Value -match '[\s]') {
        return '"' + $Value + '"'
    }
    return $Value
}

function Invoke-CapturedProcess {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList
    )

    $arguments = ($ArgumentList | ForEach-Object { ConvertTo-SafeProcessArgument $_ }) -join ' '
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = $arguments
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        [void] $process.Start()
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout
            Stderr = $stderr
        }
    }
    finally {
        $process.Dispose()
    }
}

function Invoke-GpgWithPassphrase {
    param(
        [Parameter(Mandatory = $true)][string] $Executable,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList,
        [Parameter(Mandatory = $true)][string] $Passphrase,
        [Parameter(Mandatory = $true)][string] $Operation
    )

    if ($Passphrase -match '[\r\n]') {
        throw 'The generated recovery passphrase contained an invalid line break.'
    }

    $arguments = ($ArgumentList | ForEach-Object { ConvertTo-SafeProcessArgument $_ }) -join ' '
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.Arguments = $arguments
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        [void] $process.Start()
        $process.StandardInput.WriteLine($Passphrase)
        $process.StandardInput.Close()
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()

        if ($stdout.Contains($Passphrase) -or $stderr.Contains($Passphrase)) {
            throw "$Operation emitted secret material and was stopped."
        }
        if ($process.ExitCode -ne 0) {
            throw "$Operation failed with exit code $($process.ExitCode)."
        }
    }
    finally {
        $process.Dispose()
    }
}

function New-RecoveryPassphrase {
    $alphabet = '23456789ABCDEFGHJKMNPQRSTUVWXYZ'
    $characters = [System.Collections.Generic.List[char]]::new()
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $buffer = New-Object byte[] 1
    try {
        while ($characters.Count -lt 40) {
            $rng.GetBytes($buffer)
            if ($buffer[0] -ge 240) {
                continue
            }
            $characters.Add($alphabet[$buffer[0] % $alphabet.Length])
        }
    }
    finally {
        [Array]::Clear($buffer, 0, $buffer.Length)
        $rng.Dispose()
    }

    $groups = for ($offset = 0; $offset -lt $characters.Count; $offset += 5) {
        -join $characters.GetRange($offset, 5)
    }
    return $groups -join '-'
}

function Get-DpapiSecureStringValue {
    param([Parameter(Mandatory = $true)][string] $Path)

    $protectedText = (Get-Content -Raw -LiteralPath $Path).Trim()
    if ($protectedText -cnotmatch '^[0-9a-fA-F]{64,}$' -or ($protectedText.Length % 2) -ne 0) {
        throw 'The updater password escrow is not a Windows DPAPI SecureString blob.'
    }

    $secure = $protectedText | ConvertTo-SecureString
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        $value = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer)
        if ([string]::IsNullOrWhiteSpace($value) -or $value.Length -lt 16 -or
            $value.Length -gt 1024 -or $value -match '[\r\n\x00]') {
            throw 'The decrypted updater password did not satisfy the custody policy.'
        }
        return $value
    }
    finally {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer)
        $secure.Dispose()
        $protectedText = $null
    }
}

function Add-ZipFile {
    param(
        [Parameter(Mandatory = $true)][IO.Compression.ZipArchive] $Archive,
        [Parameter(Mandatory = $true)][string] $EntryName,
        [Parameter(Mandatory = $true)][string] $SourcePath
    )

    $entry = $Archive.CreateEntry($EntryName, [IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = [DateTimeOffset]::UtcNow
    $input = [IO.File]::Open($SourcePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $output = $entry.Open()
    try {
        $input.CopyTo($output)
    }
    finally {
        $output.Dispose()
        $input.Dispose()
    }
}

function Add-ZipText {
    param(
        [Parameter(Mandatory = $true)][IO.Compression.ZipArchive] $Archive,
        [Parameter(Mandatory = $true)][string] $EntryName,
        [Parameter(Mandatory = $true)][string] $Text
    )

    $entry = $Archive.CreateEntry($EntryName, [IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = [DateTimeOffset]::UtcNow
    $stream = $entry.Open()
    $writer = [IO.StreamWriter]::new($stream, [Text.UTF8Encoding]::new($false))
    try {
        $writer.Write($Text)
    }
    finally {
        $writer.Dispose()
        $stream.Dispose()
    }
}

try {
    $identity = Resolve-BoundedFile $RecoveryIdentityPath 'The age recovery identity'
    $rageKeygen = Resolve-BoundedFile $RageKeygenPath 'rage-keygen.exe' 33554432
    $tauriPrivateKey = Resolve-BoundedFile $TauriPrivateKeyPath 'The Tauri updater private key'
    $tauriPublicKey = Resolve-BoundedFile $TauriPublicKeyPath 'The Tauri updater public key'
    $tauriPasswordBlob = Resolve-BoundedFile $TauriPasswordDpapiPath 'The Tauri updater password escrow'
    $tauriConfig = Resolve-BoundedFile $TauriConfigPath 'The Tauri configuration' 1048576

    if ($ExpectedAgeRecipient -cnotmatch '^age1[023456789acdefghjklmnpqrstuvwxyz]{58}$') {
        throw 'ExpectedAgeRecipient is not a canonical age X25519 recipient.'
    }

    $ageResult = Invoke-CapturedProcess $rageKeygen.FullName @('-y', $identity.FullName)
    if ($ageResult.ExitCode -ne 0) {
        throw "rage-keygen identity verification failed with exit code $($ageResult.ExitCode)."
    }
    $derivedRecipient = $ageResult.Stdout.Trim()
    if ($derivedRecipient -cne $ExpectedAgeRecipient) {
        throw 'The age recovery identity does not derive to the embedded recipient.'
    }

    $config = Get-Content -Raw -LiteralPath $tauriConfig.FullName | ConvertFrom-Json
    $configuredPublicKey = [string] $config.plugins.updater.pubkey
    $escrowPublicKey = (Get-Content -Raw -LiteralPath $tauriPublicKey.FullName).Trim()
    if ([string]::IsNullOrWhiteSpace($configuredPublicKey) -or $escrowPublicKey -cne $configuredPublicKey) {
        throw 'The escrowed updater public key does not match the Tauri configuration.'
    }
    try {
        $decodedPublicKey = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($configuredPublicKey))
    }
    catch {
        throw 'The configured updater public key is not valid base64.'
    }
    if (-not $decodedPublicKey.StartsWith('untrusted comment:', [StringComparison]::Ordinal)) {
        throw 'The configured updater public key is not a Minisign document.'
    }

    $privateKeyText = (Get-Content -Raw -LiteralPath $tauriPrivateKey.FullName).Trim()
    try {
        $decodedPrivateKey = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($privateKeyText))
    }
    catch {
        throw 'The updater private key escrow is not valid base64.'
    }
    $privateHeader = ($decodedPrivateKey -split "`r?`n", 2)[0]
    if (-not $privateHeader.StartsWith('untrusted comment:', [StringComparison]::Ordinal) -or
        -not $privateHeader.Contains('encrypted') -or -not $privateHeader.Contains('secret')) {
        throw 'The updater private key escrow is not a password-encrypted Minisign secret key.'
    }

    $script:tauriPassword = Get-DpapiSecureStringValue $tauriPasswordBlob.FullName

    if ([string]::IsNullOrWhiteSpace($GpgPath)) {
        $gpgCommand = Get-Command gpg.exe -ErrorAction SilentlyContinue
        if ($null -eq $gpgCommand) {
            throw 'GnuPG 2.x was not found. Pass its exact gpg.exe path with -GpgPath.'
        }
        $GpgPath = $gpgCommand.Source
    }
    $gpg = Resolve-BoundedFile $GpgPath 'gpg.exe' 33554432
    $gpgVersion = Invoke-CapturedProcess $gpg.FullName @('--version')
    if ($gpgVersion.ExitCode -ne 0 -or $gpgVersion.Stdout -cnotmatch '(?m)^gpg \(GnuPG\) 2\.') {
        throw 'The selected gpg.exe is not GnuPG 2.x.'
    }
    $gpgVersionLine = ($gpgVersion.Stdout -split "`r?`n")[0].Trim()

    $repoRoot = [IO.Path]::GetFullPath((Split-Path $PSScriptRoot -Parent)).TrimEnd('\')
    [IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
    $outputRoot = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $OutputDirectory).Path).TrimEnd('\')
    if ($outputRoot.Equals($repoRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $outputRoot.StartsWith($repoRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Recovery-kit output must be outside the Git repository.'
    }

    $createdUtc = [DateTimeOffset]::UtcNow
    $kitId = $createdUtc.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $baseName = "Pusula-Recovery-Kit-$kitId"
    $archivePath = Join-Path $outputRoot "$baseName.zip.gpg"
    $checksumPath = "$archivePath.sha256"
    $publicManifestPath = Join-Path $outputRoot "$baseName-public-manifest.json"
    $sheetPath = Join-Path $outputRoot "$baseName-RECOVERY-SHEET-PRINT-THEN-DELETE.html"
    foreach ($path in @($archivePath, $checksumPath, $publicManifestPath, $sheetPath)) {
        if (Test-Path -LiteralPath $path) {
            throw 'A generated recovery-kit output path already exists.'
        }
        $script:createdOutputPaths.Add($path)
    }

    $script:stagingDirectory = Get-PrivateDirectory (Join-Path ([IO.Path]::GetTempPath()) ('pusula-recovery-kit-' + [Guid]::NewGuid().ToString('N')))
    $plainZipPath = Join-Path $script:stagingDirectory 'recovery-kit.zip'
    $roundTripZipPath = Join-Path $script:stagingDirectory 'recovery-kit-roundtrip.zip'

    $sourceCommit = $null
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($null -ne $git) {
        $gitResult = Invoke-CapturedProcess $git.Source @('-C', $repoRoot, 'rev-parse', 'HEAD')
        if ($gitResult.ExitCode -eq 0 -and $gitResult.Stdout.Trim() -cmatch '^[0-9a-f]{40}$') {
            $sourceCommit = $gitResult.Stdout.Trim()
        }
    }

    $rageExecutable = Join-Path $rageKeygen.DirectoryName 'rage.exe'
    $includeRageExecutable = Test-Path -LiteralPath $rageExecutable -PathType Leaf
    if ($includeRageExecutable -and (Get-Item -LiteralPath $rageExecutable).Length -gt 33554432) {
        throw 'The companion rage.exe is unexpectedly large.'
    }

    $innerManifest = [ordered]@{
        schema_version = 1
        created_utc = $createdUtc.ToString('o')
        source_commit = $sourceCommit
        age_recipient = $ExpectedAgeRecipient
        files = [ordered]@{
            'pusula-recovery.agekey' = [ordered]@{
                size = [long] $identity.Length
                sha256 = Get-Sha256 $identity.FullName
            }
            'tauri-updater.key' = [ordered]@{
                size = [long] $tauriPrivateKey.Length
                sha256 = Get-Sha256 $tauriPrivateKey.FullName
            }
            'tauri-updater.key.pub' = [ordered]@{
                size = [long] $tauriPublicKey.Length
                sha256 = Get-Sha256 $tauriPublicKey.FullName
            }
        }
    }
    if ($includeRageExecutable) {
        $innerManifest.files['tools/rage.exe'] = [ordered]@{
            size = [long] (Get-Item -LiteralPath $rageExecutable).Length
            sha256 = Get-Sha256 $rageExecutable
        }
    }
    $innerManifest.files['tools/rage-keygen.exe'] = [ordered]@{
        size = [long] $rageKeygen.Length
        sha256 = Get-Sha256 $rageKeygen.FullName
    }

    $innerReadme = @"
Pusula portable recovery kit
============================

This decrypted ZIP contains high-impact private recovery material.
Keep it off the production PC except during an approved recovery operation.

Files:
- pusula-recovery.agekey: decrypts Pusula age-encrypted SQLite backups.
- tauri-updater.key: password-encrypted private Tauri updater signing key.
- tauri-updater-password.txt: password for tauri-updater.key.
- tauri-updater.key.pub: public updater key embedded in Pusula.
- manifest.json: exact hashes and public recipient binding.
- tools/rage.exe and tools/rage-keygen.exe: pinned recovery tools when present.

Verify every file against manifest.json before use. Never paste a private key,
password, database, customer export, or device token into chat, email, GitHub,
or a support ticket.
"@

    $archive = [IO.Compression.ZipFile]::Open($plainZipPath, [IO.Compression.ZipArchiveMode]::Create)
    try {
        Add-ZipFile $archive 'pusula-recovery.agekey' $identity.FullName
        Add-ZipFile $archive 'tauri-updater.key' $tauriPrivateKey.FullName
        Add-ZipFile $archive 'tauri-updater.key.pub' $tauriPublicKey.FullName
        Add-ZipText $archive 'tauri-updater-password.txt' ($script:tauriPassword + "`n")
        Add-ZipText $archive 'manifest.json' ($innerManifest | ConvertTo-Json -Depth 8)
        Add-ZipText $archive 'README.txt' $innerReadme
        Add-ZipFile $archive 'tools/rage-keygen.exe' $rageKeygen.FullName
        if ($includeRageExecutable) {
            Add-ZipFile $archive 'tools/rage.exe' $rageExecutable
        }
    }
    finally {
        $archive.Dispose()
    }
    Set-PrivateFileAcl $plainZipPath

    $script:recoveryPassphrase = New-RecoveryPassphrase
    $gpgCommon = @(
        '--no-options', '--batch', '--yes', '--no-tty',
        '--pinentry-mode', 'loopback', '--passphrase-fd', '0'
    )
    $encryptArguments = $gpgCommon + @(
        '--cipher-algo', 'AES256',
        '--s2k-cipher-algo', 'AES256',
        '--s2k-digest-algo', 'SHA512',
        '--s2k-mode', '3',
        '--s2k-count', '65011712',
        '--compress-algo', 'none',
        '--force-mdc',
        '--symmetric', '--output', $archivePath, $plainZipPath
    )
    Invoke-GpgWithPassphrase $gpg.FullName $encryptArguments $script:recoveryPassphrase 'Recovery-kit encryption'
    if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf) -or (Get-Item -LiteralPath $archivePath).Length -lt 128) {
        throw 'Recovery-kit encryption did not create a valid output file.'
    }

    $decryptArguments = $gpgCommon + @('--decrypt', '--output', $roundTripZipPath, $archivePath)
    Invoke-GpgWithPassphrase $gpg.FullName $decryptArguments $script:recoveryPassphrase 'Recovery-kit verification'
    if ((Get-Sha256 $plainZipPath) -cne (Get-Sha256 $roundTripZipPath)) {
        throw 'The encrypted recovery kit did not round-trip byte-for-byte.'
    }

    $expectedEntries = @(
        'README.txt',
        'manifest.json',
        'pusula-recovery.agekey',
        'tauri-updater-password.txt',
        'tauri-updater.key',
        'tauri-updater.key.pub',
        'tools/rage-keygen.exe'
    )
    if ($includeRageExecutable) {
        $expectedEntries += 'tools/rage.exe'
    }
    $verifiedZip = [IO.Compression.ZipFile]::OpenRead($roundTripZipPath)
    try {
        $actualEntries = @($verifiedZip.Entries | ForEach-Object { $_.FullName } | Sort-Object)
        $expectedEntries = @($expectedEntries | Sort-Object)
        if (($actualEntries -join "`n") -cne ($expectedEntries -join "`n")) {
            throw 'The recovery-kit ZIP entry allowlist did not match.'
        }
    }
    finally {
        $verifiedZip.Dispose()
    }

    $archiveHash = Get-Sha256 $archivePath
    $archiveSize = [long] (Get-Item -LiteralPath $archivePath).Length
    "$archiveHash  $(Split-Path $archivePath -Leaf)" |
        Set-Content -LiteralPath $checksumPath -Encoding Ascii

    $publicManifest = [ordered]@{
        schema_version = 1
        created_utc = $createdUtc.ToString('o')
        archive_name = Split-Path $archivePath -Leaf
        archive_size = $archiveSize
        archive_sha256 = $archiveHash
        encryption = [ordered]@{
            format = 'OpenPGP symmetric encrypted ZIP'
            cipher = 'AES256'
            s2k_mode = 3
            s2k_digest = 'SHA512'
            s2k_count = 65011712
            gpg_version = $gpgVersionLine
        }
        binding = [ordered]@{
            age_recipient_verified = $true
            age_recipient = $ExpectedAgeRecipient
            tauri_public_key_verified = $true
            tauri_public_key_file_sha256 = Get-Sha256 $tauriPublicKey.FullName
        }
        custody = [ordered]@{
            encrypted_copy_count = 1
            recovery_sheet_printed = $false
            recovery_sheet_must_be_deleted_after_print = $true
            production_ready = $false
        }
        source_commit = $sourceCommit
        custody_script_sha256 = Get-Sha256 $MyInvocation.MyCommand.Path
    }
    $publicManifest | ConvertTo-Json -Depth 8 |
        Set-Content -LiteralPath $publicManifestPath -Encoding UTF8

    $encodedArchiveName = [Net.WebUtility]::HtmlEncode((Split-Path $archivePath -Leaf))
    $encodedArchiveHash = [Net.WebUtility]::HtmlEncode($archiveHash)
    $encodedPassphrase = [Net.WebUtility]::HtmlEncode($script:recoveryPassphrase)
    $sheetHtml = @"
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Pusula recovery sheet - print then delete</title>
<style>
  body { font-family: Segoe UI, Arial, sans-serif; margin: 28mm; color: #111; }
  h1 { font-size: 22pt; margin-bottom: 4mm; }
  .warning { border: 3px solid #9b1c1c; padding: 5mm; font-weight: 700; }
  .code { border: 2px solid #111; padding: 7mm; margin: 8mm 0; font: 700 18pt Consolas, monospace; overflow-wrap: anywhere; }
  .meta { font: 10pt Consolas, monospace; overflow-wrap: anywhere; }
  li { margin: 3mm 0; }
  @media print { body { margin: 18mm; } }
</style>
</head>
<body>
<h1>Pusula recovery sheet</h1>
<div class="warning">SENSITIVE: print this page, store the paper away from the Pusula computer, then delete this HTML file from OneDrive and its recycle bin. Do not email, photograph into an ordinary gallery, or paste this code into chat.</div>
<p>This code unlocks the portable recovery kit:</p>
<div class="code" id="recovery-code">$encodedPassphrase</div>
<p class="meta"><strong>Archive:</strong> $encodedArchiveName</p>
<p class="meta"><strong>SHA-256:</strong> $encodedArchiveHash</p>
<ol>
  <li>Print clearly and verify every group against the screen.</li>
  <li>Store the paper in a physically separate, access-controlled place.</li>
  <li>Delete this HTML file from OneDrive and empty the OneDrive recycle bin.</li>
  <li>Keep two off-device copies of the encrypted <code>.zip.gpg</code> archive.</li>
</ol>
<p>Bu kod Pusula kurtarma paketini acar. Kagidi bilgisayardan ayri ve guvenli bir yerde saklayin.</p>
</body>
</html>
"@
    [IO.File]::WriteAllText($sheetPath, $sheetHtml, [Text.UTF8Encoding]::new($false))
    Set-PrivateFileAcl $sheetPath

    [pscustomobject]@{
        ArchivePath = $archivePath
        ArchiveSha256 = $archiveHash
        ChecksumPath = $checksumPath
        PublicManifestPath = $publicManifestPath
        RecoverySheetPath = $sheetPath
        AgeRecipientVerified = $true
        TauriPublicKeyVerified = $true
        Encryption = 'OpenPGP AES256 with iterated-and-salted SHA512 S2K'
        ProductionReady = $false
        RemainingCustodyAction = 'Print the recovery sheet, store it offline, delete the HTML, and create a second off-device encrypted archive copy.'
    }
}
catch {
    foreach ($path in $script:createdOutputPaths) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
    throw
}
finally {
    if ($null -ne $script:stagingDirectory) {
        Remove-Item -LiteralPath $script:stagingDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
    $script:tauriPassword = $null
    $script:recoveryPassphrase = $null
    [GC]::Collect()
    [GC]::WaitForPendingFinalizers()
}
