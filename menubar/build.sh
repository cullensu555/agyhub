#!/bin/bash
# 把 Swift 菜单栏外壳 + Rust 内核 + Sparkle 组装成一个 .app
#
# 全程不需要 Xcode 工程文件和 Apple 开发者证书：
#   - swiftc 直接编译 Swift 源码
#   - Sparkle 手动嵌入 Frameworks/ 并用 @rpath 链接
#   - 最后 ad-hoc 签名（codesign -s -），本机和个人分发都够用
#
# 参考的是 codex-mixin 的做法：它就是 adhoc 签名，TeamIdentifier 为空，
# 所以这条路是通的，不用付费账号。

set -euo pipefail

# 脚本可能从 Finder 或非登录 shell 启动，PATH 里没有 cargo。
# 显式补上，否则会报 "cargo: command not found"。
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

cd "$(dirname "$0")"
ROOT="$(pwd)"
APP_NAME="AgyHub"
BUNDLE_ID="local.agyhub.menubar"
# 版本号的唯一来源是 agyhub-rs/Cargo.toml（App 和内核共用）。
# 原来默认写死 0.1.0，本地构建的 App 和 GitHub 上的版本永远对不上。
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/../agyhub-rs/Cargo.toml" | head -1)"
VERSION="${VERSION:-$CARGO_VERSION}"
BUILD_DIR="$ROOT/build"
APP="$BUILD_DIR/$APP_NAME.app"
CORE="$ROOT/../agyhub-rs"
SPARKLE="$ROOT/vendor/Sparkle.framework"

echo "==> 检查依赖"

if [ ! -d "$SPARKLE" ]; then
  echo "缺少 Sparkle.framework。先下载："
  echo "  cd vendor && curl -L -o s.tar.xz https://github.com/sparkle-project/Sparkle/releases/download/2.10.1/Sparkle-2.10.1.tar.xz && tar -xf s.tar.xz"
  exit 1
fi

echo "==> 构建 Rust 内核"
# 版本号编进内核：status / doctor / 日志里显示的版本与 App 一致
(cd "$CORE" && AGYHUB_VERSION="$VERSION" cargo build --release)
if [ "$("$CORE/target/release/agyhub" --version)" != "agyhub $VERSION" ]; then
  echo "✗ 内核版本号不对：$("$CORE/target/release/agyhub" --version)，期望 $VERSION"
  exit 1
