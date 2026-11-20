#!/usr/bin/env bash
# Build Forbidden Library and produce the Linux release files in dist/:
#
#   ForbiddenLibrary-<version>-linux-x86_64.tar.gz   first install
#   update-linux.zip, update-linux.json              what installed copies download
#
#   scripts/package.sh                fetch tools, build the app, package it
#   scripts/package.sh --skip-build   reuse target/release/forbidden-library
#
# The version is read from Cargo.toml. Tools are downloaded and checksummed
# once by scripts/fetch-assets.sh into vendor/ (gitignored). Needs a Rust
# toolchain, curl, tar, zip, and lzip or bsdtar for the libjxl archive.

set -euo pipefail
shopt -s nullglob

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$root"

usage() {
    cat <<'EOF'
Usage: scripts/package.sh [options]

Build the Linux release files into dist/:
  ForbiddenLibrary-<version>-linux-x86_64.tar.gz
  update-linux.zip
  update-linux.json

Options:
  --skip-build   reuse target/release/forbidden-library instead of running cargo
  -h, --help     show this help and exit
EOF
}

skip_build=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-build) skip_build=1 ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$(uname -s)" != "Linux" ]; then
    echo "error: scripts/package.sh packages Linux builds; use scripts/package.ps1 on Windows" >&2
    exit 1
fi
case "$(uname -m)" in
    x86_64|amd64) ;;
    *)
        echo "error: Linux release builds are x86_64 only (this machine is $(uname -m))" >&2
        exit 1
        ;;
esac

for tool in cargo tar zip sha256sum curl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool is required" >&2
        exit 1
    fi
done

version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
if [ -z "$version" ]; then
    echo "error: could not read the workspace version from Cargo.toml" >&2
    exit 1
fi
echo "Packaging Forbidden Library $version for Linux x86_64"

# The pinned Linux tools, model and runtimes. Reuses verified downloads.
"$root/scripts/fetch-assets.sh"

if [ "$skip_build" -eq 0 ]; then
    echo "Building the app..."
    cargo build --release -p forbidden-library --no-default-features
fi

binary="$root/target/release/forbidden-library"
if [ ! -x "$binary" ]; then
    echo "error: $binary was not found; run without --skip-build" >&2
    exit 1
fi

linux="$root/vendor/linux"
semantic="$root/vendor/semantic"
assets="$root/assets"

dist="$root/dist"
payload="$dist/payload"
rm -rf -- "$payload"
mkdir -p -- "$payload"/ffmpeg "$payload"/jxl \
    "$payload"/semantic/model "$payload"/semantic/runtime/cpu "$payload"/semantic/runtime/vulkan

# The app itself and the tools it runs as subprocesses.
install -m 755 -- "$binary" "$payload/forbidden-library"
install -m 755 -- "$linux/ffmpeg/ffmpeg" "$payload/ffmpeg/ffmpeg"
install -m 755 -- "$linux/ffmpeg/ffprobe" "$payload/ffmpeg/ffprobe"
if [ -f "$linux/ffmpeg/GPLv3.txt" ]; then
    install -m 644 -- "$linux/ffmpeg/GPLv3.txt" "$payload/ffmpeg/LICENSE.txt"
fi
install -m 755 -- "$linux/jxl/cjxl" "$payload/jxl/cjxl"
install -m 755 -- "$linux/jxl/djxl" "$payload/jxl/djxl"
if [ -f "$linux/jxl/LICENSE.jpeg-xl" ]; then
    cp -a -- "$linux/jxl"/LICENSE* "$payload/jxl/"
fi
if [ -f "$linux/jxl/LICENSE.txt" ]; then
    install -m 644 -- "$linux/jxl/LICENSE.txt" "$payload/jxl/LICENSE.txt"
fi

# Semantic search: the model and both llama.cpp runtimes (shared libraries
# stay beside llama-server; it has RUNPATH $ORIGIN).
cp -a -- "$semantic/model"/*.gguf "$payload/semantic/model/"
for runtime in cpu vulkan; do
    cp -a -- "$linux/semantic/runtime/$runtime/." "$payload/semantic/runtime/$runtime/"
done

# Documentation, licences and the per-user installer.
cp -- README.md THIRD_PARTY.md LICENSE "$payload/"
cp -- "$assets/fonts/LICENSE.txt" "$payload/Roboto-LICENSE.txt"
cp -- "$root/installer/install.sh" "$payload/install.sh"
cp -- "$root/installer/forbidden-library.desktop" "$payload/forbidden-library.desktop"
cp -- "$assets/icon/app-icon.png" "$payload/app-icon.png"
chmod 755 -- "$payload/install.sh"

# Installed copies update from a small archive: only the files that change
# between releases. FFmpeg, libjxl, the model and the runtimes arrive with the
# tarball. Paths are relative to the install directory, which is what the
# Linux self-updater swaps in.
update_zip="$dist/update-linux.zip"
rm -f -- "$update_zip"
(
    cd "$payload"
    zip -q -9 -X "$update_zip" forbidden-library README.md THIRD_PARTY.md
)

update_sha256="$(sha256sum -- "$update_zip" | cut -d' ' -f1)"
update_size="$(stat -c %s "$update_zip")"
cat > "$dist/update-linux.json" <<EOF
{
  "version": "$version",
  "url": "https://github.com/barusoup/Forbidden-Library/releases/download/v$version/update-linux.zip",
  "sha256": "$update_sha256",
  "size": $update_size
}
EOF

# The tarball unpacks to a single ForbiddenLibrary-<version>-linux-x86_64
# directory holding install.sh and the payload beside it.
tarball="$dist/ForbiddenLibrary-$version-linux-x86_64.tar.gz"
rm -f -- "$tarball"
tar -C "$dist" --owner=0 --group=0 --numeric-owner \
    --transform "s,^payload,ForbiddenLibrary-$version-linux-x86_64," \
    -czf "$tarball" payload

echo ""
echo "Built dist/ForbiddenLibrary-$version-linux-x86_64.tar.gz ($(du -h -- "$tarball" | cut -f1))"
echo "Update archive: dist/update-linux.zip ($(du -h -- "$update_zip" | cut -f1))"
echo "Update manifest: dist/update-linux.json"
