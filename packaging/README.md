# Packaging

Local installers for The Real Explorer, one command per platform. Everything is
driven by [`cargo-packager`](https://crates.io/crates/cargo-packager), which is
configured in `Cargo.toml` under `[package.metadata.packager]` and installed on
first run by the scripts below. Artifacts land in `dist/`.

| Platform | Command                            | Produces                                           |
| -------- | ---------------------------------- | -------------------------------------------------- |
| Windows  | `powershell packaging\package.ps1` | `the-real-explorer_<version>_x64-setup.exe` (NSIS) |
| macOS    | `bash packaging/package.sh`        | `.app` bundle and a `.dmg`                         |
| Linux    | `bash packaging/package.sh`        | `.deb` and an `.AppImage`                          |

## Windows

The NSIS installer installs for the current user, so it needs no elevation:

- binary and uninstaller in `%LOCALAPPDATA%\The Real Explorer\`
- a Start Menu shortcut under `The Real Explorer`
- an uninstall entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`

Silent install and uninstall both work, which is how the build is verified:

```
.\dist\the-real-explorer_0.1.0_x64-setup.exe /S
"%LOCALAPPDATA%\The Real Explorer\uninstall.exe" /S
```

Nothing is code signed. SmartScreen will warn the first time a downloaded copy
runs; a locally built copy run from disk generally does not trigger it.

## Linux

The `.deb` declares `libgtk-3-0`, `libxkbcommon0` and `libwayland-client0`. A
build machine additionally needs the development packages for winit and glutin:

```
sudo apt-get install -y libxkbcommon-dev libwayland-dev libx11-dev \
    libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev libgtk-3-dev
```

## macOS

The bundle is unsigned and unnotarised, so Gatekeeper blocks a copied `.dmg`
until it is cleared with `xattr -dr com.apple.quarantine`, or opened once from
the right-click menu. Signing needs a Developer ID certificate and is out of
scope for a local build.

## Icons

`packaging/icons/icon.png` and `icon.ico` are generated, not drawn: see the
script in the commit that added them. Replace both with real artwork when there
is any; the packager reads whatever is at those paths.
