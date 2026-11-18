# AgyHub 菜单栏 App

把 Antigravity 网页访问网关做成菜单栏常驻应用。

架构参考 [codex-mixin](https://github.com/Edward-lyz/codex-mixin)，但保持独立实现。

## 架构：Rust 内核 + Swift 外壳

```
AgyHub.app/
├── Contents/MacOS/AgyHub              ← Swift 外壳（菜单栏、剪贴板、进程启停）
├── Contents/Resources/agyhub          ← Rust 内核（2.6 MB，服务器/证书/注入）
├── Contents/Resources/MenuIcon.png    ← 菜单栏模板图标（程序化生成）
└── Contents/Frameworks/
    └── Sparkle.framework              ← 静默自动更新
```

**为什么这样分**：

| 层 | 语言 | 理由 |
|---|---|---|
| 菜单栏 UI | Swift | macOS 原生 API（`NSStatusItem`、`NSMenu`、`UNUserNotification`）用 Swift 写最直接。硬用 Rust 要自己驱动 `NSApplication` 事件循环，容易卡死 |
| 代理内核 | Rust | 单文件分发、异步并发、流控正确性。已在 `agyhub-rs/` 完成并实测 |
| 自动更新 | Sparkle | macOS 上最成熟的方案，无需自建更新服务器 |

**关键原则**：Swift 只负责「显示」，事实由 `agyhub status` 的 JSON 提供。
不把网卡枚举、上游探测这些逻辑在 Swift 里重写一遍 —— 那就是两份实现，
改一处漏一处的经典开局。

## 构建

```bash
cd menubar
bash build.sh
open build/AgyHub.app
```

不需要 Xcode 工程文件，不需要 Apple 开发者证书。

## 首次运行要手动放行

`spctl` 会拒绝未公证的 App（`Signature=adhoc`，没有 Team ID）。
这是 macOS 的设计，绕不过去，**必须手动做一次**：

1. 打开「系统设置 → 隐私与安全性」
2. 滚到底部，会看到「已阻止使用 AgyHub」
3. 点「仍要打开」

或者命令行：

```bash
xattr -dr com.apple.quarantine build/AgyHub.app
```

**这一步只做一次**，之后升级（Sparkle 装的）不会再问。

## 功能

菜单结构（服务启动后会多出两个服务的开关）：

```
AgyHub  v0.1.0
● 正在运行 / ○ 服务未启动
────────────────
启动服务 / 停止服务
────────────────
☑ 启动网页版
    端口 8900（8898 被占用，已自动改）
    https://192.168.1.5:8900/?t=xxxx        ← 点一下复制
    在本机打开
☑ 启动 SSH
    端口 8897
    入口：Antigravity 里按 ⌘⇧P → 搜 ssh
────────────────
更换访问口令
  口令：xxxx（点击复制）
────────────────
安装根证书 / 登录时自动启动 / 查看日志 …
```

### 两个服务

| | 网页版 | SSH |
|---|---|---|
| 默认端口 | 8898 | 8897 |
| 实现 | Rust 内核（HTTPS/HTTP2 网关） | `agy-ssh-unlock.cjs`（CDP 注入） |
| 入口 | 浏览器访问 URL | Antigravity 命令面板 ⌘⇧P → ssh |
| 默认状态 | 开 | 开 |

### 端口智能选择

**自动模式**（推荐）：点「启动服务」时，如果 8898 被占用，自动往后找一个空闲端口，
并在菜单里标注「8898 被占用，已自动改」。

**手动模式**：点端口那一行可以输入任意端口。

- 端口被其他程序占用 → **明确报错**，告诉你「被谁占了」并建议可用端口
- 与另一个服务撞端口 → 拒绝并说明
- 小于 1024 → 拒绝（需要管理员权限）

### 口令持久化

口令存在 `~/.agyhub/config.json`。

**为什么要持久化**：PWA 装到桌面后，口令一变直接 401，用户收藏的链接全部失效。
这个坑踩过（项目记忆里的 `keep_token`）。

换口令后立刻写盘，重启不会变。手机上的旧链接会失效，需要重新复制。

### 端口被占用 ≠ 服务在跑

这是个容易混淆的点，代码里特意区分了：

- `port_owned_by_us(port)` —— 端口被**我们自己的进程**占着 → 服务在跑
- 端口被**别的程序**占着 → 服务没跑，`status` 里 `running=false`、`pid=0`

早期用 `!port_free(port)` 判断，结果 Python 版占着 8898 时菜单显示"正在运行"，
点启动因为 already-running 直接跳过 → 表现为"点了没反应"。

## 自动更新

基于 Sparkle 2，更新源是 GitHub Releases 的 appcast。用户不需要重新下载安装包。

### 第一次：生成签名密钥

```bash
cd menubar
./release.sh --init-keys
```

会输出一对 EdDSA（Ed25519）密钥：

- **公钥** → 填进构建配置
- **私钥** → 自动存进你的登录钥匙串（account `agyhub`），不落地明文

### 发版

```bash
export SU_PUBLIC_KEY="<上面生成的公钥>"
export SU_FEED_URL="https://github.com/<你>/<仓库>/releases/latest/download/appcast-aarch64.xml"
export SU_DOWNLOAD_BASE="https://github.com/<你>/<仓库>/releases/download/v0.2.0"

./release.sh 0.2.0
```

产出在 `release/` 和 `update/`：

- `AgyHub-0.2.0.dmg`
- `appcast-aarch64.xml`（含 EdDSA 签名）

把这两个文件上传到 GitHub Releases（tag 用 `v0.2.0`），已安装的旧版本
下次启动就能看到更新。

### 更新机制细节

Info.plist 里的关键配置：

| 键 | 值 | 作用 |
|---|---|---|
| `SUFeedURL` | GitHub Releases 的 appcast 地址 | 更新源 |
| `SUPublicEDKey` | EdDSA 公钥 | 验签 |
| `SUVerifyUpdateBeforeExtraction` | `true` | 先验签再解压，防篡改 |
| `SUAutomaticallyUpdate` | `false` | 默认提示后再装，不偷偷装 |
| `SUEnableAutomaticChecks` | `true` | 启动时自动检查 |

appcast 格式：

```xml
<item>
  <sparkle:version>0.2.0</sparkle:version>
  <sparkle:shortVersionString>0.2.0</sparkle:shortVersionString>
  <enclosure url=".../AgyHub-0.2.0.dmg"
             length="5013874"
             type="application/octet-stream"
             sparkle:edSignature="0r0dR/2GTP/..."/>
</item>
```

**为什么用 EdDSA 而不是 RSA**：Sparkle 2 只支持 Ed25519。它是 CryptoKit
原生支持的算法，签名短、验证快，不需要证书链。

## 踩过的坑

**1. `spctl` 拒绝启动**

`Signature=adhoc` 时 Gatekeeper 一律拒绝。`spctl --add` 在新版 macOS 已废弃
（返回 "This operation is no longer supported"）。只能靠用户手动放行一次。

**2. 进程被 SIGTERM 杀掉（退出码 137）**

会话沙箱会回收后台 GUI 进程。用 `open` 启动（走 launchd）而不是直接跑二进制
才能得到真正的独立 App 进程。

**3. 废纸篓里的僵尸实例**

会话里的 `rm` 是代理删除（移到废纸篓），旧 App 还在跑，导致「明明重新构建了
但行为没变」。排查时用 `lsof -c AgyHub` 看实际执行的路径。

**4. `set -u` 下空数组展开报错**

`"${ARR[@]}"` 在空数组时会报 `unbound variable`，必须写
`${ARR[@]+"${ARR[@]}"}`。

**5. Swift 顶层代码必须在文件末尾**

写在前面会在 `AppDelegate` 定义之前就执行。单文件编译时也不要用 `@main`
（会报 "cannot be used in a module that contains top-level code"）。

**6. 菜单栏图标必须是 template**

`img.isTemplate = true` 且图形只有纯黑 + alpha。彩色图在深色模式下看不清。

## 文件

| 文件 | 作用 |
|---|---|
| `Sources/main.swift` | 全部 Swift 代码（单文件，~430 行） |
| `build.sh` | 组装 .app（编译 Swift、拷内核、嵌 Sparkle、签名） |
| `release.sh` | 发版（打 DMG、EdDSA 签名、生成 appcast） |
| `vendor/Sparkle.framework` | Sparkle 2.10.1 |
| `vendor/bin/` | Sparkle 官方工具（`generate_keys`、`sign_update`） |
