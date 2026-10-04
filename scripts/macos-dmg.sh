#!/bin/sh
# Build AstroFiler.app and a DMG for this Mac's architecture.
# Usage: scripts/macos-dmg.sh   (from the repository root; result in dist/)
set -eu

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
app=dist/AstroFiler.app
dmg=dist/AstroFiler-$version-$arch.dmg

cargo build --release --locked

rm -rf dist/AstroFiler.app dist/dmg "$dmg"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/astrofiler-gui "$app/Contents/MacOS/AstroFiler"
# The command-line tool rides along: AstroFiler.app/Contents/MacOS/astrofiler
cp target/release/astrofiler "$app/Contents/MacOS/astrofiler"

# Icon: the PNG in every size macOS asks for.
set=dist/AstroFiler.iconset
rm -rf "$set" && mkdir -p "$set"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/astrofiler.png --out "$set/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  [ $d -le 512 ] && sips -z $d $d assets/astrofiler.png --out "$set/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set" -o "$app/Contents/Resources/AstroFiler.icns"
rm -rf "$set"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>AstroFiler</string>
  <key>CFBundleDisplayName</key><string>AstroFiler</string>
  <key>CFBundleIdentifier</key><string>io.github.peterbuitho.astrofiler</string>
  <key>CFBundleExecutable</key><string>AstroFiler</string>
  <key>CFBundleIconFile</key><string>AstroFiler</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.photography</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key>
  <string>AstroFiler connects to your telescope over Wi-Fi to list and download images.</string>
  <key>NSRemovableVolumesUsageDescription</key>
  <string>AstroFiler reads images from a telescope connected over USB-C.</string>
  <key>NSNetworkVolumesUsageDescription</key>
  <string>AstroFiler moves finished folders to the inbox on your NAS.</string>
</dict>
</plist>
PLIST

# Not notarised: an ad-hoc signature is what Apple Silicon needs to run it.
codesign --force --deep --sign - "$app"

mkdir -p dist/dmg
cp -R "$app" dist/dmg/
ln -s /Applications dist/dmg/Applications
hdiutil create -volname "AstroFiler $version" -srcfolder dist/dmg -ov -format UDZO "$dmg" >/dev/null
rm -rf dist/dmg
echo "$dmg"
