#!/bin/sh
# Removes the per-user Linux install made by install.sh. User settings in
# ~/.config/rusteditor are kept.
set -eu

bin_dir=${XDG_BIN_HOME:-"$HOME/.local/bin"}
apps_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/applications

rm -f "$bin_dir/rusteditor" "$apps_dir/rusteditor.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps_dir" || true
fi
echo "Removed RustEditor (settings in ${XDG_CONFIG_HOME:-$HOME/.config}/rusteditor were kept)"
