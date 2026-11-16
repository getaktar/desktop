#!/usr/bin/env bash
# Fails if a built binary contains the build machine's home folder or user
# name, the Linux counterpart of scripts/check_binary.ps1. A last check
# before anything is shipped (see scripts/build_linux.sh).
#
#   scripts/check_binary.sh src-tauri/target/release/aktar
set -euo pipefail

binary="${1:?usage: check_binary.sh <binary>}"
[[ -f "$binary" ]] || { echo "No binary at $binary" >&2; exit 1; }

user="$(id -un)"
found=0
for needle in "$HOME" "/$user/"; do
  if grep -a -q -F -- "$needle" "$binary"; then
    echo "The binary contains the build machine's home folder or user name ($needle):" >&2
    grep -a -o -F -m 3 -- "$needle" "$binary" | head -3 >&2
    found=1
  fi
done
exit "$found"
