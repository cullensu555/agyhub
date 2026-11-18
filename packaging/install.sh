#!/bin/bash
# ============================================================
#  AgyHub 一键安装
#
#  用法：
#     curl -fsSL https://raw.githubusercontent.com/cullensu555/agyhub/main/packaging/install.sh | bash
#
#  做的事：
#     1. 检测 CPU 架构（arm64 / x86_64）
#     2. 从 GitHub Releases 下载对应版本的 dmg
#     3. 挂载 → 拷到 /Applications → 卸载
#     4. 去掉 Gatekeeper 隔离标记（未公证的 App 否则打不开）
#     5. 启动 App
#
#  ## 为什么不直接 curl 二进制
#
#  因为这是个**菜单栏 App**，用户要的是能双击的 .app，
#  不是扔在 /usr/local/bin 里的命令行工具。所以用 dmg。
#  （命令行部分已经集成在 App 内，通过 `agyhub` 内核调用。）
# ============================================================

set -euo pipefail

REPO="${AGYHUB_REPO:-cullensu555/agyhub}"
APP_NAME="AgyHub"
INSTALL_DIR="/Applications"

C_OK=$'\033[32m'; C_ERR=$'\033[31m'; C_WARN=$'\033[33m'
C_DIM=$'\033[2m'; C_OFF=$'\033[0m'

say() { echo "$@"; }
ok()  { echo "${C_OK}✓${C_OFF} $*"; }
err() { echo "${C_ERR}✗${C_OFF} $*" >&2; }
warn(){ echo "${C_WARN}!${C_OFF} $*"; }

# ---- 1. 环境检查 ----
if [ "$(uname -s)" != "Darwin" ]; then
  err "这个安装脚本只支持 macOS。"
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  arm64)  SUFFIX="aarch64" ;;
  x86_64) SUFFIX="x86_64" ;;
  *) err "不支持的架构: $ARCH"; exit 1 ;;
esac
ok "系统: macOS $ARCH"

# ---- 2. 取最新版本号 ----
say ""
say "查询最新版本…"
API="https://api.github.com/repos/$REPO/releases/latest"
VERSION=""
if command -v curl >/dev/null 2>&1; then
  VERSION=$(curl -fsSL "$API" 2>/dev/null \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1) || true
fi

if [ -z "$VERSION" ]; then
  # 拿不到就退到固定版本，避免脚本直接失败
  warn "无法查询最新版本，改用已知版本"
  VERSION="1.0.0"
fi
ok "版本: $VERSION"

# ---- 3. 下载 ----
DMG_URL="https://github.com/$REPO/releases/download/v$VERSION/AgyHub-$VERSION-$SUFFIX.dmg"
TMP_DIR="$(mktemp -d)"
DMG_PATH="$TMP_DIR/AgyHub.dmg"

say ""
say "下载 ${C_DIM}$DMG_URL${C_OFF}"
if ! curl -fL --progress-bar -o "$DMG_PATH" "$DMG_URL"; then
  err "下载失败。可能这个版本没有 $SUFFIX 产物，或仓库地址不对。"
  err "你可以手动去下载页看看：https://github.com/$REPO/releases"
  rm -rf "$TMP_DIR"
  exit 1
fi
ok "已下载 ($(du -h "$DMG_PATH" | cut -f1))"

# ---- 4. 挂载并安装 ----
say ""
say "安装到 $INSTALL_DIR"
MOUNT_POINT="$TMP_DIR/mnt"
mkdir -p "$MOUNT_POINT"

# -nobrowse：不弹访达窗口；-quiet：安静挂载
hdiutil attach "$DMG_PATH" -mountpoint "$MOUNT_POINT" -nobrowse -quiet

cleanup() {
  hdiutil detach "$MOUNT_POINT" -quiet 2>/dev/null || true
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

if [ ! -d "$MOUNT_POINT/$APP_NAME.app" ]; then
  err "dmg 里没有找到 $APP_NAME.app"
  exit 1
fi

# 已有旧版本：先退出再替换，否则 macOS 会拒绝覆盖正在运行的 App
if [ -d "$INSTALL_DIR/$APP_NAME.app" ]; then
  say "  检测到已安装的版本，先退出它"
  osascript -e "tell application \"$APP_NAME\" to quit" 2>/dev/null || true
  sleep 1
  rm -rf "$INSTALL_DIR/$APP_NAME.app"
fi

cp -R "$MOUNT_POINT/$APP_NAME.app" "$INSTALL_DIR/"
ok "已安装"

# ---- 5. 去掉隔离标记 ----
# 未公证的 App 带 com.apple.quarantine 会被 Gatekeeper 拦下，
# 提示"无法验证开发者"。这一步让它双击就能开。
# （如果你将来买了证书并公证，这里可以保留，无害。）
if xattr -p com.apple.quarantine "$INSTALL_DIR/$APP_NAME.app" >/dev/null 2>&1; then
  xattr -dr com.apple.quarantine "$INSTALL_DIR/$APP_NAME.app" 2>/dev/null || {
    warn "去除隔离标记失败，首次打开请右键 → 打开"
  }
fi
ok "已解除 Gatekeeper 隔离"

# ---- 6. 启动 ----
say ""
if open "$INSTALL_DIR/$APP_NAME.app" 2>/dev/null; then
  ok "已启动，看菜单栏图标"
else
  warn "启动失败，请手动去 应用程序 里打开"
fi

say ""
say "完成。接下来："
say "  1. 菜单栏点 ${C_OK}AgyHub${C_OFF} 图标"
say "  2. 点「安装证书」（让浏览器信任本地 CA）"
say "  3. 点「复制链接」发到手机，同一 Wi-Fi 打开即可"
say ""
say "${C_DIM}卸载：rm -rf /Applications/AgyHub.app ~/.agyhub${C_OFF}"
