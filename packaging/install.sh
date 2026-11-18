#!/bin/bash
# ============================================================
#  AgyHub 一键安装
#
#  用法：
#     curl -fsSL https://raw.githubusercontent.com/cullensu555/agyhub/main/packaging/install.sh | bash
#
#  做的事：
#     1. 检查系统（只支持 Apple Silicon、macOS 13+）
#     2. 从 GitHub Releases 下载最新的 dmg
#     3. 挂载 → 拷到 /Applications → 卸载
#     4. 去掉 Gatekeeper 隔离标记（没有 Apple 开发者签名，否则打不开）
#     5. 启动 App
# ============================================================
set -euo pipefail

REPO="${AGYHUB_REPO:-cullensu555/agyhub}"
APP_NAME="AgyHub"
BUNDLE_ID="local.agyhub.menubar"
INSTALL_DIR="/Applications"

C_OK=$'\033[32m'; C_ERR=$'\033[31m'; C_WARN=$'\033[33m'
C_DIM=$'\033[2m'; C_OFF=$'\033[0m'
ok()  { echo "${C_OK}✓${C_OFF} $*"; }
err() { echo "${C_ERR}✗${C_OFF} $*" >&2; }
warn(){ echo "${C_WARN}!${C_OFF} $*"; }

# ---- 1. 环境检查 ----
if [ "$(uname -s)" != "Darwin" ]; then
  err "这个安装脚本只支持 macOS。"
  exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
  # 原来这里会接受 x86_64 并去下一个不存在的 -x86_64.dmg，最后报"下载失败"
  err "目前只提供 Apple Silicon（M 系列芯片）版本，这台 Mac 是 $(uname -m)。"
  exit 1
fi
OS_MAJOR="$(sw_vers -productVersion | cut -d. -f1)"
if [ "$OS_MAJOR" -lt 13 ]; then
  err "需要 macOS 13 或更高版本（当前 $(sw_vers -productVersion)）。"
  exit 1
fi
ok "系统: macOS $(sw_vers -productVersion) arm64"

# ---- 2. 取最新版本号 ----
echo ""
echo "查询最新版本…"
VERSION="${AGYHUB_VERSION:-}"
if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1) || true
fi
if [ -z "$VERSION" ]; then
  # GitHub API 有频率限制；退到 releases/latest 的跳转地址里取版本号
  VERSION=$(curl -fsSI "https://github.com/$REPO/releases/latest" 2>/dev/null \
    | sed -n 's#.*/releases/tag/v\{0,1\}\([^[:space:]]*\).*#\1#p' | tr -d '\r' | head -1) || true
fi
if [ -z "$VERSION" ]; then
  err "查不到最新版本（网络问题或 GitHub 限流）。"
  err "可以手动下载：https://github.com/$REPO/releases"
  exit 1
fi
ok "版本: $VERSION"

# ---- 3. 下载 ----
# 文件名与 release.sh 的产物一致：AgyHub-<版本>.dmg
DMG_URL="https://github.com/$REPO/releases/download/v$VERSION/AgyHub-$VERSION.dmg"
TMP_DIR="$(mktemp -d)"
MOUNT_POINT="$TMP_DIR/mnt"
cleanup() {
  hdiutil detach "$MOUNT_POINT" -quiet 2>/dev/null || true
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

DMG_PATH="$TMP_DIR/AgyHub.dmg"
echo ""
echo "下载 ${C_DIM}$DMG_URL${C_OFF}"
if ! curl -fL --progress-bar -o "$DMG_PATH" "$DMG_URL"; then
  err "下载失败。可以手动去下载页看看：https://github.com/$REPO/releases"
  exit 1
fi
ok "已下载 ($(du -h "$DMG_PATH" | cut -f1))"

# ---- 4. 挂载并安装 ----
echo ""
echo "安装到 $INSTALL_DIR"
mkdir -p "$MOUNT_POINT"
if ! hdiutil attach "$DMG_PATH" -mountpoint "$MOUNT_POINT" -nobrowse -quiet; then
  err "dmg 挂载失败（文件可能下载不完整），请重试。"
  exit 1
fi
if [ ! -d "$MOUNT_POINT/$APP_NAME.app" ]; then
  err "dmg 里没有找到 $APP_NAME.app"
  exit 1
fi

# 已有旧版本：先让它退出，再停掉后台服务
if [ -d "$INSTALL_DIR/$APP_NAME.app" ]; then
  echo "  检测到已安装的版本，先退出它"
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" 2>/dev/null || true
  sleep 2
fi
# ⚠ 用**新版本**的内核去停服务，并先确认它认识 stop 命令：
# 1.1.0 及以前的内核没有 stop，未知命令会直接在前台起一个网关，脚本就卡住了。
NEW_CORE="$MOUNT_POINT/$APP_NAME.app/Contents/Resources/agyhub"
if "$NEW_CORE" help 2>/dev/null | grep -q "stop-web"; then
  "$NEW_CORE" stop >/dev/null 2>&1 || true
fi

# /Applications 通常对管理员用户可写；不可写时提示用 sudo
if [ -w "$INSTALL_DIR" ]; then
  rm -rf "$INSTALL_DIR/$APP_NAME.app"
  cp -R "$MOUNT_POINT/$APP_NAME.app" "$INSTALL_DIR/"
else
  warn "$INSTALL_DIR 需要管理员权限，接下来会要求输入电脑密码"
  sudo rm -rf "$INSTALL_DIR/$APP_NAME.app"
  sudo cp -R "$MOUNT_POINT/$APP_NAME.app" "$INSTALL_DIR/"
  sudo chown -R "$(id -un)" "$INSTALL_DIR/$APP_NAME.app" || true
fi
ok "已安装"

# ---- 5. 去掉隔离标记 ----
# 没有 Apple 开发者签名的 App 带 com.apple.quarantine 会被 Gatekeeper 拦下。
xattr -dr com.apple.quarantine "$INSTALL_DIR/$APP_NAME.app" 2>/dev/null || \
  warn "去除隔离标记失败：首次打开如被拦，去「系统设置 → 隐私与安全性」点「仍要打开」"
ok "已解除 Gatekeeper 隔离"

# ---- 6. 启动 ----
echo ""
if open "$INSTALL_DIR/$APP_NAME.app" 2>/dev/null; then
  ok "已启动，看屏幕右上角菜单栏的 AgyHub 图标"
else
  warn "启动失败，请手动去「应用程序」里打开 AgyHub"
fi

echo ""
echo "接下来："
echo "  1. 第一次打开会提示安装证书，输入一次电脑密码"
echo "  2. 菜单栏点 AgyHub →「显示二维码」，手机（同一 Wi-Fi）扫码打开"
echo ""
echo "${C_DIM}卸载：先在菜单里「退出 AgyHub」，再 rm -rf /Applications/AgyHub.app ~/.agyhub${C_OFF}"
