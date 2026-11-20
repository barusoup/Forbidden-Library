# Forbidden Library

A local photo and video library for Windows and Linux. Import from folders or
a phone, browse a timeline, search by what is in the picture, and keep
everything on your own machine.

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
- **Library password.** Optionally, the whole library is encrypted on disk
  under one password: the catalogue with SQLCipher, and every photo, video and
  preview with XChaCha20-Poly1305. The app stays shut until the password is
  typed, and it is set in Settings. A forgotten password cannot be recovered.
- **Verification and backups.** The library can check every stored file against
  its checksum and write a consistent backup while the app is running.
- **Hardened media decoding.** FFmpeg runs with time, memory and format limits;
  Rust decoders have size ceilings. A malformed file fails without crashing.

## Install

Both platforms are officially supported. Releases carry a Windows installer
and a Linux tarball.

### Windows

Download `ForbiddenLibrarySetup-<version>.exe` from the
[latest release](https://github.com/barusoup/Forbidden-Library/releases/latest)
and run it. It installs per user and needs no administrator rights.

Your library lives in `%LOCALAPPDATA%\Forbidden Library` and is never touched by
installs or updates. `FORBIDDEN_LIBRARY_DATA_DIR` overrides the location.

### Linux

Download `ForbiddenLibrary-<version>-linux-x86_64.tar.gz` from the
[latest release](https://github.com/barusoup/Forbidden-Library/releases/latest),
unpack it and run the installer:

```sh
tar -xzf ForbiddenLibrary-<version>-linux-x86_64.tar.gz
cd ForbiddenLibrary-<version>-linux-x86_64
./install.sh
```

The installer is per user and does not use `sudo`. It puts the program in
`${XDG_DATA_HOME:-$HOME/.local/share}/forbidden-library/app`, adds a
`forbidden-library` launcher to `${XDG_BIN_HOME:-$HOME/.local/bin}`, and
installs a menu entry and icon. If `~/.local/bin` is not on your `PATH`, add it
or run `~/.local/share/forbidden-library/app/forbidden-library` directly.

Run `./install.sh --uninstall` from the same tarball to remove the program,
launcher, menu entry and icon. Your library is left untouched.

Your library lives in `${XDG_DATA_HOME:-$HOME/.local/share}/Forbidden Library`
and is never touched by installs or updates. `FORBIDDEN_LIBRARY_DATA_DIR`
overrides the location.

### Updates

Installed copies on both platforms update themselves: at launch, before the
window opens, the app checks the latest GitHub release and, if there is a newer
one, installs it and starts the new version. If the check cannot reach GitHub,
the current version just starts. Set `FORBIDDEN_LIBRARY_NO_UPDATE=1` to turn
this off.

Windows updates come from `update.json`/`update.zip`; Linux updates come from
`update-linux.json`/`update-linux.zip` and only run for a copy installed by
`install.sh` (a loose unpacked build is never rewritten).

## Command line

Windows:

```powershell
forbidden-library --backup D:\Backups   # consistent copy of the library
forbidden-library --selfcheck           # headless engine and UI check
forbidden-library --version
```

Linux:

```bash
forbidden-library --backup ~/Backups     # consistent copy of the library
forbidden-library --selfcheck            # headless engine and UI check
forbidden-library --version
```

To restore a backup, close the app and copy the backup folder over the library
folder.

## Building

### Windows

Requires a recent stable Rust toolchain on Windows, [Inno Setup 6](https://jrsoftware.org/isinfo.php),
and a Perl on the `PATH` that builds Windows OpenSSL: the catalogue is SQLCipher,
and its OpenSSL is compiled from source on the first build. [Strawberry
Perl](https://strawberryperl.com) works; the Perl that ships with Git for Windows
or Cygwin does not.

```powershell
cargo test --workspace
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

`scripts/package.ps1` downloads the pinned FFmpeg, libjxl and search model,
builds the app, and writes the installer, `update.zip` and `update.json` to
`dist/`.

### Linux (x86_64)

Requires a recent stable Rust toolchain, a C toolchain, `cmake`, `perl`, `make`,
`pkg-config`, the ALSA, udev, xkbcommon, Wayland and X11 development headers
(for audio playback and windowing), plus `zip` and `lzip` (or `bsdtar`) for
packaging. On Ubuntu 24.04:

```bash
sudo apt-get install build-essential pkg-config cmake clang perl make \
  libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcursor-dev libxi-dev libxrandr-dev zip lzip
```

Then:

```bash
cargo test --workspace
scripts/package.sh
```

`scripts/package.sh` reads the version from `Cargo.toml`, downloads the pinned
Linux FFmpeg, libjxl, llama.cpp runtimes and search model through
`scripts/fetch-assets.sh` (into `vendor/`, which is gitignored), builds the app
and writes to `dist/`:

- `ForbiddenLibrary-<version>-linux-x86_64.tar.gz` — the per-user installer
  tarball;
- `update-linux.zip`, `update-linux.json` — what installed copies download to
  update.

`scripts/package.sh --skip-build` reuses an existing
`target/release/forbidden-library`.

The UI runs on the [Dream engine](https://github.com/barusoup/Dream).

## Releasing

Bump `workspace.package.version` in `Cargo.toml` and push a tag named
`v<version>`. The `Release` workflow then builds both platforms on GitHub
Actions and publishes one release carrying:

- `ForbiddenLibrarySetup-<version>.exe` (Windows installer);
- `ForbiddenLibrary-<version>-linux-x86_64.tar.gz` (Linux installer tarball);
- `update.zip` + `update.json` (Windows self-update);
- `update-linux.zip` + `update-linux.json` (Linux self-update).

A `workflow_dispatch` run builds the same artifacts without publishing, or
publishes a release when given a version and "build only" unchecked. The tag or
version input must match the workspace version, or the release job fails.

Installed copies pick the release up on their next launch.

## License

Forbidden Library is free software under the
[GNU Affero General Public License v3](LICENSE), with additional terms under
section 7. Any fork or derivative must stay open source under the same terms
and must credit the original project and its author, barusoup
(<https://github.com/barusoup>). See [LICENSE](LICENSE) for the full terms.

Third-party components and their licenses are listed in
[THIRD_PARTY.md](THIRD_PARTY.md).
