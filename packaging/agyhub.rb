# Homebrew Formula 模板
#
# 用法：把本文件放到你的 homebrew tap 仓库里
#   https://github.com/cullensu555/homebrew-agyhub/Casks/agyhub.rb
# 用户就能：
#   brew tap cullensu555/agyhub
#   brew install --cask agyhub     # 或是 brew install agyhub
#
# 注意这是 **cask**（图形 App），不是普通 formula。
# 所以文件名可以是 agyhub.rb 但内容用 cask 语法。

cask "agyhub" do
  version "1.0.0"
  sha256 "REPLACE_WITH_SHA256_OF_DMG"

  url "https://github.com/cullensu555/agyhub/releases/download/v#{version}/AgyHub-#{version}.dmg"
  name "AgyHub"
  desc "Antigravity 手机访问网关（菜单栏 App）"
  homepage "https://github.com/cullensu555/agyhub"

  # arm64（Apple Silicon）。Intel 机器需另出 x86_64 产物。
  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "AgyHub.app"

  # 未公证的 App 会被 Gatekeeper 拦。cask 装完自动去隔离标记，
  # 让用户双击就能开（否则要右键→打开）。
  #
  # 如果你将来买了开发者证书并公证了，这段可以删掉。
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{staged_path}/AgyHub.app"]
  end

  zap trash: [
    "~/.agyhub",
    "~/Library/Logs/AgyHub-debug.log",
    "~/Library/Logs/AgyHub-sshweb.log",
  ]

  caveats <<~EOS
    AgyHub 需要「登录时启动」才能常驻（菜单里可开关）。

    首次使用：
      1. 打开 AgyHub（菜单栏会出现图标）
      2. 点「安装证书」让浏览器信任本地 CA
      3. 点「复制链接」发到手机，用同一 Wi-Fi 打开

    SSH 网页版需要先配好 ~/.ssh/config 里的主机并确保密钥免密登录。
  EOS
end
