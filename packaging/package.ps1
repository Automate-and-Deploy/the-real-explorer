# Build a local Windows installer into dist\.
#
# Produces an NSIS setup .exe that installs per user (no elevation) and, when
# the WiX toolset is available, an .msi as well. Configuration lives in
# Cargo.toml under [package.metadata.packager]; cargo-packager downloads NSIS
# on first use. Nothing here is code signed: SmartScreen will warn on a fresh
# download, which is expected for a local build.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found. Install Rust first: https://rustup.rs'
}

$havePackager = $false
try { cargo packager --version *> $null; $havePackager = $? } catch { $havePackager = $false }
if (-not $havePackager) {
    Write-Host 'installing cargo-packager (first run only)'
    cargo install cargo-packager --locked
}

# Stop a running copy: cargo cannot replace a locked exe.
Get-Process 'the-real-explorer' -ErrorAction SilentlyContinue | Stop-Process -Force

Write-Host 'building release binary and packaging: nsis'
cargo packager --release --formats nsis --verbose

Write-Host ''
Write-Host 'artifacts in dist\:'
if (Test-Path dist) { Get-ChildItem dist | Format-Table Name, Length, LastWriteTime -AutoSize }
else { Write-Host '  (nothing produced)' }
