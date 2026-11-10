#!/bin/sh
# 把桌面应用打成可分发的 Rind.app，再封成 DMG。
#
# 用法：scripts/package-dmg.sh [输出目录]     （默认 target/dmg）
# 依赖：macOS（hdiutil / codesign / sips / iconutil）、Rust 工具链；版本号取自工作区 Cargo.toml。
# 图标来自 assets/AppIcon-1024.png（scripts/make-brand-images.py 生成），这里转成 AppIcon.icns。
# 产物：<输出目录>/Rind-v<版本>.dmg（内含 Rind.app 与 /Applications 快捷方式）。
#
# 注意：这里只做 ad-hoc 签名（codesign --sign -）。没有 Developer ID，下载后首次
# 打开需要右键 →「打开」。Developer ID 签名与公证是 ROADMAP M4 剩下的部分。
set -eu

cd "$(dirname "$0")/.."

OUT="${1:-target/dmg}"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [ -z "$VERSION" ]; then
  echo "package-dmg: 从 Cargo.toml 读不到工作区版本号" >&2
  exit 1
fi

echo "==> 构建 rind-app ${VERSION}（--release）"
cargo build --release -p rind-app

BIN="${CARGO_TARGET_DIR:-target}/release/rind-app"
if [ ! -x "$BIN" ]; then
  echo "package-dmg: 找不到构建产物 $BIN" >&2
  exit 1
fi

APP="$OUT/Rind.app"
STAGE="$OUT/stage"
rm -rf "$APP" "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$STAGE"
cp "$BIN" "$APP/Contents/MacOS/rind-app"

echo "==> 生成图标"
ICON_SRC="assets/AppIcon-1024.png"
ICONSET="$OUT/AppIcon.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns "$ICONSET" --output "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key><string>rind-app</string>
	<key>CFBundleIdentifier</key><string>dev.rind.Rind</string>
	<key>CFBundleName</key><string>Rind</string>
	<key>CFBundleDisplayName</key><string>Rind</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>CFBundleVersion</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "==> ad-hoc 签名"
codesign --force --sign - "$APP"
codesign --verify --verbose=1 "$APP"

echo "==> 组装 DMG"
cp -R "$APP" "$STAGE/Rind.app"
ln -sf /Applications "$STAGE/Applications"
DMG="$OUT/Rind-v${VERSION}.dmg"
hdiutil create -volname "Rind" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
echo "==> 完成：$DMG"
shasum -a 256 "$DMG"
