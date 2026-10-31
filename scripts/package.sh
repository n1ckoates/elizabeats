#!/usr/bin/env bash
# Packages a release build with Velopack: an installer and a portable copy
# that update themselves from the GitHub releases (see crates/app/src/updates.rs).
#
# Usage: scripts/package.sh <version> [rust-target]
#   version      e.g. 1.0.0, or 1.0.0-dev.42 for CI test builds
#   rust-target  only when cross-building, e.g. x86_64-apple-darwin
#
# Build first: cargo build --release -p elizabeats [--target <rust-target>].
# On macOS, run scripts/compile-macos-icon.sh first for the macOS 26 icon.
# With VELOPACK_DELTAS=1 and GITHUB_TOKEN set, the previous release is
# downloaded first so updates from it can be small deltas.
#
# Output: Velopack's files in target/velopack/<channel>/, and in target/dist/
# the files a release publishes: the downloads under stable names (e.g.
# Elizabeats-Windows-Setup.exe) plus the update packages and feed the
# updater reads.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$1
target=${2:-}
bin_dir=target/${target:+$target/}release
repository=https://github.com/n1ckoates/elizabeats

# Each OS/architecture is its own Velopack channel, so an installed copy only
# ever updates to builds for its own platform.
case "$(uname -s)" in
  Darwin)
    case "${target:-$(uname -m)}" in
      x86_64*) arch=x64 platform=macOS-Intel ;;
      *) arch=arm64 platform=macOS-AppleSilicon ;;
    esac
    channel=osx-$arch runtime=osx-$arch
    ;;
  MINGW* | MSYS* | CYGWIN*) channel=win runtime=win-x64 platform=Windows ;;
  Linux) channel=linux runtime=linux-x64 platform=Linux ;;
  *) echo "Unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

extra=()
stage=target/stage/$channel
out=target/velopack/$channel
rm -rf "$stage" "$out"
mkdir -p "$stage" "$out" target/dist

case $channel in
  osx-*)
    # The app bundle, with our Info.plist and icons.
    app=$stage/Elizabeats.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin_dir/elizabeats" "$app/Contents/MacOS/"
    cp assets/icon.icns "$app/Contents/Resources/icon.icns"
    sed "s/\$VERSION/$version/g" crates/app/macos/Info.plist > "$app/Contents/Info.plist"
    if [[ -f target/macos-icon/Assets.car ]]; then
      cp target/macos-icon/Assets.car "$app/Contents/Resources/"
    else
      # Without the compiled icon, macOS 26 falls back to icon.icns.
      plutil -remove CFBundleIconName "$app/Contents/Info.plist"
    fi
    pack_dir=$app main_exe=elizabeats icon=assets/icon.icns
    # Ad-hoc signing ("-": no certificate) after Velopack adds its updater to
    # the bundle. Without it the bundle's signature is broken and macOS calls
    # the downloaded app "damaged" instead of "from an unidentified developer".
    extra=(--signAppIdentity -)
    ;;
  win)
    cp "$bin_dir/elizabeats.exe" "$stage/"
    pack_dir=$stage main_exe=elizabeats.exe icon=assets/icon.ico
    ;;
  linux)
    cp "$bin_dir/elizabeats" "$stage/"
    pack_dir=$stage main_exe=elizabeats icon=assets/icon.png
    ;;
esac

if [[ "${VELOPACK_DELTAS:-}" == 1 ]]; then
  vpk download github --repoUrl "$repository" --channel "$channel" \
    --outputDir "$out" --token "$GITHUB_TOKEN" ||
    echo "No earlier release found; this release has no delta update."
fi

vpk pack \
  --packId Elizabeats \
  --packVersion "$version" \
  --packTitle Elizabeats \
  --packAuthors "Nick Oates" \
  --packDir "$pack_dir" \
  --mainExe "$main_exe" \
  --icon "$icon" \
  --channel "$channel" \
  --runtime "$runtime" \
  --outputDir "$out" \
  ${extra[@]+"${extra[@]}"}

# The downloads, under names that never change, so links to
# releases/latest/download/<name> always get the newest version.
shopt -s nullglob
for file in "$out"/*-Setup.exe "$out"/*-Setup.pkg; do
  cp "$file" "target/dist/Elizabeats-$platform-Setup.${file##*.}"
done
for file in "$out"/*-Portable.zip; do
  cp "$file" "target/dist/Elizabeats-$platform-Portable.zip"
done
for file in "$out"/*.AppImage; do
  cp "$file" "target/dist/Elizabeats-$platform.AppImage"
done
# What the updater reads: this version's packages and the channel's feed.
for file in "$out"/*-"$version"-*.nupkg "$out/releases.$channel.json"; do
  cp "$file" target/dist/
done
ls -l target/dist
