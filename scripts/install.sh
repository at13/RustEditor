#!/bin/sh
# Per-user install for Linux: binary in ~/.local/bin, desktop entry so
# RustEditor appears in the app menu and "Open With".
#
#   ./scripts/install.sh [path/to/rusteditor]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
source=${1:-"$here/../target/release/rusteditor"}
bin_dir=${XDG_BIN_HOME:-"$HOME/.local/bin"}
apps_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/applications

if [ ! -x "$source" ]; then
    echo "error: $source not found; build with: cargo build --release" >&2
    exit 1
fi

mkdir -p "$bin_dir" "$apps_dir"
# Copy to a temporary name first so a running editor is not disturbed.
cp "$source" "$bin_dir/.rusteditor.new"
chmod 755 "$bin_dir/.rusteditor.new"
mv -f "$bin_dir/.rusteditor.new" "$bin_dir/rusteditor"

sed "s|^Exec=rusteditor|Exec=$bin_dir/rusteditor|" \
    "$here/../packaging/linux/rusteditor.desktop" > "$apps_dir/rusteditor.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps_dir" || true
fi

echo "Installed $bin_dir/rusteditor"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "note: add $bin_dir to PATH to run 'rusteditor' from a terminal" ;;
esac
