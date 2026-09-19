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

Building needs a C compiler and nothing else: `sudo apt-get install -y build-essential`.
No X11, Wayland, GL, GTK or fontconfig development packages are required, and
`libssl-dev` is not either, since the HTTP client uses rustls. Verified from a
cold build on Ubuntu 24.04 with every GUI dev package removed.

The window libraries are loaded at runtime rather than linked, so the ELF
NEEDED list is only libc and friends and `dpkg-shlibdeps` derives nothing. The
`.deb` therefore declares them by hand, in `Cargo.toml`. Without
`libxkbcommon-x11-0` the installed app panics at startup, which was verified in
a bare `ubuntu:24.04` container.

Dragging a file out to another application is not available on Linux: the
underlying crate needs a GTK application window that eframe cannot supply, so
the app reports the gesture as unsupported rather than pulling in GTK.

## macOS

The bundle is unsigned and unnotarised, so Gatekeeper blocks a copied `.dmg`
until it is cleared with `xattr -dr com.apple.quarantine`, or opened once from
the right-click menu. Signing needs a Developer ID certificate and is out of
scope for a local build.

## Icons

`packaging/icons/icon.png` and `icon.ico` are generated, not drawn: see the
script in the commit that added them. Replace both with real artwork when there
is any; the packager reads whatever is at those paths.
