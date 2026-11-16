#!/usr/bin/env bash
# Prints rustc flags that keep the build machine's paths out of the binary,
# the Linux counterpart of scripts/remap_paths.ps1.
#
# Rust embeds the source paths of every crate it compiles (for panic
# messages), so a plain build puts the home folder, and with it the user
# name, into the binary. These flags map the home folder, Rust's folders
# and the repository to neutral names.
#
# The result is for CARGO_ENCODED_RUSTFLAGS (flags separated by 0x1F, so
# paths with spaces survive), including any flags already set there.
# scripts/build_linux.sh and the release workflow use it;
# scripts/check_binary.sh verifies the result.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"

# When several prefixes match, rustc applies the last one: the home folder
# goes first so the more specific folders inside it win.
flags=(
  "--remap-path-prefix=$HOME=/home"
  "--remap-path-prefix=$cargo_home=/cargo"
  "--remap-path-prefix=$rustup_home=/rustup"
  "--remap-path-prefix=$root=/aktar"
)
encoded="$(IFS=$'\x1f'; echo "${flags[*]}")"
if [[ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
  encoded="$CARGO_ENCODED_RUSTFLAGS"$'\x1f'"$encoded"
fi
printf '%s' "$encoded"
