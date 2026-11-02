#!/usr/bin/env bash
# Packs the Microsoft Store package (unsigned; the Store signs it) from an
# already-built target/release/elizabeats.exe. Windows only: uses makeappx
# from the Windows SDK. The manifest is crates/app/windows/Package.appxmanifest.
#
# Usage: scripts/package-msix.sh <version>
#   1.0.2         a release: target/packages/Elizabeats-Windows.msix, package
#                 version 1.0.2.0 (the one to submit to the Store)
#   1.0.2-dev.57  a test build: target/packages/Elizabeats-1.0.2-dev.57-Windows.msix,
#                 package version 1.0.2.57, which the Store refuses (it needs
#                 the last number to be 0), so it can't be submitted by mistake
set -euo pipefail
cd "$(dirname "$0")/.."

version=$1
if [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  package_version="$version.0"
  output=target/packages/Elizabeats-Windows.msix
elif [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-dev\.([0-9]+)$ ]]; then
  # The run number as the last of the four numbers (each at most 65535, and
  # never 0, which would make it look like a release).
  revision=$(( BASH_REMATCH[2] % 65536 ))
  package_version="${BASH_REMATCH[1]}.$(( revision == 0 ? 1 : revision ))"
  output=target/packages/Elizabeats-$version-Windows.msix
else
  echo "MSIX needs a version like 1.0.2 or 1.0.2-dev.57, not $version" >&2
  exit 1
fi

sdk_bin=$(dirname "$(ls "/c/Program Files (x86)/Windows Kits/10/bin/"*/x64/makeappx.exe | sort -V | tail -1)")

stage=target/msix
rm -rf "$stage"
mkdir -p "$stage" target/packages
cp target/release/elizabeats.exe "$stage/"
cp -r crates/app/windows/Assets "$stage/Assets"
sed "s/Version=\"0.0.0.0\"/Version=\"$package_version\"/" \
  crates/app/windows/Package.appxmanifest > "$stage/AppxManifest.xml"
grep -q "Version=\"$package_version\"" "$stage/AppxManifest.xml"

# The resource index lets Windows pick the right icon variant (sizes, and the
# unplated ones without a colored tile behind them). Without it, Windows only
# sees the plain Square44x44Logo.png and draws it on an accent-color plate.
# (MSYS_NO_PATHCONV: Git Bash would otherwise turn options like /cf into paths.)
MSYS_NO_PATHCONV=1 "$sdk_bin/makepri.exe" createconfig \
  /cf target/msix-priconfig.xml /dq en-US /pv 10.0.0 /o
MSYS_NO_PATHCONV=1 "$sdk_bin/makepri.exe" new /pr "$stage" /cf target/msix-priconfig.xml \
  /mn "$stage/AppxManifest.xml" /of "$stage/resources.pri" /o

"$sdk_bin/makeappx.exe" pack -o -d "$stage" -p "$output"
