# AgyHub

把本机运行的 [Antigravity](https://antigravity.google) 界面开放给手机、平板和局域网设备。

**菜单栏 App，下载即用。** 不需要装 Python、Node，也不需要改 Antigravity 本体。

```
AgyHub.app
├── MacOS/AgyHub         Swift 外壳（菜单栏 UI）
└── Resources/agyhub     Rust 内核（2.8 MB，所有逻辑都在这里）
```

内核是静态编译的单文件，注入脚本和中文词库都通过 `include_str!` 编进了二进制。
**整个 App 只依赖系统自带的 `ssh` 命令。**

---

## 安装

要求：Apple Silicon（M 系列芯片）的 Mac，macOS 13 或更高。

### 一行命令（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/cullensu555/agyhub/main/packaging/install.sh | bash
```

会自动下载最新版、装进「应用程序」、解除 Gatekeeper 隔离并打开。

### 手动下载

从 [Releases](../../releases) 下载 `AgyHub-<版本>.dmg`，打开后把 AgyHub 拖到旁边的「应用程序」。
**不要直接在安装盘里双击运行**（推出安装盘后 App 就没了）。

> **首次打开被拦？** 这个 App 没有 Apple 开发者签名，macOS 会提示无法验证。
> macOS 15 及以后「右键 → 打开」已经不管用了，二选一：
> - 打开「系统设置 → 隐私与安全性」，拉到底部点「仍要打开」
> - 或在终端执行：`xattr -dr com.apple.quarantine /Applications/AgyHub.app`

### Homebrew

```bash
brew tap cullensu555/agyhub https://github.com/cullensu555/agyhub
brew install --cask cullensu555/agyhub/agyhub
```

第一行**必须带仓库地址**：本仓库不叫 `homebrew-agyhub`，不带地址的话 brew 会去找一个不存在的仓库。
装好后 App 自己会通过「检查更新」升级，不需要 `brew upgrade`。

cask 文件在 `Casks/agyhub.rb`，`release.sh` 发版时会自动更新里面的版本号和校验和。

---

## 用起来

第一次打开：

1. 弹出欢迎提示 → 点「安装证书」，输入一次电脑密码（让浏览器信任本地证书）
2. 弹出二维码 → 手机（连同一个 Wi-Fi）用相机扫码打开
3. 手机提示「不安全」时，按页面底部的步骤给手机装一次证书
4. 想装成 App？Safari「分享 → 添加到主屏幕」/ Chrome「安装应用」

之后 AgyHub 会随开机自动启动，服务掉线会自动拉起。菜单里的开关：

| 开关 | 作用 | 需要什么 |
|---|---|---|
| **本机网页版** | 手机访问你这台 Mac 上的 Antigravity（默认开启） | 无 |
| **SSH 网页版** | 手机访问**开发机**的完整界面 | 配好 `~/.ssh/config` + 密钥免密 |
| **桌面版 SSH 入口** | 让电脑上的 Antigravity 出现「Connect to SSH Host…」 | 打开时 Antigravity 窗口会刷新一次 |
| **中文界面（桌面版）** | 电脑上的 Antigravity 显示中文，重启后自动恢复 | 无 |

每个服务下面有：链接（点击在本机打开）、复制链接、显示二维码、端口。
启动失败时，原因直接显示在菜单里，点击可以看完整说明。

**退出 AgyHub 会同时停止所有服务**（不会在后台偷偷开着）。

**打不开？网关会告诉你为什么。** 如果 Antigravity 没装、没打开或没登录、
开发机断线，页面会显示对应的引导。操作完点「重新检测」即可。

---

## 端口

| 端口 | 谁 | 说明 |
|---|---|---|
| 8898 | 本机网页版 | 转发本机的 Antigravity |
| 8897 | SSH 网页版 | 隧道到开发机，装到手机叫 **Agy SSH** |

「启动 SSH」（桌面版入口解锁）**不占端口**，它只是通过 CDP 注入让界面出现入口。

端口可以在菜单里改。启动时端口被别的程序临时占用，会自动用下一个空闲端口，
但**不会改掉配置**：占用解除后下次启动自动回到原端口，手机上收藏的链接不会失效。

---

## 为什么需要证书

浏览器要求 HTTPS 才给 HTTP/2（Antigravity 前端同时开 6 个流式订阅，
HTTP/1.1 下会把连接槽位占死，表现为"页面能看、一操作就卡"）。

所以网关自带一个**本地根 CA**，用它签发服务器证书：

```bash
agyhub trust-cert     # 装进钥匙串
agyhub doctor         # 检查是否已信任
```

手机端首次访问会提示不安全，选择「继续访问」即可。
想去掉提示、装成 PWA，需要在手机上装根证书（`https://<电脑IP>:8898/__agy/ca.crt`），
页面底部会给出 iPhone / 安卓各自的步骤。**iPhone 装完描述文件后，还要去
「设置 → 通用 → 关于本机 → 证书信任设置」打开开关**，这一步最常被漏掉。

本机 IP 变了（换 Wi-Fi）时，网关会在 30 秒内自动换上新证书，不用重启。

---

## 命令行

App 里的内核也能直接用：

```bash
A=/Applications/AgyHub.app/Contents/Resources/agyhub
$A status                        # 各服务状态（单行 JSON）
$A start-web | stop-web          # 后台启停本机网页版（幂等，重复调用不会多起进程）
$A start-ssh-web --target dev    # 后台启动 SSH 网页版（--wait 30 等到连上为止）
$A stop                          # 停止全部服务
$A ssh-hosts                     # 列出 ~/.ssh/config 里的主机（支持 Include）
$A zh on|off                     # 桌面版中文界面
$A ssh-unlock [off]              # 桌面版 SSH 入口
$A trust-cert                    # 安装根证书
$A rotate-token                  # 更换访问口令（立即生效，不用重启）
$A doctor                        # 自检：Antigravity、证书、端口、服务、上次失败原因
```

服务进程登记在 `~/.agyhub/run/*.json`（pid、端口），状态判断只用系统调用，
不依赖 `ps` / `pgrep` / `openssl`（这些在部分公司电脑上会被管控软件拦截）。

---

## 开发环境

### 必需

| 工具 | 版本要求 | 用途 | 获取方式 |
|---|---|---|---|
| **Xcode** | 15+ | 编译 Swift 外壳、提供 macOS SDK | App Store 搜「Xcode」 |
| **Xcode Command Line Tools** | — | `swiftc`、`codesign`、`ditto`、`hdiutil` | `xcode-select --install` |
| **Rust** | 1.75+ | 编译内核 | 见下方 |
| **Git** | — | 版本控制 | `xcode-select --install` 自带 |

> 本项目在 **Rust 1.100 / Swift 6.4 / macOS 26** 上验证通过。
> 版本门槛来自依赖：`rustls 0.23` 要求 Rust 1.71+，`h2 0.4` 要求 1.63+。

### 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**不要用 `brew install rust`** —— Homebrew 版本经常落后，且升级时可能
静默换掉工具链，导致 `cargo build` 突然报奇怪的错误。

### 可选（只有发版才需要）

| 工具 | 用途 | 获取方式 |
|---|---|---|
| **Sparkle** 2.x | 自动更新框架 + 签名/appcast 工具 | 见下方 |
| **GitHub CLI**（`gh`） | 命令行创建 release、上传产物 | `brew install gh` |

#### 获取 Sparkle

`release.sh` 需要 Sparkle 的四个命令行工具：
`generate_keys`、`sign_update`、`generate_appcast`、`BinaryDelta`。

它们**不在版本库里**（107MB，且能从官网重下）。手动准备：

```bash
cd menubar
# 下载（换成你需要的版本）
curl -L -o sparkle.tar.xz \
  https://github.com/sparkle-project/Sparkle/releases/download/2.10.1/Sparkle-2.10.1.tar.xz

mkdir -p vendor && tar -xJf sparkle.tar.xz -C vendor
# 解压后应能看到 vendor/bin/ 和 vendor/Sparkle.framework/
ls vendor/bin
```

> 版本号必须与 `build.sh` 里拷贝的 `Sparkle.framework` 一致，
> 否则签名验不过。本项目用的是 **2.10.1**。

---

## 从源码构建

```bash
# 1. 内核
cd agyhub-rs
cargo build --release
# 产物 target/release/agyhub（约 2.8MB）

# 2. App（会自动把内核拷进去）
cd ../menubar
bash build.sh
# 产物 build/AgyHub.app，同时装到 /Applications/
```

只想用不想开发的话，`bash build.sh` 一条命令就够 —— 它会先编译内核、
检查注入脚本的 JS 语法，再打包 App 并装到 `/Applications`（会先退出旧版本）。

### 发版

```bash
cd menubar

# 首次：生成签名的密钥对（只做一次）
# 私钥存进登录钥匙串（account: agyhub），公钥会打印出来
./release.sh --init-keys

# 发版
export SU_PUBLIC_KEY=<上一步打印的公钥>
export SU_FEED_URL=https://github.com/cullensu555/agyhub/releases/latest/download/appcast-aarch64.xml
./release.sh 1.0.0
```

产出：

```
menubar/release/AgyHub-1.0.0.dmg      分发包
menubar/update/appcast-aarch64.xml    自动更新的索引
```

`release.sh` 会做一道**硬门槛检查**：包内不能有 `.py`/`.cjs`/`.js`。
这是"能不能发给别人用"的底线 —— 一旦发现就 `exit 1`。

### 上传到 GitHub

```bash
gh release create v1.0.0 \
  menubar/release/AgyHub-1.0.0.dmg \
  menubar/update/appcast-aarch64.xml \
  --title "AgyHub 1.0.0" \
  --notes "首个公开版本"
```

没有 `gh` 的话去网页操作：仓库 → Releases → Draft a new release，
tag 填 `v1.0.0`，把两个文件拖进去。

> ⚠️ **tag 名必须是 `v1.0.0`（带 v）**，因为 appcast 和 Homebrew 里的
> 下载地址都是按 `/releases/download/v1.0.0/` 拼的。

---

## 常见问题

**构建报「找不到 agyhub」**
先跑 `cargo build --release`。`build.sh` 会从 `../agyhub-rs/target/release/`
找内核，没有就失败。

**`release.sh` 报「签名失败」**
钥匙串里没有私钥。跑 `./release.sh --init-keys` 生成；
如果已经有但换了机器，私钥没法迁移，只能重新生成（老用户需要重装一次）。

**App 打开后菜单栏没图标**
看 `~/Library/Logs/AgyHub-debug.log`。常见原因是内核没打包进去。

**改了 Rust 代码但行为没变**
`build.sh` 会重编内核；App 下次启动时会自动替换掉旧内核起的服务进程。
确认版本：`/Applications/AgyHub.app/Contents/Resources/agyhub --version`。

**手机打不开链接**
按顺序排查：
1. 手机和电脑是不是同一个 Wi-Fi（访客网络、公司 Wi-Fi 常常禁止设备互访）
2. 菜单里有没有「防火墙拦截」的提示 —— 有就按提示放行
3. 电脑开着 VPN 时，链接里的 IP 可能不对；菜单里的链接会优先用 Wi-Fi / 有线网卡的地址
4. 点「查看日志（含自检）」，把生成的 `AgyHub-汇总.txt` 发过来（口令已自动隐藏）

---

## 目录

```
agyhub-rs/          Rust 内核
├── src/
│   ├── main.rs     命令分发、服务启停、status
│   ├── state.rs    服务登记（pid 文件）、注入巡检状态
│   ├── sys.rs      带超时的外部命令、进程查询、随机数
│   ├── server.rs   HTTP/2 网关
│   ├── proxy.rs    上游 HTTP/1.1 客户端
│   ├── tunnel.rs   SSH 隧道（替代原 Python 实现）
│   ├── cdp.rs      CDP 客户端（手写 WebSocket，替代 node 脚本）
│   ├── actions.rs  桌面版注入巡检：SSH 入口、中文界面
│   ├── inject.rs   HTML 注入（品牌、manifest、中文、引导页）
│   ├── cert.rs     本地 CA 与服务器证书（可热替换）
│   ├── trust.rs    把根证书装进钥匙串
│   ├── config.rs   配置
│   └── upstream.rs 上游端口发现
└── assets/         编译进二进制的注入脚本与词库

menubar/            Swift 外壳
├── Sources/main.swift
├── build.sh
└── release.sh

packaging/          Homebrew cask 与安装脚本
docs/customflags.md 网页版 101 个功能开关的调研清单（含哪些打不开）
```

---

## 已知限制

- **只支持 Apple Silicon**（arm64）。Intel 机器需要另出产物。
- **没有 Apple 签名**，首次打开需手动放行；开着 macOS 防火墙的话，每次升级后可能要再点一次「允许传入连接」。
- **SSH 网页版的 AI 推理由远端发起** —— 开发机自己必须能访问
  `generativelanguage.googleapis.com`，否则界面能用、一问 AI 就超时。
- 网页版有前端硬编码的附件上限（文本 1 MB、PDF/文档 10 MB、
  Zip/音频/图片 20 MB）。**大文件让 agent 按路径自己读，不要当附件传。**
- 依赖 Antigravity 未公开的本地接口，**随版本升级可能失效**（当前验证于 2.32.2）。

---

## 许可

MIT
