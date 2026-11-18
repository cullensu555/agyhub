#!/bin/bash
# 生成一个可分发的版本，并（可选）产出 Sparkle 的 appcast。
#
# 用法：
#   1) 首次：./release.sh --init-keys
#      生成 EdDSA 密钥对。公钥要写进 build.sh 的 SU_PUBLIC_KEY，
#      私钥会存在你的钥匙串里（Sparkle 用 Keychain 保管，不落地明文）。
#
#   2) 每次发版：./release.sh 0.2.0
#      产出 release/AgyHub-0.2.0.dmg 和一个 update/ 目录，
#      里面是要推到 GitHub Releases 的东西。
#
# 为什么用 EdDSA 而不是 RSA：
#   Sparkle 2 只支持 EdDSA（Ed25519）。它是 Apple 自家 CryptoKit
#   支持的算法，签名短、验证快，且不需要证书链。

set -euo pipefail
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

cd "$(dirname "$0")"
ROOT="$(pwd)"
VENDOR="$ROOT/vendor"
RELEASE_DIR="$ROOT/release"
UPDATE_DIR="$ROOT/update"
SPARKLE_KEYS_ACCOUNT="agyhub"

if [ "${1:-}" = "--init-keys" ]; then
  echo "==> 生成 Sparkle EdDSA 密钥对"
  echo
  echo "私钥将保存到你的登录钥匙串（account: $SPARKLE_KEYS_ACCOUNT）。"
  echo "公钥请复制到 build.sh 的 SU_PUBLIC_KEY。"
  echo
  "$VENDOR/bin/generate_keys" --account "$SPARKLE_KEYS_ACCOUNT"
  echo
  echo "✓ 完成。把上面输出的公钥填进 build.sh："
  echo "    SU_PUBLIC_KEY=xxxxx ./build.sh"
  echo "  或直接 export SU_PUBLIC_KEY=xxxxx"
  exit 0
fi

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  echo "用法: ./release.sh 0.2.0      # 发版"
  echo "      ./release.sh --init-keys # 首次生成密钥"
  exit 1
fi

echo "==> 发版 v$VERSION"

# 1. 构建（把版本号和更新源编进去）
if [ -z "${SU_PUBLIC_KEY:-}" ]; then
  echo "错误：没有设置 SU_PUBLIC_KEY。"
  echo "  首次请先跑 ./release.sh --init-keys 生成密钥对。"
  echo "  然后：export SU_PUBLIC_KEY=<公钥> 再重跑。"
  exit 1
fi

if [ -z "${SU_FEED_URL:-}" ]; then
  echo "错误：没有设置 SU_FEED_URL（appcast 的在线地址）。"
  echo "  例：export SU_FEED_URL=https://github.com/<你>/<仓库>/releases/latest/download/appcast-aarch64.xml"
  exit 1
fi

# ⚠️ 必须带 NO_INSTALL=1
#
# build.sh 默认会把产物装到 /Applications 并**删掉 build/ 里的副本**
# （为了不让聚焦搜索出现两个同名 App）。
# 但发版要拿 build/AgyHub.app 去打 dmg，删了就没了 ——
# 表现是 hdiutil 报 "No such file"，最终 release/ 目录空空如也。
NO_INSTALL=1 VERSION="$VERSION" SU_PUBLIC_KEY="$SU_PUBLIC_KEY" SU_FEED_URL="$SU_FEED_URL" \
  bash "$ROOT/build.sh"

APP="$ROOT/build/AgyHub.app"
if [ ! -d "$APP" ]; then
  echo "✗ 构建产物不存在: $APP"
  echo "  如果 build.sh 已经把它装到 /Applications 并删了 build/，"
  echo "  说明 NO_INSTALL=1 没传进去。"
  exit 1
fi
mkdir -p "$RELEASE_DIR" "$UPDATE_DIR"

# ---- 1.5 分发前检查：包内不能有外部脚本依赖 ----
#
# 这一步是"能不能发给别人用"的硬门槛。
#
# 历史教训：曾经 SSH 网页版靠工作区里的 agygate.py + .venv 干活、
# SSH 解锁靠 agy-ssh-unlock.cjs。开发机上跑得好好的，
# 但拷给别人就废了——对方没有 Python、没有 node、没有那个目录。
#
# 现在这些都已编译进 Rust 内核（include_str!），
# 所以这里一旦发现 .py/.cjs/.js 就说明有人退回去了，必须拦下。
echo "==> 检查包内是否残留外部脚本"
LEAK=0
while IFS= read -r f; do
  echo "  ⚠ 发现脚本: ${f#$APP/}"
  LEAK=1
done < <(find "$APP/Contents" \( -name "*.py" -o -name "*.cjs" -o -name "*.js" \) 2>/dev/null)

