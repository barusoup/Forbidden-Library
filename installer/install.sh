#!/usr/bin/env bash
# Install Forbidden Library for the current user, or remove it again.
#
#   ./install.sh              install (or reinstall) the copy beside this script
#   ./install.sh --uninstall  remove the program, desktop entry and icon
#
# The tarball from the releases page already has the layout this script
# expects: install.sh, forbidden-library and the ffmpeg/, jxl/ and semantic/
# folders in one directory. From a checkout, point it at the staged payload:
#
#   scripts/package.sh --skip-build
#   installer/install.sh
#
# Nothing here needs root. The program goes into the XDG data directory
# (${XDG_DATA_HOME:-$HOME/.local/share}/forbidden-library/app), a launcher
# symlink goes into ${XDG_BIN_HOME:-$HOME/.local/bin}, and the library itself
# is kept separately in ${XDG_DATA_HOME:-$HOME/.local/share}/Forbidden Library
# so reinstalling or uninstalling never touches it.

set -euo pipefail

app_slug="forbidden-library"
app_name="Forbidden Library"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_home="${XDG_BIN_HOME:-$HOME/.local/bin}"
app_dir="$data_home/$app_slug/app"
desktop_dir="$data_home/applications"
desktop_file="$desktop_dir/$app_slug.desktop"
icon_dir="$data_home/icons/hicolor/256x256/apps"
icon_file="$icon_dir/$app_slug.png"
library_dir="$data_home/$app_name"

usage() {
    cat <<EOF
Usage: install.sh [options]

Installs $app_name for the current user:
  program     $app_dir
  launcher    $bin_home/$app_slug
  menu entry  $desktop_file
  icon        $icon_file

Options:
  --uninstall  remove the program, launcher, menu entry and icon
  -h, --help   show this help and exit

Environment:
  XDG_DATA_HOME          default $HOME/.local/share
  XDG_BIN_HOME           default $HOME/.local/bin
  FORBIDDEN_LIBRARY_PAYLOAD
                         install from this directory instead of the one
                         containing this script

Your library in "$library_dir" is never modified by this script.
EOF
}

uninstall() {
    rm -f -- "$bin_home/$app_slug" "$desktop_file" "$icon_file"
    rm -rf -- "$app_dir"
    rmdir -- "$data_home/$app_slug" 2>/dev/null || true
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$desktop_dir" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache --quiet "$data_home/icons/hicolor" >/dev/null 2>&1 || true
    fi
    echo "Removed $app_name from $app_dir."
    echo "Your library in \"$library_dir\" was left untouched."
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

# Where the payload lives: an explicit override, the directory this script is
# in (tarball layout), or the staged payload of a checkout.
payload="${FORBIDDEN_LIBRARY_PAYLOAD:-}"
if [ -z "$payload" ]; then
    if [ -x "$script_dir/$app_slug" ]; then
        payload="$script_dir"
    elif [ -x "$script_dir/../dist/payload/$app_slug" ]; then
        payload="$(cd -- "$script_dir/../dist/payload" && pwd -P)"
    fi
fi

case "${1:-}" in
    --uninstall)
        uninstall
        exit 0
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    "")
        ;;
    *)
        echo "error: unknown argument: $1" >&2
        usage >&2
        exit 2
        ;;
esac

if [ -z "$payload" ] || [ ! -x "$payload/$app_slug" ]; then
    echo "error: no $app_name payload found." >&2
    echo "Run this script from the unpacked release tarball, or set FORBIDDEN_LIBRARY_PAYLOAD." >&2
    exit 1
fi

# The desktop entry and icon are leaf files: use the payload's copies, or the
# checkout's originals when installing straight from a source tree.
desktop_source="$payload/$app_slug.desktop"
[ -f "$desktop_source" ] || desktop_source="$script_dir/$app_slug.desktop"
if [ ! -f "$desktop_source" ]; then
    echo "error: $app_slug.desktop was not found" >&2
    exit 1
fi

icon_source="$payload/app-icon.png"
if [ ! -f "$icon_source" ]; then
    icon_source="$script_dir/../assets/icon/app-icon.png"
fi
if [ ! -f "$icon_source" ]; then
    echo "error: app-icon.png was not found" >&2
    exit 1
fi

install -d -- "$data_home" "$bin_home" "$desktop_dir" "$icon_dir"

# Stage the payload next to the install directory and swap it in, so a running
# copy cannot get in the way and a half-copied install never replaces a good
# one. The data directory lives outside the app directory, so it is not touched.
staged="$app_dir.new.$$"
retired="$app_dir.old.$$"
rm -rf -- "$staged" "$retired"
mkdir -p -- "$staged"
trap 'rm -rf -- "$staged"' EXIT
cp -a -- "$payload/." "$staged/"
chmod 755 -- "$staged/$app_slug"
# The self-updater only runs for a copy carrying this marker; a loose build
# unpacked somewhere must not rewrite itself or its neighbours.
: > "$staged/.installed"
if [ -d "$app_dir" ]; then
    mv -- "$app_dir" "$retired"
fi
mv -- "$staged" "$app_dir"
rm -rf -- "$retired"
trap - EXIT

ln -sfn -- "$app_dir/$app_slug" "$bin_home/$app_slug"
install -m 644 -- "$desktop_source" "$desktop_file"
install -m 644 -- "$icon_source" "$icon_file"

if command -v desktop-file-validate >/dev/null 2>&1; then
    if ! desktop-file-validate "$desktop_file"; then
        echo "warning: desktop-file-validate reported problems with $desktop_file" >&2
    fi
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$desktop_dir" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --quiet "$data_home/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Installed $app_name to $app_dir."
echo "Launcher: $bin_home/$app_slug"
case ":$PATH:" in
    *":$bin_home:"*) ;;
    *) echo "note: $bin_home is not on your PATH; add it or run $app_dir/$app_slug directly." ;;
esac
echo "Your library lives in \"$library_dir\" (FORBIDDEN_LIBRARY_DATA_DIR overrides it)."
