#!/usr/bin/env bash
# Fetch the Linux tools the release bundle carries, once, into vendor/.
#
#   scripts/fetch-assets.sh
#
# Everything is pinned by URL and SHA-256 so a build is reproducible and a
# corrupt download is never bundled. Existing verified files are reused, and
# scripts/package.sh calls this automatically, so an install never downloads
# anything. Nothing fetched here is committed: vendor/ is in .gitignore.
#
# The Windows fetch script is scripts/fetch-semantic.ps1; it fills
# vendor/semantic/model (shared with this script) and the Windows runtimes.
# This script adds the Linux runtimes under vendor/linux and the Linux
# FFmpeg/libjxl tools.

set -euo pipefail
shopt -s nullglob

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
vendor="$root/vendor"
cache="$vendor/downloads"
tools="$vendor/linux"
models="$vendor/semantic/model"
runtimes="$vendor/linux/semantic/runtime"

mkdir -p -- "$cache" "$models" "$runtimes"

# --- pinned downloads -------------------------------------------------------
#
# FFmpeg 7.0.2 static amd64 build from johnvansickle.com: one self-contained
# executable for ffmpeg and one for ffprobe, built on Debian, GPLv3. The
# checksum below was taken from the copy downloaded and verified on
# 2026-11-19. The release directory keeps old versions, so this URL stays
# valid even after newer builds appear.
ffmpeg_url="https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-amd64-static.tar.xz"
ffmpeg_sha256="abda8d77ce8309141f83ab8edf0596834087c52467f6badf376a6a2a4c87cf67"

# libjxl v0.12.0 static Linux build of cjxl/djxl. The release asset is
# lzip-compressed, so extraction needs lzip or libarchive's bsdtar.
jxl_url="https://github.com/libjxl/libjxl/releases/download/v0.12.0/jxl-linux-x86_64-static.tar.lz"
jxl_sha256="5318a1ea40adad76d023e0c17a03d4627f8282f83cb2b575e69be8e74f1ff456"

# llama.cpp b11846: Vulkan is preferred when a GPU is available, CPU is the
# fallback. Same release layout as the Windows zips used by package.ps1.
llama_build="b11846"
llama_cpu_url="https://github.com/ggml-org/llama.cpp/releases/download/$llama_build/llama-$llama_build-bin-ubuntu-x64.tar.gz"
llama_cpu_sha256="eb26d338963f04eda3f70f23ef4a8575d44bc643f58d26c2f51925a58fbd43ff"
llama_vulkan_url="https://github.com/ggml-org/llama.cpp/releases/download/$llama_build/llama-$llama_build-bin-ubuntu-vulkan-x64.tar.gz"
llama_vulkan_sha256="92d10e8f3a4370988b6b7f7aa23647fb74dc3e59f8a9d2d407cb381c4932fc7a"

# The embedding model is the same one the Windows bundle ships, so a checkout
# that already has vendor/semantic from package.ps1 does not fetch it again.
model_url="https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-UD-Q4_K_XL.gguf?download=true"
model_sha256="ea905fd08e8061db77a0031cbb5096d459d7cfd2bdd0ad2ba66e483f16719493"
mmproj_url="https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/mmproj-Q8_0.gguf?download=true"
mmproj_sha256="90e7b0238009e2954f856f2081dcf7f35af026b64c765e98f4777053e1754460"

# --- helpers ----------------------------------------------------------------

tmpdirs=()
cleanup() {
    local dir
    if [ "${#tmpdirs[@]}" -eq 0 ]; then
        return 0
    fi
    for dir in "${tmpdirs[@]}"; do
        [ -n "$dir" ] && rm -rf -- "$dir"
    done
    return 0
}
trap cleanup EXIT

fresh_tmp() {
    local dir
    dir="$(mktemp -d "${TMPDIR:-/tmp}/forbidden-library-assets.XXXXXX")"
    tmpdirs+=("$dir")
    printf '%s' "$dir"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "$1" | cut -d' ' -f1
    else
        echo "error: sha256sum or shasum is required" >&2
        exit 1
    fi
}

fetch() {
    local url="$1" dest="$2" sha256="$3"
    if [ -f "$dest" ]; then
        if [ "$(sha256_file "$dest")" = "$sha256" ]; then
            echo "  present  $(basename -- "$dest")"
            return 0
        fi
        echo "  replacing $(basename -- "$dest") (checksum mismatch)"
        rm -f -- "$dest"
    fi

    echo "  download $(basename -- "$dest")"
    local partial="$dest.part"
    rm -f -- "$partial"
    curl --fail --location --silent --show-error \
        --retry 4 --retry-delay 2 \
        --output "$partial" "$url"
    local actual
    actual="$(sha256_file "$partial")"
    if [ "$actual" != "$sha256" ]; then
        rm -f -- "$partial"
        echo "error: $url has SHA-256 $actual, expected $sha256" >&2
        exit 1
    fi
    mv -- "$partial" "$dest"
}

# Extract the libjxl lzip archive. lzip is the canonical tool (installed in
# CI); libarchive's bsdtar reads the format too, which is what Arch ships.
extract_lzip() {
    local archive="$1" dest="$2"
    if command -v lzip >/dev/null 2>&1; then
        lzip -dc -- "$archive" | tar -xf - -C "$dest"
    elif command -v bsdtar >/dev/null 2>&1; then
        bsdtar -xf "$archive" -C "$dest"
    else
        echo "error: extracting $archive needs lzip or bsdtar (apt install lzip)" >&2
        exit 1
    fi
}