if [ "$LEAK" = "1" ]; then
  echo
  echo "✗ 包内还有脚本文件，分发出去对方用不了。"
  echo "  请确认这些逻辑已用 include_str! 编译进 Rust 内核（见 src/actions.rs）。"
  exit 1
fi
echo "  ✓ 无外部脚本依赖（纯二进制）"

# 2. 打 DMG
# 用 hdiutil create 的 -srcfolder，比先建空盘再拷简单，也不会有残留挂载点
DMG="$RELEASE_DIR/AgyHub-$VERSION.dmg"
echo "==> 打 DMG"
/bin/rm -f "$DMG"
# 盘里放 App + 「应用程序」的快捷方式：用户一看就知道要拖过去。
# 原来盘里只有一个 AgyHub.app，很多人直接在盘里双击运行 ——
# 推出安装盘 App 就没了，登录项和自动更新也都指向一个不存在的路径。
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "AgyHub" -srcfolder "$STAGE" -ov -format UDZO -quiet "$DMG"
/bin/rm -rf "$STAGE"
echo "  $DMG ($(du -h "$DMG" | cut -f1))"
SHA256=$(shasum -a 256 "$DMG" | cut -d' ' -f1)
echo "  sha256: $SHA256"

# 3. 签名
echo "==> 用 Sparkle 私钥签名"
SIG=$("$VENDOR/bin/sign_update" --account "$SPARKLE_KEYS_ACCOUNT" "$DMG" 2>/dev/null | sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p')
if [ -z "$SIG" ]; then
  # 老版本输出格式是纯签名串
  SIG=$("$VENDOR/bin/sign_update" --account "$SPARKLE_KEYS_ACCOUNT" "$DMG" 2>/dev/null | tail -1)
fi
if [ -z "$SIG" ]; then
  echo "签名失败。检查钥匙串里是否有 account=$SPARKLE_KEYS_ACCOUNT 的私钥。"
  exit 1
fi
SIZE=$(stat -f%z "$DMG")
echo "  签名: ${SIG:0:24}..."
echo "  大小: $SIZE"

# 4. 生成 appcast
echo "==> 生成 appcast-aarch64.xml"
PUBDATE=$(date -u "+%a, %d %b %Y %H:%M:%S +0000")
# 默认指向本项目真实仓库。换仓库时用 SU_DOWNLOAD_BASE 覆盖。
#
# ⚠️ 这里必须写真实地址：appcast 里这个 URL 是给**已安装用户**做更新用的，
# 留占位符意味着所有人的自动更新都会 404（而且很难发现，
# 因为只有"装了旧版的人"才会踩到）。
DEFAULT_REPO="cullensu555/agyhub"
DOWNLOAD_URL="${SU_DOWNLOAD_BASE:-https://github.com/$DEFAULT_REPO/releases/download/v$VERSION}/AgyHub-$VERSION.dmg"

cat > "$UPDATE_DIR/appcast-aarch64.xml" <<XML
<?xml version="1.0" standalone="yes"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
    <channel>
        <title>AgyHub</title>
        <item>
            <title>$VERSION</title>
            <pubDate>$PUBDATE</pubDate>
            <sparkle:version>$VERSION</sparkle:version>
            <sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>
            <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
            <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>
            <enclosure url="$DOWNLOAD_URL"
                       length="$SIZE"
                       type="application/octet-stream"
                       sparkle:edSignature="$SIG"/>
        </item>
    </channel>
</rss>
XML

cp "$DMG" "$UPDATE_DIR/"

# 5. 同步 Homebrew cask 的版本号和校验和（cask 里写死的旧值会让 brew 安装直接失败）
CASK="$ROOT/../packaging/agyhub.rb"
if [ -f "$CASK" ]; then
  sed -i '' -E "s/^  version \".*\"/  version \"$VERSION\"/; s/^  sha256 \".*\"/  sha256 \"$SHA256\"/" "$CASK"
  echo "==> 已更新 packaging/agyhub.rb（version $VERSION, sha256 ${SHA256:0:12}…）"
fi

echo
echo "✓ 完成"
echo
echo "要上传到 GitHub Releases（tag v${VERSION}）的文件："
echo "  $UPDATE_DIR/AgyHub-$VERSION.dmg"
echo "  $UPDATE_DIR/appcast-aarch64.xml"
echo
echo "appcast 里的下载地址是："
echo "  $DOWNLOAD_URL"
echo "  若仓库不是 $DEFAULT_REPO，用 SU_DOWNLOAD_BASE 覆盖："
echo "  export SU_DOWNLOAD_BASE=https://github.com/用户名/仓库名/releases/download/v$VERSION"
echo
echo "上传后，已安装的旧版本会在下次启动时自动看到更新。"
echo
