# Windows code signing

Pusula releases use Microsoft Azure Artifact Signing with a **Public Trust** certificate profile. The private key remains in Microsoft's managed hardware security modules; the release workflow does not import an Authenticode PFX or store a code-signing private key in GitHub.

This is separate from Tauri updater signing. The existing `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets sign the updater archive and must remain configured.

## One-time Azure enrollment

An authorized representative of the legal publisher must complete these steps. Identity validation can be completed only in the Azure portal and may require email verification, government-issued identification, and current business records.

1. Use a paid Azure subscription and register the `Microsoft.CodeSigning` resource provider.
2. Create a Basic Artifact Signing account in a supported region.
3. Assign the representative the `Artifact Signing Identity Verifier` role and complete Public Trust organization identity validation.
4. After validation succeeds, create a Public Trust certificate profile.
5. Create a Microsoft Entra app registration for the GitHub release workflow.
6. Add a federated identity credential with:
   - Issuer: `https://token.actions.githubusercontent.com`
   - Audience: `api://AzureADTokenExchange`
   - Subject: `repo:stronganchor/pusula-desktop:environment:windows-release`
7. Assign the app registration the `Artifact Signing Certificate Profile Signer` role at the certificate-profile scope only:

   ```text
   /subscriptions/<subscription-id>/resourceGroups/<resource-group>/providers/Microsoft.CodeSigning/codeSigningAccounts/<account>/certificateProfiles/<profile>
   ```

Do not create an Entra client secret for this workflow. GitHub obtains a short-lived Azure token through OpenID Connect (OIDC).

Public Trust enrollment is currently available to organizations in the United States, Canada, the European Union, and the United Kingdom, and to individual developers in the United States and Canada. Microsoft documents a normal identity-validation window of 1 to 20 business days, potentially longer when more documentation is required.

Microsoft's setup documentation:

- <https://learn.microsoft.com/azure/artifact-signing/quickstart>
- <https://learn.microsoft.com/azure/artifact-signing/tutorial-assign-roles>
- <https://learn.microsoft.com/azure/developer/github/connect-from-azure-openid-connect>

## GitHub release environment

Create and protect a GitHub environment named `windows-release`. Restrict deployments to the `main` branch and require an appropriate reviewer before a signing job starts.

Configure these environment variables (not secrets):

| Variable | Value |
| --- | --- |
| `AZURE_CLIENT_ID` | Application/client ID of the Entra app registration |
| `AZURE_TENANT_ID` | Entra tenant ID |
| `AZURE_SUBSCRIPTION_ID` | Azure subscription containing Artifact Signing |
| `ARTIFACT_SIGNING_ENDPOINT` | Regional endpoint, for example `https://eus.codesigning.azure.net` |
| `ARTIFACT_SIGNING_ACCOUNT` | Artifact Signing account name |
| `ARTIFACT_SIGNING_PROFILE` | Public Trust certificate profile name |

Configure these environment secrets:

| Secret | Purpose |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Tauri updater private key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for the updater private key |

The obsolete `WINDOWS_CODESIGN_PFX_BASE64` and `WINDOWS_CODESIGN_PASSWORD` secrets must not be configured or used.

## Release behavior

The workflow logs in with `azure/login` using GitHub OIDC, downloads the official Microsoft Artifact Signing Client Tools MSI, verifies Microsoft's Authenticode signature on that installer, and installs it noninteractively. Tauri then invokes `scripts/Invoke-ArtifactSigning.ps1` through its custom `signCommand` for every Windows executable it bundles.

The helper uses SHA-256 and Microsoft's RFC 3161 timestamp service. It authenticates only through the Azure CLI session created by `azure/login`; interactive and unrelated credential providers are explicitly disabled. A signing or verification failure stops the build.

Signing happens during Tauri bundling so both the inner `pusula-desktop.exe` and each NSIS setup executable are signed before Tauri creates the updater ZIP and updater signature. The release gate verifies valid, timestamped Authenticode signatures on:

- `src-tauri\target\release\pusula-desktop.exe`
- `Pusula_<version>_x64_offline-setup.exe`
- `Pusula_<version>_x64-setup.exe`

The release remains blocked until the Azure identity validation is complete, the Public Trust profile exists, the GitHub environment is protected, and a workflow run proves all three signatures valid.
