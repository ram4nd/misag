#!/bin/sh
# Builds the .deb, then the Arch package from it in build/, leaving the tracked
# PKGBUILD as it is. Pass makepkg flags through, e.g. -i to install.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
# The version the .deb is named after.
ver=$(node -p 'require(process.argv[1]).version' "$root/src-tauri/tauri.conf.json")

(cd "$root" && pnpm tauri build --bundles deb)
rm -rf "$here/build"
mkdir "$here/build"
cp "$root/src-tauri/target/release/bundle/deb/"*"_${ver}_amd64.deb" "$here/build/misag_${ver}_amd64.deb"
sed "s/^pkgver=.*/pkgver=$ver/" "$here/PKGBUILD" > "$here/build/PKGBUILD"
cd "$here/build" && makepkg -f "$@"