fi
# 注入页面的脚本必须是合法 JS（include_str! 原样注入，语法错误编译期发现不了）
if command -v node >/dev/null 2>&1; then
  for js in "$CORE"/assets/*.js; do
    node --check "$js" || { echo "✗ $js 有语法错误"; exit 1; }
  done
fi

echo "==> 清理旧产物"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"
mkdir -p "$APP/Contents/Frameworks"

echo "==> 拷贝 Rust 内核"
cp "$CORE/target/release/agyhub" "$APP/Contents/Resources/agyhub"
chmod +x "$APP/Contents/Resources/agyhub"

echo "==> 拷贝 Sparkle"
# -R 保留 framework 内部的符号链接结构（Versions/B、Current 等），
# 用 -L 会把链接展开成实体，framework 就坏了
cp -R "$SPARKLE" "$APP/Contents/Frameworks/"

echo "==> 编译 Swift 外壳"
# Sparkle 相关常量在编译期注入：
#   SU_FEED_URL      appcast 的在线地址（留空则不启用更新）
#   SU_PUBLIC_KEY    EdDSA 公钥（私钥在钥匙串里）
# 之所以用 -D 条件编译而不是只写在 Info.plist：
#   SUFeedURL 为空时 Sparkle 会报错甚至起不来，需要代码层面也能判断。
# 注意 set -u 下空数组 "${ARR[@]}" 会报 unbound variable，
# 必须写成 ${ARR[@]+"${ARR[@]}"} 这种兼容形式（bash 3.2 也认）。
SPARKLE_FLAGS=()
if [ -n "${SU_FEED_URL:-}" ]; then
  SPARKLE_FLAGS+=(-DHAVE_FEED)
fi

swiftc \
  -O \
  -target arm64-apple-macosx13.0 \
  ${SPARKLE_FLAGS[@]+"${SPARKLE_FLAGS[@]}"} \
  -F "$APP/Contents/Frameworks" \
  -framework Cocoa \
  -framework SwiftUI \
  -framework Sparkle \
  -Xlinker -rpath \
  -Xlinker "@executable_path/../Frameworks" \
  -o "$APP/Contents/MacOS/$APP_NAME" \
  "$ROOT/Sources/main.swift"

echo "==> 生成 Info.plist"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleExecutable</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>AgyHub</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>

  <!-- 自动更新：指向 GitHub Releases 的 appcast。
       留空则菜单里不显示「检查更新」，Sparkle 也不会初始化。 -->
  <key>SUFeedURL</key><string>${SU_FEED_URL:-}</string>
  <key>SUPublicEDKey</key><string>${SU_PUBLIC_KEY:-}</string>
  <key>SUVerifyUpdateBeforeExtraction</key><true/>
  <key>SUAutomaticallyUpdate</key><false/>
  <key>SUEnableAutomaticChecks</key><true/>
</dict>
</plist>
PLIST

# 注：SSH 解锁、中文注入的 JS 已经编译进 Rust 内核（include_str!），
# 不再需要单独打包 .cjs 脚本。App 现在只有两个文件：
#   Contents/MacOS/AgyHub       Swift 外壳
#   Contents/Resources/agyhub   Rust 内核（含所有注入脚本与词库）

echo "==> 图标"
# 图标由 make-icon.py 程序化生成（SDF 绘制，可随时改参数重出）。
# 不再借用 Antigravity 的 icns —— 那会让 App 看起来就是 Antigravity 本身。
ICON_SRC="$ROOT/icon-design"
ICON_PY="$ROOT/make-icon.py"

# 没有现成的就现场生成一次
if [ ! -f "$ICON_SRC/AppIcon.icns" ] || [ ! -f "$ICON_SRC/MenuIcon.png" ]; then
  echo "  未找到图标，现场生成…"
  PYGEN=""
  for cand in /opt/homebrew/bin/python3 \
              "$HOME/.workbuddy/binaries/python/envs/default/bin/python" \
              python3; do
    if "$cand" -c "import numpy, PIL" 2>/dev/null; then PYGEN="$cand"; break; fi
  done
  if [ -z "$PYGEN" ]; then
    echo "  ✗ 找不到带 numpy+PIL 的 python，跳过图标"
  else
    "$PYGEN" "$ICON_PY" "$ICON_SRC"
  fi
fi

if [ -f "$ICON_SRC/AppIcon.icns" ]; then
  cp "$ICON_SRC/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" \
    "$APP/Contents/Info.plist" 2>/dev/null || true
  # 1024 PNG 也放一份，某些地方（通知、活动监视器）优先读 png
  [ -f "$ICON_SRC/Icon-1024.png" ] && \
    cp "$ICON_SRC/Icon-1024.png" "$APP/Contents/Resources/AppIcon.png"
fi

if [ -f "$ICON_SRC/MenuIcon.png" ]; then
  cp "$ICON_SRC/MenuIcon.png" "$APP/Contents/Resources/MenuIcon.png"
fi

echo "==> 签名"
# 先签内层（Sparkle 自己的可执行文件），再签整体。
# 顺序错了 codesign 会报 "nested bundle not signed"。
codesign --force --deep --sign - \
  --options runtime \
  "$APP/Contents/Frameworks/Sparkle.framework" 2>/dev/null || \
codesign --force --deep --sign - "$APP/Contents/Frameworks/Sparkle.framework"

codesign --force --deep --sign - "$APP"

echo "==> 校验"
codesign -dv "$APP" 2>&1 | grep -E "Identifier|Signature" || true

echo
echo "✓ 打包完成：$APP"
echo

# 构建产物放在 build/ 会让「聚焦搜索」里出现两个同名 App
# （一个 /Applications 一个 build/），用户会以为装重了。
# 所以直接装到 /Applications 并清掉 build/ 里的副本，只保留一份。
if [ "${NO_INSTALL:-0}" != "1" ]; then
  echo "==> 安装到 /Applications（保持只有一份）"
  # 先正常退出旧 App（它退出时会停掉自己的服务），再兜底停一次内核服务
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" 2>/dev/null || true
  sleep 1
  pkill -f "/Applications/$APP_NAME.app/Contents/MacOS/$APP_NAME" 2>/dev/null || true
  # ⚠ 用**刚编出来的**内核去停，不用 /Applications 里的旧内核：
  # 1.1.0 及以前的内核没有 stop 命令，未知命令会落到默认分支直接起一个网关。
  # 新内核能认出旧版本起的进程（按配置里的端口 + 进程名），会一起停掉。
  "$CORE/target/release/agyhub" stop >/dev/null 2>&1 || true
  sleep 0.5
  /bin/rm -rf "/Applications/$APP_NAME.app"
  cp -R "$APP" "/Applications/"
  # 移除隔离属性，避免每次都要「仍要打开」
  xattr -cr "/Applications/$APP_NAME.app" 2>/dev/null || true
  /bin/rm -rf "$APP"
  echo "  ✓ /Applications/$APP_NAME.app"
  echo
  echo "  打开： open /Applications/$APP_NAME.app"
else
  echo "  （NO_INSTALL=1，跳过安装）"
  echo "  打开： open \"$APP\""
fi
echo
