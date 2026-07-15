[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$scriptPath = Join-Path $repoRoot 'scripts\New-PusulaRecoveryKit.ps1'
$gpgCommand = Get-Command gpg.exe -ErrorAction SilentlyContinue
if ($null -eq $gpgCommand) {
    throw 'recovery-custody.tests.ps1 requires GnuPG 2.x.'
}

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool] $Condition,
        [Parameter(Mandatory = $true)][string] $Message
    )
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-ThrowsLike {
    param(
        [Parameter(Mandatory = $true)][scriptblock] $Action,
        [Parameter(Mandatory = $true)][string] $Pattern
    )
    try {
        & $Action
    }
    catch {
        if ($_.Exception.Message -notlike $Pattern) {
            throw "Expected '$Pattern', received '$($_.Exception.Message)'."
        }
        return
    }
    throw "Expected action to fail like '$Pattern'."
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('pusula-recovery-custody-tests-' + [Guid]::NewGuid().ToString('N'))
try {
    [IO.Directory]::CreateDirectory($root) | Out-Null
    $inputs = Join-Path $root 'inputs'
    $outputs = Join-Path $root 'outputs'
    [IO.Directory]::CreateDirectory($inputs) | Out-Null
    [IO.Directory]::CreateDirectory($outputs) | Out-Null

    $recipient = 'age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq82x5wf'
    $identityPath = Join-Path $inputs 'synthetic.agekey'
    $privateKeyPath = Join-Path $inputs 'updater.key'
    $publicKeyPath = Join-Path $inputs 'updater.key.pub'
    $passwordBlobPath = Join-Path $inputs 'updater-password.dpapi'
    $configPath = Join-Path $inputs 'tauri.conf.json'
    $mockKeygenPath = Join-Path $inputs 'mock-rage-keygen.exe'

    [IO.File]::WriteAllText($identityPath, 'SYNTHETIC-AGE-IDENTITY-ONLY', [Text.UTF8Encoding]::new($false))
    $publicDocument = "untrusted comment: minisign public key: SYNTHETIC`nRWSYNTHETICPUBLICKEYONLY`n"
    $encodedPublic = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($publicDocument))
    [IO.File]::WriteAllText($publicKeyPath, $encodedPublic + "`n", [Text.UTF8Encoding]::new($false))
    $privateDocument = "untrusted comment: minisign encrypted secret key`nSYNTHETIC-PRIVATE-CIPHERTEXT-ONLY`n"
    [IO.File]::WriteAllText(
        $privateKeyPath,
        [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($privateDocument)) + "`n",
        [Text.UTF8Encoding]::new($false)
    )
    [ordered]@{
        plugins = [ordered]@{
            updater = [ordered]@{ pubkey = $encodedPublic }
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $configPath -Encoding UTF8
    ConvertTo-SecureString 'SYNTHETIC-UPDATER-PASSWORD-ONLY-1234567890' -AsPlainText -Force |
        ConvertFrom-SecureString |
        Set-Content -LiteralPath $passwordBlobPath -Encoding Ascii

    $typeName = 'MockRageKeygen' + [Guid]::NewGuid().ToString('N')
    $source = @"
using System;
public static class $typeName {
    public static int Main(string[] args) {
        if (args.Length != 2 || args[0] != "-y") return 2;
        Console.WriteLine("$recipient");
        return 0;
    }
}
"@
    Add-Type -TypeDefinition $source -Language CSharp -OutputType ConsoleApplication -OutputAssembly $mockKeygenPath

    $invoke = @{
        RecoveryIdentityPath = $identityPath
        ExpectedAgeRecipient = $recipient
        RageKeygenPath = $mockKeygenPath
        TauriPrivateKeyPath = $privateKeyPath
        TauriPublicKeyPath = $publicKeyPath
        TauriPasswordDpapiPath = $passwordBlobPath
        TauriConfigPath = $configPath
        OutputDirectory = $outputs
        GpgPath = $gpgCommand.Source
    }

    $result = & $scriptPath @invoke
    Assert-True $result.AgeRecipientVerified 'The synthetic age recipient was not verified.'
    Assert-True $result.TauriPublicKeyVerified 'The synthetic updater public key was not verified.'
    Assert-True (-not $result.ProductionReady) 'A newly created kit must not claim production custody.'
    foreach ($path in @($result.ArchivePath, $result.ChecksumPath, $result.PublicManifestPath, $result.RecoverySheetPath)) {
        Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "Expected output is missing: $path"
    }

    $manifest = Get-Content -Raw -LiteralPath $result.PublicManifestPath | ConvertFrom-Json
    Assert-True ($manifest.binding.age_recipient_verified -eq $true) 'Public manifest lost the age binding.'
    Assert-True ($manifest.binding.tauri_public_key_verified -eq $true) 'Public manifest lost the updater binding.'
    Assert-True ($manifest.custody.production_ready -eq $false) 'Public manifest overstated custody readiness.'
    Assert-True ([string]$manifest.archive_sha256 -ceq [string]$result.ArchiveSha256) 'Archive hash changed.'

    $publicText = @(
        Get-Content -Raw -LiteralPath $result.PublicManifestPath
        Get-Content -Raw -LiteralPath $result.ChecksumPath
        ($result | ConvertTo-Json -Depth 5)
    ) -join "`n"
    foreach ($secretMarker in @(
        'SYNTHETIC-AGE-IDENTITY-ONLY',
        'SYNTHETIC-PRIVATE-CIPHERTEXT-ONLY',
        'SYNTHETIC-UPDATER-PASSWORD-ONLY'
    )) {
        Assert-True (-not $publicText.Contains($secretMarker)) 'A public output exposed synthetic secret material.'
    }

    $sheet = Get-Content -Raw -LiteralPath $result.RecoverySheetPath
    $match = [regex]::Match($sheet, '<div class="code" id="recovery-code">([^<]+)</div>')
    Assert-True $match.Success 'Recovery sheet did not contain the generated code.'
    $code = [Net.WebUtility]::HtmlDecode($match.Groups[1].Value)
    Assert-True ($code -cmatch '^[23456789ABCDEFGHJKMNPQRSTUVWXYZ]{5}(-[23456789ABCDEFGHJKMNPQRSTUVWXYZ]{5}){7}$') `
        'Generated recovery code did not satisfy the format policy.'

    $decryptedZipPath = Join-Path $root 'independent-roundtrip.zip'
    $gpgArguments = @(
        '--no-options', '--batch', '--yes', '--no-tty',
        '--pinentry-mode', 'loopback', '--passphrase-fd', '0',
        '--decrypt', '--output', $decryptedZipPath, $result.ArchivePath
    )
    $quotedGpgArguments = ($gpgArguments | ForEach-Object {
        if ($_.Contains('"')) { throw 'Unexpected quote in the synthetic GnuPG arguments.' }
        if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
    }) -join ' '
    $gpgStartInfo = [Diagnostics.ProcessStartInfo]::new()
    $gpgStartInfo.FileName = $gpgCommand.Source
    $gpgStartInfo.Arguments = $quotedGpgArguments
    $gpgStartInfo.UseShellExecute = $false
    $gpgStartInfo.CreateNoWindow = $true
    $gpgStartInfo.RedirectStandardInput = $true
    $gpgStartInfo.RedirectStandardOutput = $true
    $gpgStartInfo.RedirectStandardError = $true
    $gpgProcess = [Diagnostics.Process]::new()
    $gpgProcess.StartInfo = $gpgStartInfo
    try {
        [void] $gpgProcess.Start()
        $gpgProcess.StandardInput.WriteLine($code)
        $gpgProcess.StandardInput.Close()
        $gpgStdout = $gpgProcess.StandardOutput.ReadToEnd()
        $gpgStderr = $gpgProcess.StandardError.ReadToEnd()
        $gpgProcess.WaitForExit()
        Assert-True ($gpgProcess.ExitCode -eq 0) 'The recovery-sheet code could not decrypt the synthetic kit.'
        Assert-True (-not ($gpgStdout + $gpgStderr).Contains($code)) 'GnuPG exposed the recovery code.'
    }
    finally {
        $gpgProcess.Dispose()
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $roundTrip = [IO.Compression.ZipFile]::OpenRead($decryptedZipPath)
    try {
        $entryNames = @($roundTrip.Entries | ForEach-Object { $_.FullName })
        foreach ($requiredEntry in @(
            'pusula-recovery.agekey',
            'tauri-updater.key',
            'tauri-updater.key.pub',
            'tauri-updater-password.txt',
            'manifest.json',
            'README.txt',
            'tools/rage-keygen.exe'
        )) {
            Assert-True ($entryNames -ccontains $requiredEntry) "Independent recovery lost $requiredEntry."
        }

        $passwordEntry = @($roundTrip.Entries | Where-Object { $_.FullName -ceq 'tauri-updater-password.txt' })
        Assert-True ($passwordEntry.Count -eq 1) 'Updater password entry was ambiguous.'
        $reader = [IO.StreamReader]::new($passwordEntry[0].Open(), [Text.Encoding]::UTF8)
        try {
            $recoveredUpdaterPassword = $reader.ReadToEnd().Trim()
        }
        finally {
            $reader.Dispose()
        }
        Assert-True ($recoveredUpdaterPassword -ceq 'SYNTHETIC-UPDATER-PASSWORD-ONLY-1234567890') `
            'Portable updater-password recovery did not round-trip.'
        $recoveredUpdaterPassword = $null
    }
    finally {
        $roundTrip.Dispose()
    }

    $encryptedBytesAsText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($result.ArchivePath))
    Assert-True (-not $encryptedBytesAsText.Contains('SYNTHETIC-UPDATER-PASSWORD-ONLY')) `
        'The encrypted wrapper exposed a plaintext marker.'
    $encryptedBytesAsText = $null
    $code = $null

    $badOutputs = Join-Path $root 'bad-recipient'
    [IO.Directory]::CreateDirectory($badOutputs) | Out-Null
    $badRecipientInvoke = @{} + $invoke
    $badRecipientInvoke.OutputDirectory = $badOutputs
    $badRecipientInvoke.ExpectedAgeRecipient = $recipient.Substring(0, $recipient.Length - 1) + 'q'
    Assert-ThrowsLike { & $scriptPath @badRecipientInvoke } '*does not derive to the embedded recipient*'
    Assert-True (@(Get-ChildItem -LiteralPath $badOutputs -Force).Count -eq 0) `
        'Recipient mismatch left an output artifact.'

    $badPublicPath = Join-Path $inputs 'mismatched-updater.key.pub'
    [IO.File]::WriteAllText($badPublicPath, [Convert]::ToBase64String([byte[]](1, 2, 3)), [Text.UTF8Encoding]::new($false))
    $badPublicOutputs = Join-Path $root 'bad-public-key'
    [IO.Directory]::CreateDirectory($badPublicOutputs) | Out-Null
    $badPublicInvoke = @{} + $invoke
    $badPublicInvoke.OutputDirectory = $badPublicOutputs
    $badPublicInvoke.TauriPublicKeyPath = $badPublicPath
    Assert-ThrowsLike { & $scriptPath @badPublicInvoke } '*does not match the Tauri configuration*'
    Assert-True (@(Get-ChildItem -LiteralPath $badPublicOutputs -Force).Count -eq 0) `
        'Updater-key mismatch left an output artifact.'

    Write-Output 'Recovery custody tests passed.'
}
finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
