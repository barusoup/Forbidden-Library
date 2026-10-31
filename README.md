# Forbidden Library

A local photo and video library for Windows. Import from folders or a phone,
browse a timeline, search by what is in the picture, and keep everything on
your own machine.

## Features

- **Safe imports.** Files are copied, never moved, into a content-addressed
  store, so duplicates are detected by content and a crash cannot leave a
  half-written file behind.
- **Lossless optimization.** JPEGs become JPEG XL, and PNG/BMP files are
  recompressed, only when the result is verified identical to the original.
  Otherwise the original is kept. Videos are stored as imported.
- **Local semantic search.** A bundled embedding model ranks photos and video
  previews against a text query. Nothing leaves the PC.
- **Wireless transfer.** Scan a QR code with a phone on the same network and
  upload straight into the library over temporary HTTPS.
- **Password-locked collections.** Contents are encrypted with
  XChaCha20-Poly1305 under a key wrapped with Argon2id.
- **Verification and backups.** The library can check every stored file against
  its checksum and write a consistent backup while the app is running.
- **Hardened media decoding.** FFmpeg runs with time, memory and format limits;
  Rust decoders have size ceilings. A malformed file fails without crashing.

## Install

Download `ForbiddenLibrarySetup-<version>.exe` from the
[latest release](https://github.com/barusoup/Forbidden-Library/releases/latest)
and run it. It installs per user and needs no administrator rights.

Installed copies update themselves: at launch, before the window opens, the app
checks the latest GitHub release and, if there is a newer one, installs it and
starts the new version. If the check cannot reach GitHub, the current version
just starts. Set `FORBIDDEN_LIBRARY_NO_UPDATE=1` to turn this off.

Your library lives in `%LOCALAPPDATA%\Forbidden Library` and is never touched by
installs or updates. `FORBIDDEN_LIBRARY_DATA_DIR` overrides the location.

## Command line

```powershell
forbidden-library --backup D:\Backups   # consistent copy of the library
forbidden-library --selfcheck           # headless engine and UI check
forbidden-library --version
```

To restore a backup, close the app and copy the backup folder over the library
folder.

## Building

Requires a recent stable Rust toolchain on Windows and [Inno Setup 6](https://jrsoftware.org/isinfo.php).

```powershell
cargo test --workspace
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

`scripts/package.ps1` downloads the pinned FFmpeg, libjxl and search model,
builds the app, and writes the installer, `update.zip` and `update.json` to
`dist/`. The UI runs on the [Dream engine](https://github.com/barusoup/Dream).

## Releasing

Bump the workspace version in `Cargo.toml`, run `scripts/package.ps1`, then
publish a release tagged `v<version>` with `ForbiddenLibrarySetup-<version>.exe`,
`update.zip` and `update.json` attached. Installed copies pick it up on their
next launch.

## License

Forbidden Library is free software under the
[GNU Affero General Public License v3](LICENSE), with additional terms under
section 7. Any fork or derivative must stay open source under the same terms
and must credit the original project and its author, barusoup
(<https://github.com/barusoup>). See [LICENSE](LICENSE) for the full terms.

Third-party components and their licenses are listed in
[THIRD_PARTY.md](THIRD_PARTY.md).
