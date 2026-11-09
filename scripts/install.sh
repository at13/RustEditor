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

helper=$(dirname "$source")/rusteditor-openai
if [ -x "$helper" ]; then
    cp "$helper" "$bin_dir/.rusteditor-openai.new"
    chmod 755 "$bin_dir/.rusteditor-openai.new"
    mv -f "$bin_dir/.rusteditor-openai.new" "$bin_dir/rusteditor-openai"
else
    echo "note: build rusteditor-openai to enable provider setup" >&2
fi

sdk_source=${2:-"$here/../providers/copilot"}
if [ -f "$sdk_source/package.json" ] && command -v npm >/dev/null 2>&1; then
    (cd "$sdk_source" && npm ci && npm run build)
    sdk_dir="$bin_dir/copilot-sdk"
    mkdir -p "$sdk_dir"
    cp -R "$sdk_source/dist" "$sdk_source/node_modules" "$sdk_dir/"
    cp "$sdk_source/package.json" "$sdk_dir/"
else
    echo "note: Copilot SDK needs Node.js/npm and providers/copilot" >&2
fi

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
