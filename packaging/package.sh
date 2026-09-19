#!/usr/bin/env bash
# Build a local installer for the current platform into dist/.
#
#   macOS  -> .app bundle and a .dmg
#   Linux  -> .deb and an .AppImage
#
# Everything comes from `cargo packager`, configured in Cargo.toml under
# [package.metadata.packager]. The tool is installed on first run. Nothing here
# signs or notarises anything: these are local installers for local machines.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install Rust first: https://rustup.rs" >&2
  exit 1
fi

if ! cargo packager --version >/dev/null 2>&1; then
  echo "installing cargo-packager (first run only)"
  cargo install cargo-packager --locked
fi

# The dmg step drives Finder over AppleScript to arrange the window, which
# needs a desktop session and an Automation permission grant. Over SSH it
# hangs for a minute and fails with -1712, so default to the bundle alone
# unless the caller asks for the dmg.
case "$(uname -s)" in
  Darwin) formats="${FORMATS:-app}" ;;
  Linux)  formats="deb,appimage" ;;
  *)      echo "use packaging/package.ps1 on Windows" >&2; exit 1 ;;
esac

echo "building release binary and packaging: $formats"
if [ "$(uname -s)" = "Darwin" ] && [ "$formats" = "app" ]; then
  echo "(set FORMATS=app,dmg from a desktop Terminal to also build the dmg)"
fi
cargo packager --release --formats "$formats" --verbose

# create-dmg leaves its read-write intermediate behind when it fails.
rm -f dist/rw.*.dmg 2>/dev/null || true

echo
echo "artifacts in dist/:"
ls -la dist/ 2>/dev/null || echo "  (nothing produced)"
