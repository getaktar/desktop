#!/usr/bin/env bash
# Builds the Linux release of Aktar (.deb, .rpm and AppImage) without the
# build machine's paths in it, the Linux counterpart of scripts/build.ps1.
#
# Rust embeds the source paths of every crate it compiles, and with them
# the home folder and user name; scripts/remap_paths.sh maps them to
# neutral names, and scripts/check_binary.sh checks the binary afterwards.
#
#   scripts/build_linux.sh                    # .deb, .rpm and AppImage
#   scripts/build_linux.sh --bundles deb      # just the .deb
#
# Needs Rust, Node.js 22+, pnpm, and the WebKitGTK and tray libraries:
#   sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev \
#     libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CARGO_ENCODED_RUSTFLAGS="$("$root/scripts/remap_paths.sh")"
export CARGO_ENCODED_RUSTFLAGS
unset RUSTFLAGS

if [[ $# -eq 0 ]]; then
  set -- --bundles deb,rpm,appimage
fi

cd "$root"
pnpm tauri build "$@"

"$root/scripts/check_binary.sh" "$root/src-tauri/target/release/aktar"
echo "Built without the build machine's paths. Bundles: $root/src-tauri/target/release/bundle"
