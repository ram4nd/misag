#!/usr/bin/env bash
# Builds misag's console program, misag-cli.exe, where the Windows app bundles it
# (src-tauri/tauri.windows.conf.json lists it as an external binary). Run it on
# Windows (Git Bash) before `pnpm tauri build` or `pnpm tauri dev`; the release
# workflow does too. Tauri's build script wants the file present before it can build
# it, so a placeholder stands in first.
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"
triple=$(rustc -vV | sed -n 's/^host: //p')
target="binaries/misag-cli-${triple}.exe"
mkdir -p binaries
[ -s "$target" ] || : > "$target"
cargo build --release --bin misag-cli
cp target/release/misag-cli.exe "$target"
# A placeholder left in place would ship an empty program.
[ -s "$target" ] || { echo "error: $target is empty" >&2; exit 1; }
echo "built $target"
