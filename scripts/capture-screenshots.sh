#!/bin/sh
# 重拍 README 与文档用的界面截图和 demo.gif：docs/images/{en,zh}/。
#
# 用法：scripts/capture-screenshots.sh [en|zh ...]      （默认两种语言都拍）
# 依赖：macOS（screencapture / sips / osascript）、ffmpeg（生成 GIF）、Rust 工具链。
#
# 全程走示例数据（RIND_DEMO=1），不读也不碰真实家目录。每页单独起一次应用：
# RIND_PAGE 指定页、RIND_AUTOSCAN=1 让示例扫描先跑完、RIND_APPEARANCE 固定外观，
# 然后用 `screencapture -o -l <窗口号>` 只拍 Rind 的窗口（不带投影，别的窗口挡着也没关系）。
# 运行这个脚本的终端需要「屏幕录制」权限，否则截出来是全黑的。
set -eu

cd "$(dirname "$0")/.."

LANGS="${*:-en zh}"
WAIT="${RIND_SHOT_WAIT:-7}"   # 每页等多久再拍（秒）：示例扫描与图标加载要跑完
PAGES="overview apps orphans cleanup space launch history settings"
TARGET="${CARGO_TARGET_DIR:-target}"
BIN="$TARGET/release/rind-app"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "==> 构建 rind-app（--release）"
cargo build --release -p rind-app

# 按进程号找它的普通窗口（layer 0），输出窗口号。
cat > "$WORK/winid.js" <<'JS'
ObjC.import('CoreGraphics');
function run(argv) {
  const pid = parseInt(argv[0]);
  const list = ObjC.deepUnwrap(ObjC.castRefToObject(
    $.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));
  const w = list.find(w => w.kCGWindowOwnerPID === pid && w.kCGWindowLayer === 0);
  return w ? String(w.kCGWindowNumber) : "";
}
JS

# shot <语言> <页> <外观> <输出 jpg>
shot() {
  RIND_DEMO=1 RIND_LANG="$1" RIND_PAGE="$2" RIND_AUTOSCAN=1 RIND_APPEARANCE="$3" \
    "$BIN" >/dev/null 2>&1 &
  pid=$!
  sleep "$WAIT"
  win="$(osascript -l JavaScript "$WORK/winid.js" "$pid")"
  if [ -z "$win" ]; then
    kill "$pid" 2>/dev/null || true
    echo "capture-screenshots: 找不到 $1/$2 的窗口（终端有没有「屏幕录制」权限？）" >&2
    exit 1
  fi
  screencapture -x -o -l "$win" "$WORK/shot.png"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  sips -s format jpeg -s formatOptions 85 "$WORK/shot.png" --out "$4" >/dev/null
  echo "    $4"
}

for lang in $LANGS; do
  out="docs/images/$lang"
  mkdir -p "$out"
  echo "==> $lang"
  for page in $PAGES; do
    shot "$lang" "$page" dark "$out/$page.jpg"
  done
  shot "$lang" overview light "$out/overview-light.jpg"

  # demo.gif：九张图依次展示，每张 1.4 秒，宽 1400。
  frames="$WORK/frames-$lang"
  mkdir -p "$frames"
  i=0
  for page in $PAGES overview-light; do
    sips -Z 1400 "$out/$page.jpg" --out "$frames/$(printf %02d $i).png" -s format png >/dev/null
    i=$((i + 1))
  done
  ffmpeg -v error -y -framerate 1/1.4 -i "$frames/%02d.png" \
    -vf "split[a][b];[a]palettegen=max_colors=192[p];[b][p]paletteuse=dither=sierra2_4a" \
    -loop 0 "$out/demo.gif"
  echo "    $out/demo.gif"
done
