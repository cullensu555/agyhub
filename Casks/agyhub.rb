# Homebrew Cask
#
# 本仓库本身就是 tap（cask 放在根目录的 Casks/ 下）。用户安装：
#   brew tap cullensu555/agyhub https://github.com/cullensu555/agyhub
#   brew install --cask cullensu555/agyhub/agyhub
#
# tap 时必须带地址：仓库名不是 homebrew-agyhub，不带地址 brew 找不到。
#
# ⚠ version / sha256 由 menubar/release.sh 在发版时自动改写，不要手改。
cask "agyhub" do
  version "1.2.3"
  sha256 "a0550341ba89609ca9ad7c2ca2a6a295a81fd8ce60607e81f72e572be9e6dd62"

  url "https://github.com/cullensu555/agyhub/releases/download/v#{version}/AgyHub-#{version}.dmg"
  name "AgyHub"
  desc "Antigravity 手机访问网关（菜单栏 App）"
  homepage "https://github.com/cullensu555/agyhub"

  # auto_updates：App 自己通过 Sparkle 更新，brew upgrade 默认不和它抢。
  # depends_on：只有 Apple Silicon（arm64）版本。
  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :ventura

  app "AgyHub.app"

  # 没有 Apple 开发者签名，去掉隔离标记让用户能直接打开。
  # 注意路径是 appdir（装好后的位置），不是 staged_path（解压的临时目录）。
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/AgyHub.app"]
  end

  # 卸载前停掉后台服务。
  # 先确认内核认识 stop 命令：1.1.0 及以前的内核没有 stop，
  # 未知命令会直接在前台起一个网关，brew uninstall 就卡住了。
  uninstall quit:   "local.agyhub.menubar",
            script: {
              executable:   "/bin/sh",
              args:         [
                "-c",
                "K='#{appdir}/AgyHub.app/Contents/Resources/agyhub'; " \
                "\"$K\" help 2>/dev/null | grep -q stop-web && \"$K\" stop; true",
              ],
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
