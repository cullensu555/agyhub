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

### Homebrew（推荐）

```bash
brew tap cullensu555/agyhub
brew install --cask agyhub
```

### 一行命令

```bash
curl -fsSL https://raw.githubusercontent.com/cullensu555/agyhub/main/packaging/install.sh | bash
```

### 手动下载

从 [Releases](../../releases) 下载 `AgyHub-<版本>.dmg`，拖进「应用程序」。

> **首次打开被拦？** 这个 App 没有 Apple 开发者签名，Gatekeeper 会提示
> 「无法验证开发者」。右键点 App → 打开，或执行：
> ```bash
> xattr -dr com.apple.quarantine /Applications/AgyHub.app
> ```

---

## 用起来

打开 AgyHub，菜单栏出现图标。三个开关：

| 开关 | 作用 | 需要什么 |
|---|---|---|
| **本机网页版** | 手机访问你这台 Mac 上的 Antigravity | 无 |
| **SSH 网页版** | 手机访问**开发机**的完整界面 | 配好 `~/.ssh/config` + 密钥免密 |
| **启动 SSH** | 让**桌面版** Antigravity 出现「Connect to SSH Host…」 | 无 |

流程：

1. 点「安装证书」→ 让浏览器信任本地 CA（只需一次）
2. 点链接在浏览器里打开，或点「复制链接」发到手机
3. 手机连同一个 Wi-Fi，打开链接
4. 想装成 App？Safari/Chrome 里「添加到主屏幕」

**打不开？网关会告诉你为什么。** 如果 Antigravity 没装、没打开或没登录，
页面会显示对应的引导（而不是一句 `not running`）。操作完点「重新检测」即可。

---

## 端口

| 端口 | 谁 | 说明 |
|---|---|---|
| 8898 | 本机网页版 | 转发本机的 Antigravity |
| 8897 | SSH 网页版 | 隧道到开发机，装到手机叫 **Agy SSH** |

「启动 SSH」（桌面版入口解锁）**不占端口**，它只是通过 CDP 注入让界面出现入口。

端口可以在菜单里改。被占用时会自动找下一个空闲的。

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
想装成 PWA 的话需要装根证书（`https://<电脑IP>:8898/__agy/ca.crt`）。

---

## 命令行

App 里的内核也能直接用：

```bash
agyhub status              # 各服务状态（单行 JSON）
agyhub start-ssh-web --target dev   # 起 SSH 网页版
agyhub ssh-hosts           # 列出 ~/.ssh/config 里的主机
agyhub zh on|off           # 中文界面
agyhub ssh-unlock          # 解锁桌面版 SSH 入口
agyhub trust-cert          # 安装根证书
agyhub doctor              # 自检
```

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

只想用不想开发的话，`bash build.sh` 一条命令就够 —— 它会先检查内核是否
需要重编。

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
`build.sh` 不会自动重编内核。先 `cargo build --release` 再 `bash build.sh`。
判断办法：`cmp agyhub-rs/target/release/agyhub /Applications/AgyHub.app/Contents/Resources/agyhub`。

---

## 目录

```
agyhub-rs/          Rust 内核
├── src/
│   ├── main.rs     命令分发、服务生命周期
│   ├── server.rs   HTTP/2 网关
│   ├── tunnel.rs   SSH 隧道（替代原 Python 实现）
│   ├── cdp.rs      CDP 客户端（手写 WebSocket，替代 node 脚本）
│   ├── actions.rs  SSH 解锁、中文注入、界面还原
│   ├── inject.rs   HTML 注入（品牌、manifest、中文）
│   ├── cert.rs     本地 CA 与服务器证书
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
- **没有 Apple 签名**，首次打开需手动放行。
- **SSH 网页版的 AI 推理由远端发起** —— 开发机自己必须能访问
  `generativelanguage.googleapis.com`，否则界面能用、一问 AI 就超时。
- 网页版有前端硬编码的附件上限（文本 1 MB、PDF/文档 10 MB、
  Zip/音频/图片 20 MB）。**大文件让 agent 按路径自己读，不要当附件传。**
- 依赖 Antigravity 未公开的本地接口，**随版本升级可能失效**（当前验证于 2.32.2）。

---

## 许可

MIT