# --- FFmpeg -----------------------------------------------------------------

install_ffmpeg() {
    if [ -x "$tools/ffmpeg/ffmpeg" ] && [ -x "$tools/ffmpeg/ffprobe" ]; then
        echo "  present  ffmpeg"
        return 0
    fi
    echo "  extract  ffmpeg"
    local tmp source
    tmp="$(fresh_tmp)"
    tar -xJf "$cache/ffmpeg-7.0.2-amd64-static.tar.xz" -C "$tmp"
    source="$(find "$tmp" -maxdepth 2 -type f -name ffmpeg -print -quit)"
    if [ -z "$source" ]; then
        echo "error: the FFmpeg archive did not contain ffmpeg" >&2
        exit 1
    fi
    source="$(dirname -- "$source")"
    rm -rf -- "$tools/ffmpeg"
    mkdir -p -- "$tools/ffmpeg"
    install -m 755 -- "$source/ffmpeg" "$tools/ffmpeg/ffmpeg"
    install -m 755 -- "$source/ffprobe" "$tools/ffmpeg/ffprobe"
    if [ -f "$source/GPLv3.txt" ]; then
        install -m 644 -- "$source/GPLv3.txt" "$tools/ffmpeg/GPLv3.txt"
    fi
}

# --- libjxl -----------------------------------------------------------------

install_jxl() {
    if [ -x "$tools/jxl/cjxl" ] && [ -x "$tools/jxl/djxl" ]; then
        echo "  present  libjxl"
        return 0
    fi
    echo "  extract  libjxl"
    local tmp license
    tmp="$(fresh_tmp)"
    extract_lzip "$cache/jxl-linux-x86_64-static.tar.lz" "$tmp"
    if [ ! -x "$tmp/tools/cjxl" ] || [ ! -x "$tmp/tools/djxl" ]; then
        echo "error: the libjxl archive did not contain tools/cjxl and tools/djxl" >&2
        exit 1
    fi
    rm -rf -- "$tools/jxl"
    mkdir -p -- "$tools/jxl"
    install -m 755 -- "$tmp/tools/cjxl" "$tools/jxl/cjxl"
    install -m 755 -- "$tmp/tools/djxl" "$tools/jxl/djxl"
    # The static build bundles several libraries; keep every licence it ships.
    for license in "$tmp"/LICENSE.*; do
        install -m 644 -- "$license" "$tools/jxl/$(basename -- "$license")"
    done
    if [ -f "$tmp/LICENSE.jpeg-xl" ]; then
        install -m 644 -- "$tmp/LICENSE.jpeg-xl" "$tools/jxl/LICENSE.txt"
    fi
}

# --- llama.cpp runtimes -----------------------------------------------------

install_llama_runtime() {
    local name="$1" archive="$2" dest="$runtimes/$1"
    if [ -x "$dest/llama-server" ]; then
        echo "  present  llama.cpp $name"
        return 0
    fi
    echo "  extract  llama.cpp $name"
    local tmp source library license
    tmp="$(fresh_tmp)"
    tar -xzf "$archive" -C "$tmp"
    source="$(find "$tmp" -maxdepth 2 -type f -name llama-server -print -quit)"
    if [ -z "$source" ]; then
        echo "error: the llama.cpp $name archive did not contain llama-server" >&2
        exit 1
    fi
    source="$(dirname -- "$source")"
    rm -rf -- "$dest"
    mkdir -p -- "$dest"
    install -m 755 -- "$source/llama-server" "$dest/llama-server"
    # llama-server has RUNPATH $ORIGIN, so the shared libraries stay beside it.
    for library in "$source"/*.so*; do
        cp -a -- "$library" "$dest/"
    done
    for license in "$source"/LICENSE*; do
        cp -a -- "$license" "$dest/"
    done
}

# --- main -------------------------------------------------------------------

echo "Linux tools (FFmpeg 7.0.2, libjxl v0.12.0, llama.cpp $llama_build)"
fetch "$ffmpeg_url" "$cache/ffmpeg-7.0.2-amd64-static.tar.xz" "$ffmpeg_sha256"
fetch "$jxl_url" "$cache/jxl-linux-x86_64-static.tar.lz" "$jxl_sha256"
fetch "$llama_cpu_url" "$cache/llama-$llama_build-bin-ubuntu-x64.tar.gz" "$llama_cpu_sha256"
fetch "$llama_vulkan_url" "$cache/llama-$llama_build-bin-ubuntu-vulkan-x64.tar.gz" "$llama_vulkan_sha256"

install_ffmpeg
install_jxl
install_llama_runtime cpu "$cache/llama-$llama_build-bin-ubuntu-x64.tar.gz"
install_llama_runtime vulkan "$cache/llama-$llama_build-bin-ubuntu-vulkan-x64.tar.gz"

echo "Semantic model (EmbeddingGemma 2, Unsloth UD-Q4_K_XL + Q8 projector)"
mkdir -p -- "$models"
fetch "$model_url" "$models/embeddinggemma-2-UD-Q4_K_XL.gguf" "$model_sha256"
fetch "$mmproj_url" "$models/mmproj-Q8_0.gguf" "$mmproj_sha256"

echo "Assets ready:"
du -sh -- "$tools" "$models" 2>/dev/null || true
