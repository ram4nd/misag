#!/bin/sh
# Builds the .deb, then the Arch package from it. Pass makepkg flags through, e.g. -i to install.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
ver=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/src-tauri/Cargo.toml" | head -n1)

(cd "$root" && pnpm tauri build --bundles deb)
cp "$root/src-tauri/target/release/bundle/deb/Mischief of Agents_${ver}_amd64.deb" "$here/misag_${ver}_amd64.deb"
sed -i "s/^pkgver=.*/pkgver=$ver/" "$here/PKGBUILD"
cd "$here" && makepkg -f "$@"
