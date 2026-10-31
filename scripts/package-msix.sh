#!/usr/bin/env bash
# Packs the Microsoft Store package (unsigned; the Store signs it) at
# target/packages/Elizabeats_<version>_x64.msix, from an already-built
# target/release/elizabeats.exe. Windows only: uses makeappx from the
# Windows SDK. The manifest is crates/app/windows/Package.appxmanifest.
set -euo pipefail
cd "$(dirname "$0")/.."

# MSIX versions are four numbers; the Store needs the last to be 0, and every
# submission needs a higher version than the last.
version=$(cargo metadata --no-deps --format-version 1 |
  jq -r '.packages[] | select(.name == "elizabeats") | .version')
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "MSIX needs a plain X.Y.Z version in Cargo.toml, not $version" >&2
  exit 1
fi

makeappx=$(ls "/c/Program Files (x86)/Windows Kits/10/bin/"*/x64/makeappx.exe | sort -V | tail -1)

stage=target/msix
rm -rf "$stage"
mkdir -p "$stage" target/packages
cp target/release/elizabeats.exe "$stage/"
cp -r crates/app/windows/Assets "$stage/Assets"
sed "s/Version=\"0.0.0.0\"/Version=\"$version.0\"/" \
  crates/app/windows/Package.appxmanifest > "$stage/AppxManifest.xml"
grep -q "Version=\"$version.0\"" "$stage/AppxManifest.xml"

"$makeappx" pack -o -d "$stage" -p "target/packages/Elizabeats_${version}_x64.msix"
