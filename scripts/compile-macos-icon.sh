#!/usr/bin/env bash
# Compiles assets/icon.icon (Icon Composer) into target/macos-icon/Assets.car,
# which scripts/package.sh copies into Elizabeats.app.
# macOS 26+ uses it for the Liquid Glass icon, including its dark, tinted and
# clear variants; older macOS keeps using assets/icon.icns.
#
# Needs Xcode 26 or newer (for `actool`); the command line tools alone aren't
# enough. Run it before scripts/package.sh on macOS.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=target/macos-icon
rm -rf "$OUT"
mkdir -p "$OUT"

# `--app-icon icon` must match the file name of assets/icon.icon and the
# CFBundleIconName in crates/app/macos/Info.plist.
xcrun actool assets/icon.icon \
  --compile "$OUT" \
  --output-partial-info-plist "$OUT/partial-info.plist" \
  --app-icon icon \
  --include-all-app-icons \
  --enable-on-demand-resources NO \
  --development-region en \
  --target-device mac \
  --minimum-deployment-target 11.0 \
  --platform macosx \
  --output-format human-readable-text --notices --warnings --errors

if [[ ! -f "$OUT/Assets.car" ]]; then
  echo "actool did not produce $OUT/Assets.car" >&2
  exit 1
fi
