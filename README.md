# Pusula Desktop

Pusula Desktop is the offline-first Windows version of Pusula Lite. One Windows PC and its local SQLite database are the production source of truth; internet access is optional and used only for signed application updates and encrypted off-machine backups.

The desktop app preserves the existing Turkish customer, contact, sale, installment, payment, receipt, expected-payment, and daily-report workflows without requiring WordPress, Apache, PHP, MySQL, or a browser login.

## Development

Requirements:

- Windows 10/11 x64
- Node.js 22+
- Rust stable with the `x86_64-pc-windows-msvc` target
- Visual Studio 2022 C++ Build Tools
- WebView2

```powershell
npm ci
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
npm run tauri dev
```

Fast validation:

```powershell
npm run build
npm run test:frontend
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path gateway/Cargo.toml
```

An unsigned local installer can be built for testing with:

```powershell
npm run tauri -- build --bundles nsis --no-sign
```

Production artifacts must be Authenticode signed and must also carry a valid Tauri updater signature.

## Repository Layout

- `src/`: existing Pusula interface adapted to the Tauri compatibility bridge
- `src-tauri/`: local SQLite service, migration/import, backup client, updater, and Windows packaging
- `gateway/`: VPS enrollment and presigned Backblaze B2 upload service
- `docs/`: installation, migration, backup/restore, testing, and release runbooks

No credentials or production customer exports belong in this repository.
