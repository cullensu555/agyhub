# Homebrew Cask
#
# 用法：把本文件放到 homebrew tap 仓库里
#   https://github.com/cullensu555/homebrew-agyhub/Casks/agyhub.rb
# 用户就能：
#   brew tap cullensu555/agyhub
#   brew install --cask agyhub
#
# ⚠ version / sha256 由 menubar/release.sh 在发版时自动改写，不要手改。
cask "agyhub" do
  version "1.1.0"
  sha256 "3e1803ba0fface3f6e0a78bd93a653de551a842e9fc6530a71eb59b5d71ac7bf"

  url "https://github.com/cullensu555/agyhub/releases/download/v#{version}/AgyHub-#{version}.dmg"
  name "AgyHub"
  desc "Antigravity 手机访问网关（菜单栏 App）"
  homepage "https://github.com/cullensu555/agyhub"

  # 只有 Apple Silicon（arm64）版本
  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "AgyHub.app"

  # 没有 Apple 开发者签名，去掉隔离标记让用户能直接打开。
  # 注意路径是 appdir（装好后的位置），不是 staged_path（解压的临时目录）。
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/AgyHub.app"]
  end

  # 卸载前停掉后台服务
  uninstall quit:   "local.agyhub.menubar",
            script: {
              executable:   "#{appdir}/AgyHub.app/Contents/Resources/agyhub",
              args:         ["stop"],
              must_succeed: false,
            }

  zap trash: [
    "~/.agyhub",
    "~/Library/Logs/AgyHub*.log*",
    "~/Library/Logs/AgyHub-汇总.txt",
    "~/Library/Preferences/local.agyhub.menubar.plist",
  ]

  caveats <<~EOS
    首次使用：
      1. 打开 AgyHub（屏幕右上角菜单栏会出现图标）
      2. 按提示安装证书（输入一次电脑密码）
      3. 菜单里点「显示二维码」，手机（同一 Wi-Fi）扫码打开
    SSH 网页版需要先配好 ~/.ssh/config 里的主机并能免密登录。
  EOS
end
