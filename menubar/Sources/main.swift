// AgyHub —— Antigravity 网页访问网关的菜单栏外壳
//
// 分工：
//   这个文件（Swift）只做 macOS 原生部分：菜单栏图标、菜单、剪贴板、
//   进程启停、通知。
//   真正的代理内核是 Resources/agyhub（Rust），它负责服务器、证书、注入。
//
// 为什么不让 Swift 干所有事：
//   网卡枚举、上游端口探测、端口占用判断、证书签发，这些在 Rust 里
//   已经写好并实测过了。Swift 重做一遍就是两份实现，改一处漏一处的经典开局。
//   所以 Swift 只「显示」，事实由 `agyhub status` 的 JSON 提供。

import Cocoa
import SwiftUI
import Sparkle
import UserNotifications
import ServiceManagement

// MARK: - 状态模型

/// 单个服务的状态。
struct ServiceStatus: Decodable {
    var enabled: Bool
    var running: Bool
    var port: Int
    var preferredPort: Int
    var pid: Int
    var url: String
    /// 仅 SSH 网页版有：转发到哪台开发机
    var target: String?

    enum CodingKeys: String, CodingKey {
        case enabled, running, port, pid, url, target
        case preferredPort = "preferred_port"
    }

    /// offline 兜底
    static func stub() -> ServiceStatus {
        ServiceStatus(
            enabled: true, running: false, port: 0,
            preferredPort: 0, pid: 0, url: "", target: nil
        )
    }
}

/// 中文语言包状态。
struct ZhStatus: Decodable {
    var enabled: Bool
    var running: Bool
    var pid: Int
}

/// `agyhub status` 的输出。
struct HubStatus: Decodable {
    var version: String
    var running: Bool
    var port: Int
    var upstreamOk: Bool
    var upstreamPort: Int
    var lanIp: String
    var token: String
    var url: String
    var caTrusted: Bool
    var caFingerprint: String
    var pid: Int
    var web: ServiceStatus
    var ssh: ServiceStatus
    var sshWeb: ServiceStatus
    var bothOn: Bool
    var tokenPersisted: Bool
    var zh: ZhStatus

    enum CodingKeys: String, CodingKey {
        case version, running, port, token, url, pid, web, ssh
        case upstreamOk = "upstream_ok"
        case upstreamPort = "upstream_port"
        case lanIp = "lan_ip"
        case caTrusted = "ca_trusted"
        case caFingerprint = "ca_fingerprint"
        case bothOn = "both_on"
        case tokenPersisted = "token_persisted"
        case sshWeb = "ssh_web"
        case zh
    }

    /// 兜底：内核没跑或 status 失败时用，避免界面全是 nil。
    static func offline(port: Int) -> HubStatus {
        let empty = ServiceStatus.stub()
        return HubStatus(
            version: "-", running: false, port: port,
            upstreamOk: false, upstreamPort: 0,
            lanIp: "127.0.0.1", token: "", url: "",
            caTrusted: false, caFingerprint: "", pid: 0,
            web: empty, ssh: empty, sshWeb: empty,
            bothOn: false, tokenPersisted: false,
            zh: ZhStatus(enabled: false, running: false, pid: 0)
        )
    }
}

// MARK: - 内核调用

/// 找到 Rust 内核。
func corePath() -> String? {
    let fm = FileManager.default
    var candidates: [String] = []

    if let res = Bundle.main.resourcePath {
        candidates.append(res + "/agyhub")
    }
    var dir = Bundle.main.bundleURL
    for _ in 0..<4 {
        candidates.append(dir.path + "/agyhub-rs/target/release/agyhub")
        dir = dir.deletingLastPathComponent()
    }
    // ⚠️ 不写死任何绝对路径。
    //
    // 上面两条已经覆盖了所有正当场景：
    //   · 正式分发：内核在 Bundle.main.resourcePath 里（打包时拷进去）
    //   · 本地开发：从 build/ 往上能找到 agyhub-rs/target/release/
    //
    // 早先这里还有一条写死的 `<开发机上的个人目录>`，
    // 那是开发机的个人路径 —— 分包出去毫无意义，还泄露目录结构。已去掉。

    for c in candidates where fm.isExecutableFile(atPath: c) {
        return c
    }
    return nil
}

/// 同步跑一次内核命令，返回 stdout 数据。
///
/// ⚠️ 为什么用**临时文件**而不是 Pipe：
///   早先的实现用 `Pipe()` + `readDataToEndOfFile()`，在 GUI 进程里
///   偶尔会拿不到任何数据（菜单显示 `v-`、状态永远"未启动"）。
///   GUI 进程没有终端，子进程的管道缓冲区行为和终端里跑不同，
///   加上 readDataToEndOfFile 与 waitUntilExit 的调用顺序问题，
///   很容易死锁或读到空。
///   改成重定向到临时文件、跑完再读，稳定可靠。
func runCore(_ args: [String]) -> Data? {
    guard let core = corePath() else { return nil }
    let tmp = FileManager.default.temporaryDirectory
        .appendingPathComponent("agyhub-out-\(UUID().uuidString).json")

    FileManager.default.createFile(atPath: tmp.path, contents: nil)
    guard let fh = FileHandle(forWritingAtPath: tmp.path) else { return nil }

    let p = Process()
    p.executableURL = URL(fileURLWithPath: core)
    p.arguments = args
    p.standardOutput = fh
    // stderr 必须重定向到别处，否则会阻塞（管道满了没人读）
    p.standardError = FileHandle(forWritingAtPath: "/dev/null")

    do {
        try p.run()
    } catch {
        try? fh.close()
        try? FileManager.default.removeItem(at: tmp)
        return nil
    }
    p.waitUntilExit()
    try? fh.close()

    let data = try? Data(contentsOf: tmp)
    try? FileManager.default.removeItem(at: tmp)
    return data
}

/// 写诊断日志。
///
/// 为什么需要：GUI 进程的 stdout 会被系统吞掉，出了问题看不到任何线索。
/// 菜单显示 `v-`（读不到状态）和「点启动没反应」都没有报错弹窗，
/// 只能靠日志。
func dlog(_ msg: String) {
    let dir = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("Logs")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let url = dir.appendingPathComponent("AgyHub-debug.log")
    let line = "\(ISO8601DateFormatter().string(from: Date())) \(msg)\n"
    if let h = try? FileHandle(forWritingTo: url) {
        h.seekToEndOfFile()
        h.write(line.data(using: .utf8)!)
        h.closeFile()
    } else {
        try? line.write(to: url, atomically: true, encoding: .utf8)
    }
}

func fetchStatus(port: Int) -> HubStatus? {
    guard let data = runCore(["--port", String(port), "status"]) else {
        dlog("fetchStatus: runCore 返回 nil（corePath=\(corePath() ?? "nil")）")
        return nil
    }
    do {
        return try JSONDecoder().decode(HubStatus.self, from: data)
    } catch {
        let raw = String(data: data, encoding: .utf8) ?? "<非文本>"
        dlog("fetchStatus: 解码失败 \(error)；原始输出前 200 字：\(raw.prefix(200))")
        return nil
    }
}

// MARK: - 应用主体

final class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var status: HubStatus = .offline(port: 8898)
    var coreProcess: Process?
    var refreshTimer: Timer?
    var updaterController: SPUStandardUpdaterController?
    var port = 8898

    /// 每个服务的最近一次"自动重启"时间，用于退避。
    /// 防止服务起不来时每 3 秒重启一次，把系统拖垮。
    var lastAutoStart: [String: Date] = [:]
    /// 自动重启的最小间隔（秒）
    let autoStartCooldown: TimeInterval = 20

    func applicationDidFinishLaunching(_ notification: Notification) {
        setupStatusItem()
        refresh()

        // ---- 需求：打开 App 就自动把服务拉起来，不用用户手动点 ----
        //
        // 判断条件用 status.web.running（而不是「配置里 enabled」）：
        // 用户可能手动停过服务，那种情况不该强行拉起。
        // 只在"确实没在跑"且"配置里是启用状态"时才自动启动。
        ensureServicesRunning()

        // ---- 需求：登录时自动启动默认勾选 ----
        // 第一次运行时自动注册登录项，尽量少让用户手动操作。
        // 用 UserDefaults 记住"已经处理过一次"，避免用户主动取消后又被自动打开。
        enableLaunchAtLoginIfFirstRun()

        refreshTimer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            self?.refresh()
            self?.guardServices()
        }

        if let feed = Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") as? String,
           !feed.isEmpty,
           let key = Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String,
           !key.isEmpty {
            updaterController = SPUStandardUpdaterController(
                startingUpdater: true,
                updaterDelegate: nil,
                userDriverDelegate: nil
            )
        }
    }

    // MARK: 图标

    func setupStatusItem() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = statusItem.button {
            button.image = menuBarImage()
            button.imageScaling = .scaleProportionallyDown
        }
        rebuildMenu()
    }

    /// 菜单栏图标。
    ///
    /// ⚠️ 不能设 isTemplate = true：模板图会被系统强制渲染成单色剪影，
    /// 摆在 Docker / iStat 那类彩色图标中间格格不入。
    func menuBarImage() -> NSImage? {
        let pt: CGFloat = 18
        if let img = Bundle.main.image(forResource: "MenuIcon") {
            img.isTemplate = false
            img.size = NSSize(width: pt, height: pt)
            return img
        }
        let sym = NSImage(
            systemSymbolName: "antenna.radiowaves.left.and.right",
            accessibilityDescription: "AgyHub"
        )
        sym?.isTemplate = true
        return sym
    }

    func updateIcon() {
        guard let button = statusItem.button else { return }
        button.image = menuBarImage()
        button.toolTip = status.running ? "AgyHub · 正在运行" : "AgyHub · 未启动"
    }

    /// 打开 App 时自动拉起服务。
    ///
    /// 为什么不做成"无条件启动"：
    ///   用户可能刚点了「停止服务」，切走再切回来 App 又被拉起会很烦。
    ///   这里只在「配置里是开启状态」且「实际没在跑」时启动。
    func ensureServicesRunning() {
        let needWeb = status.web.enabled && !status.web.running
        let needSSHWeb = status.sshWeb.enabled && !status.sshWeb.running
        let needSSH = status.ssh.enabled && status.ssh.pid == 0

        guard needWeb || needSSHWeb || needSSH else {
            dlog("ensureServicesRunning: 跳过 | web(en=\(status.web.enabled) run=\(status.web.running)) sshWeb(en=\(status.sshWeb.enabled) run=\(status.sshWeb.running) port=\(status.sshWeb.port)) ssh(en=\(status.ssh.enabled) pid=\(status.ssh.pid))")
            return
        }
        dlog("ensureServicesRunning: web=\(needWeb) sshWeb=\(needSSHWeb) ssh=\(needSSH)")
        startAll()
    }

    /// 守护各个服务：掉线了就自动拉起来。
    ///
    /// 为什么需要这个（用户的原话：「关掉你又没重启，怎么会继续生效？」）：
    ///   之前只在 App 启动时跑一次 ensureServicesRunning，
    ///   之后服务挂了就再也不管了 —— 表现就是"明明说会自动启动，
    ///   结果过一会儿又打不开了"。
    ///   现在每 3 秒检查一次，发现掉线就重启。
    ///
    /// 退避（cooldown）是必须的：
    ///   如果某个服务因为配置错误/端口永久冲突起不来，
    ///   没有退避就会每 3 秒起一次，日志爆炸、CPU 占满。
    func guardServices() {
        // 只守护「配置里启用」的服务
        checkAndRestart(key: "ssh_web", enabled: status.sshWeb.enabled, running: status.sshWeb.running) { [weak self] in
            self?.startSSHWebService()
        }
    }

    func checkAndRestart(key: String, enabled: Bool, running: Bool, action: @escaping () -> Void) {
        guard enabled, !running else { return }

        let now = Date()
        if let last = lastAutoStart[key], now.timeIntervalSince(last) < autoStartCooldown {
            return
        }
        lastAutoStart[key] = now
        dlog("guardServices: 检测到 \(key) 掉线，自动重启")
        action()
    }

    /// 首次运行时把「登录时自动启动」打开。
    ///
    /// 只在第一次做，之后尊重用户的选择 ——
    /// 如果用户主动关掉，下次启动不该又自动打开。
    func enableLaunchAtLoginIfFirstRun() {
        guard #available(macOS 13.0, *) else { return }
        let key = "AGYHub.didSetupLaunchAtLogin"
        guard !UserDefaults.standard.bool(forKey: key) else { return }

        let svc = SMAppService.mainApp
        if svc.status != .enabled {
            do {
                try svc.register()
                dlog("enableLaunchAtLogin: 已注册登录项")
            } catch {
                dlog("enableLaunchAtLogin: 失败 \(error.localizedDescription)")
            }
        }
        UserDefaults.standard.set(true, forKey: key)
        rebuildMenu()
    }

    // MARK: 状态刷新

    func refresh() {
        if let s = fetchStatus(port: port) {
            status = s
            // ⚠️ 不要在这里把 port 改成 s.web.port。
            //
            // 早期这么写过，导致：用户自定义端口后，下一次 refresh
            // 又把它改回内核记录的旧值，看起来像"改了个假端口"。
            // 端口只有一个真相 —— 内核配置文件。Swift 侧不需要维护副本，
            // 每次启动服务前都重新 pick-port / 读配置即可。
        } else {
            status = .offline(port: port)
        }
        updateIcon()
        rebuildMenu()
    }

    // MARK: 菜单

    func rebuildMenu() {
        let menu = NSMenu()

        // ---- 标题 ----
        let title = NSMenuItem(title: "AgyHub  v\(status.version)", action: nil, keyEquivalent: "")
        title.isEnabled = false
        menu.addItem(title)

        let stateText: String
        if status.web.running || status.ssh.running {
            stateText = status.upstreamOk ? "● 正在运行" : "● 运行中，但 Antigravity 未打开"
        } else {
            stateText = "○ 服务未启动"
        }
        let stateItem = NSMenuItem(title: stateText, action: nil, keyEquivalent: "")
        stateItem.isEnabled = false
        menu.addItem(stateItem)

        menu.addItem(.separator())

        // ---- 启动/停止 ----
        if status.web.running || status.ssh.running {
            let stop = NSMenuItem(title: "停止服务", action: #selector(stopAll), keyEquivalent: "")
            stop.target = self
            menu.addItem(stop)
        } else {
            let start = NSMenuItem(title: "启动服务", action: #selector(startAll), keyEquivalent: "")
            start.target = self
            menu.addItem(start)
        }

        // ---- 三个服务的开关（服务运行后才显示）----
        if status.web.running || status.ssh.running || status.sshWeb.running {
            menu.addItem(.separator())

            // 网页版（本机 Antigravity 的网页界面）
            let webItem = NSMenuItem(
                title: "启动网页版（手机连接内网可访问）",
                action: #selector(toggleWeb),
                keyEquivalent: ""
            )
            webItem.target = self
            webItem.state = status.web.enabled ? .on : .off
            menu.addItem(webItem)

            if status.web.enabled {
                addServiceDetail(to: menu, svc: status.web, kind: .web)
            }

            // SSH 网页版（转发远端开发机的界面）
            let sshWebItem = NSMenuItem(
                title: "启动 SSH 网页版（手机连接内网可访问）",
                action: #selector(toggleSSHWeb),
                keyEquivalent: ""
            )
            sshWebItem.target = self
            sshWebItem.state = status.sshWeb.enabled ? .on : .off
            menu.addItem(sshWebItem)

            if status.sshWeb.enabled {
                addServiceDetail(to: menu, svc: status.sshWeb, kind: .sshWeb)
            }

            // SSH（桌面版命令面板入口解锁）
            let sshItem = NSMenuItem(
                title: "启动 SSH",
                action: #selector(toggleSSH),
                keyEquivalent: ""
            )
            sshItem.target = self
            sshItem.state = status.ssh.enabled ? .on : .off
            menu.addItem(sshItem)

            if status.ssh.enabled {
                addServiceDetail(to: menu, svc: status.ssh, kind: .ssh)
            }

            menu.addItem(.separator())

            // 换口令
            let rotate = NSMenuItem(
                title: "更换访问口令",
                action: #selector(rotateToken),
                keyEquivalent: "r"
            )
            rotate.target = self
            menu.addItem(rotate)

            if !status.token.isEmpty {
                let tok = NSMenuItem(
                    title: "  口令：\(status.token)（点击复制）",
                    action: #selector(copyToken),
                    keyEquivalent: ""
                )
                tok.target = self
                menu.addItem(tok)
            }
        }

        menu.addItem(.separator())

        // ---- 证书 ----
        if !status.caTrusted {
            let trust = NSMenuItem(
                title: "⚠︎ 安装根证书（消除不安全警告）",
                action: #selector(installCert),
                keyEquivalent: ""
            )
            trust.target = self
            menu.addItem(trust)
        }

        // ---- 中文界面 ----
        //
        // 这一项解决的是「Antigravity 一重启，中文包就失效」。
        // 中文包的 daemon.py 是常驻的，自己会跟随重启重新注入；
        // App 只要保证它一直在跑就行。
        let zhItem = NSMenuItem(
            title: "中文界面（Antigravity）",
            action: #selector(toggleZh),
            keyEquivalent: ""
        )
        zhItem.target = self
        zhItem.state = status.zh.enabled ? .on : .off
        menu.addItem(zhItem)

        if status.zh.enabled {
            let zhState = NSMenuItem(
                title: status.zh.running ? "    已注入（重启 Antigravity 也会自动恢复）" : "    守护未运行",
                action: nil, keyEquivalent: ""
            )
            zhState.isEnabled = false
            menu.addItem(zhState)
        }

        // ---- 登录时启动 ----
        let launchAtLogin = NSMenuItem(
            title: "登录时自动启动",
            action: #selector(toggleLaunchAtLogin),
            keyEquivalent: ""
        )
        launchAtLogin.target = self
        launchAtLogin.state = launchAtLoginEnabled() ? .on : .off
        menu.addItem(launchAtLogin)

        menu.addItem(.separator())

        if updaterController != nil {
            let check = NSMenuItem(
                title: "检查更新…",
                action: #selector(SPUStandardUpdaterController.checkForUpdates(_:)),
                keyEquivalent: ""
            )
            check.target = updaterController
            menu.addItem(check)
        }

        let logs = NSMenuItem(title: "查看日志", action: #selector(openLog), keyEquivalent: "l")
        logs.target = self
        menu.addItem(logs)

        let about = NSMenuItem(title: "这个 App 怎么用？", action: #selector(showAbout), keyEquivalent: "")
        about.target = self
        menu.addItem(about)

        menu.addItem(.separator())
        let quit = NSMenuItem(title: "退出 AgyHub", action: #selector(quitApp), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)

        statusItem.menu = menu
    }

    enum ServiceKind { case web, ssh, sshWeb }

    /// 在开关下面加详情行：端口、URL、操作按钮。
    ///
    /// 设计取舍（按用户要求调整过）：
    ///   · 端口行括号里只说「点击可修改」，不再提"被占用/已自动改" ——
    ///     用户不关心技术原因，只想知道能不能点。
    ///   · 链接下面不再是"在本机打开"这种说明文字，而是两个明确的按钮：
    ///     打开 / 复制。打开用链接原样（LAN IP），不用 localhost ——
    ///     因为用户可能拿着这个链接到手机上验证，用 localhost 会打不开。
    func addServiceDetail(to menu: NSMenu, svc: ServiceStatus, kind: ServiceKind) {
        // ---- 端口行 ----
        //
        // ⚠️ 「启动 SSH」（桌面版入口解锁）**不显示端口**。
        //   它靠 CDP 注入实现，压根不监听端口，显示端口只会让人困惑
        //   （用户的原话：为什么启动 SSH 有端口？）。
        //   真正需要端口的是「SSH 网页版」，那是另一项。
        if kind != .ssh {
            let portItem = NSMenuItem(
                title: "    端口 \(svc.port)（点击可修改）",
                action: #selector(changePort(_:)),
                keyEquivalent: ""
            )
            portItem.target = self
            portItem.representedObject = portKindString(kind)
            portItem.toolTip = "点击修改端口"
            menu.addItem(portItem)
        }

        // ---- 链接 + 两个按钮 ----
        switch kind {
        case .web, .sshWeb:
            if !svc.url.isEmpty {
                // 链接本身：点击 = 在本机浏览器打开。
                //
                // 为什么不保留「打开」那个独立按钮（用户要求）：
                //   链接文字本身已经写在菜单里了，旁边再放一个"打开"
                //   属于同一个动作的两种表达，纯重复。
                //   现在点链接就是打开，另有「复制链接」负责发到手机。
                let urlItem = NSMenuItem(
                    title: "    \(svc.url)",
                    action: #selector(openServiceURL(_:)),
                    keyEquivalent: ""
                )
                urlItem.target = self
                urlItem.representedObject = svc.url
                urlItem.toolTip = "点击在浏览器中打开"
                menu.addItem(urlItem)

                let copyItem = NSMenuItem(
                    title: "    复制链接",
                    action: #selector(copyServiceURL(_:)),
                    keyEquivalent: ""
                )
                copyItem.target = self
                copyItem.representedObject = svc.url
                menu.addItem(copyItem)
            } else if !svc.running {
                let hint = NSMenuItem(title: "    未运行", action: nil, keyEquivalent: "")
                hint.isEnabled = false
                menu.addItem(hint)
            }

        case .ssh:
            // 桌面版入口：不监听端口、没有 URL。
            // 作用是在 Antigravity 里让「Connect to SSH Host…」出现。
            let hint = NSMenuItem(
                title: svc.running
                    ? "    已解锁 · 在 Antigravity 按 ⌘⇧P 搜 ssh"
                    : "    未运行 · 点击上方开关启动",
                action: nil, keyEquivalent: ""
            )
            hint.isEnabled = false
            menu.addItem(hint)

            let note = NSMenuItem(
                title: "    这一项是让电脑桌面版能用 SSH，不占端口",
                action: nil, keyEquivalent: ""
            )
            note.isEnabled = false
            menu.addItem(note)
        }
    }

    func portKindString(_ kind: ServiceKind) -> String {
        switch kind {
        case .web: return "web"
        case .ssh: return "ssh"
        case .sshWeb: return "ssh_web"
        }
    }

    /// 打开链接时用原样的 LAN IP 地址，**不换成 localhost**。
    ///
    /// 为什么：用户要验证的往往就是"手机能不能打开"。若在这里偷偷换成
    /// localhost，本机能开、手机不能开，就会误判成功。
    /// 局域网 IP 在本机也完全可访问，没必要换。
    @objc func openServiceURL(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String, let u = URL(string: raw) else { return }
        NSWorkspace.shared.open(u)
    }

    @objc func copyServiceURL(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String else { return }
        copyToPasteboard(raw)
        notify("链接已复制", "发到手机上打开即可")
    }

    // MARK: 动作 —— 服务

    @objc func startAll() {
        dlog("startAll: 进入")
        guard let core = corePath() else {
            dlog("startAll: 找不到内核 corePath=nil")
            alert("找不到内核", "没有找到 agyhub 可执行文件。请先构建 agyhub-rs。")
            return
        }

        // 先让内核算好可用端口（会写回配置），再用该端口启动。
        // 这样「启动服务」不会因为 8898 被占而静默失败。
        if let d = runCore(["pick-port", "web"]),
           let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any],
           let p = obj["port"] as? Int {
            port = p
        }

        let p = Process()
        p.executableURL = URL(fileURLWithPath: core)
        p.arguments = ["--port", String(port)]

        let logPath = logURL().path
        FileManager.default.createFile(atPath: logPath, contents: nil)
        if let fh = FileHandle(forWritingAtPath: logPath) {
            fh.seekToEndOfFile()
            p.standardOutput = fh
            p.standardError = fh
        }

        do {
            try p.run()
            coreProcess = p
            dlog("startAll: 已启动 pid=\(p.processIdentifier) port=\(port)")
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak self] in
                self?.refresh()
            }
        } catch {
            dlog("startAll: 启动抛错 \(error)")
            alert("启动失败", error.localizedDescription)
        }

        // 若 SSH 也是启用状态，一并把解锁守护拉起来
        if status.ssh.enabled {
            startSSHUnlock()
        }

        // SSH 网页版（转发开发机界面）
        if status.sshWeb.enabled {
            startSSHWebService()
        }
    }

    /// 启动 SSH 网页版（agygate.py --ssh-target）。
    ///
    /// 第一次启动时如果还没选过开发机，就先弹一个列表让用户选 ——
    /// 空着主机名直接启动只会失败，不如在这里问。
    func startSSHWebService() {
        guard status.sshWeb.pid == 0 else { return }

        var target = status.sshWeb.target ?? ""
        if target.isEmpty {
            target = promptSSHHost()
            if target.isEmpty { return }
        }

        dlog("startSSHWebService: target=\(target)")
        if let d = runCore(["start-ssh-web", "--target", target]),
           let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any] {
            if let ok = obj["ok"] as? Bool, !ok {
                let msg = (obj["error"] as? String) ?? "未知错误"
                dlog("startSSHWebService: 失败 \(msg)")
                alert("SSH 网页版启动失败", msg)
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 3.0) { [weak self] in
            self?.refresh()
        }
    }

    /// 弹出 SSH 主机选择列表。
    func promptSSHHost() -> String {
        guard let d = runCore(["ssh-hosts"]),
              let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any],
              let hosts = obj["hosts"] as? [String],
              !hosts.isEmpty
        else {
            alert("没有可用的 SSH 主机", """
            没有在 ~/.ssh/config 里找到可用的 Host。

            请先配置一台开发机，例如：
              Host dev
                  HostName 1.2.3.4
                  User you
            """)
            return ""
        }

        let a = NSAlert()
        a.messageText = "选择要转发的开发机"
        a.informativeText = """
        SSH 网页版会把这台机器的界面转发到本机，手机打开就是它的完整界面。

        ⚠️ 该机器自己必须能访问 Google，否则界面能用、一问 AI 就超时。
        """
        a.addButton(withTitle: "确定")
        a.addButton(withTitle: "取消")

        let popup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 240, height: 26))
        popup.addItems(withTitles: hosts)
        a.accessoryView = popup

        guard a.runModal() == .alertFirstButtonReturn else { return "" }
        return popup.titleOfSelectedItem ?? ""
    }

    @objc func stopAll() {
        // 只杀**我们自己记录到的** pid，不用 pkill 模式匹配。
        //
        // 这条纪律是被一次事故换来的：早期版本用 `pkill -f agygate.py`
        // 之类的宽泛匹配，把用户从终端自己跑的那份远端网页版（8897）
        // 一起杀掉了 —— 表现就是"昨天还能打开的 8897 打不开了"。
        // 任何"按命令行特征杀进程"的写法都不安全，一律不用。
        if status.web.pid > 0 {
            kill(pid_t(status.web.pid), SIGTERM)
        }
        if let p = coreProcess, p.isRunning {
            p.terminate()
        }
        coreProcess = nil

        if status.ssh.pid > 0 {
            kill(pid_t(status.ssh.pid), SIGTERM)
        }

        // SSH 网页版：只有"确认是我们启动的"才停。
        // 判断依据是配置里记录的 target 与进程实际情况一致，且端口对的，
        // 否则视作用户自己跑的，不动它。
        if status.sshWeb.pid > 0 {
            kill(pid_t(status.sshWeb.pid), SIGTERM)
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
            self?.refresh()
        }
    }

    @objc func toggleWeb() {
        let on = !status.web.enabled
        _ = runCore(["set-enabled", "web", on ? "1" : "0"])
        if !on, status.web.pid > 0 {
            kill(pid_t(status.web.pid), SIGTERM)
        } else if on {
            startAll()
        }
        refresh()
    }

    @objc func toggleSSH() {
        let on = !status.ssh.enabled
        _ = runCore(["set-enabled", "ssh", on ? "1" : "0"])
        if on {
            startSSHUnlock()
        } else if status.ssh.pid > 0 {
            kill(pid_t(status.ssh.pid), SIGTERM)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
            self?.refresh()
        }
    }

    /// SSH 网页版的开关。
    ///
    /// 它和「SSH」不是一回事：
    ///   · SSH（桌面版入口解锁）= 让 Antigravity 界面里出现 Connect to SSH Host
    ///   · SSH 网页版 = 用 ssh 隧道把开发机界面转发到本机某个端口，
    ///     手机打开就是开发机的完整界面（文件树、终端、AI 都在开发机上跑）
    @objc func toggleSSHWeb() {
        let on = !status.sshWeb.enabled
        _ = runCore(["set-enabled", "ssh_web", on ? "1" : "0"])
        if !on, status.sshWeb.pid > 0 {
            kill(pid_t(status.sshWeb.pid), SIGTERM)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
            self?.refresh()
        }
    }

    /// 启动 SSH 入口解锁。
    ///
    /// 原来这里 fork 一个 node 脚本（agy-ssh-unlock.cjs）常驻，
    /// 通过 CDP 持续往 Antigravity 注入钩子。
    ///
    /// 现在改成调内核的 `ssh-unlock` 命令 —— 注入逻辑已经用 Rust
    /// 重写并编译进二进制（src/actions.rs），不再需要 node。
    ///
    /// **为什么不再需要"常驻"**：原脚本常驻是为了给新开的窗口
    /// 也自动注入（监听 attachedToTarget）。现在改为：
    ///   · 一次性给当前所有页面装钩子
    ///   · 装上的是 addScriptToEvaluateOnNewDocument，
    ///     对之后的新导航同样生效
    ///   · guardServices 每 3 秒巡检，发现钩子掉了就重新装
    /// 这样不需要一个长期挂着的外部进程。
    func startSSHUnlock() {
        guard let d = runCore(["ssh-unlock"]),
              let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any]
        else {
            dlog("startSSHUnlock: 内核无响应")
            return
        }

        if let ok = obj["ok"] as? Bool, ok {
            dlog("startSSHUnlock: \(obj["message"] as? String ?? "已注入")")
        } else {
            let msg = (obj["error"] as? String) ?? "未知错误"
            dlog("startSSHUnlock: \(msg)")
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            self?.refresh()
        }
    }

    // MARK: 动作 —— 口令与链接

    @objc func rotateToken() {
        guard let d = runCore(["rotate-token"]),
              let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any],
              let t = obj["token"] as? String
        else {
            alert("更换口令失败", "内核没有返回新口令。")
            return
        }
        refresh()
        let a = NSAlert()
        a.messageText = "口令已更换"
        a.informativeText = """
        新口令：\(t)

        已保存，重启 App 不会变。
        手机上的旧链接会失效，请重新复制：
        \(status.web.url)
        """
        a.addButton(withTitle: "复制新链接")
        a.addButton(withTitle: "知道了")
        if a.runModal() == .alertFirstButtonReturn {
            copyToPasteboard(status.web.url)
        }
    }

    /// 修改服务端口。
    ///
    /// 用 NSAlert + 输入框而不是子菜单预设列表：用户可能想用任意端口，
    /// 预设列表总会漏。输入后由内核校验（是否被占、是否与服务冲突），
    /// 校验不过就把内核给的原因原样显示出来。
    @objc func changePort(_ sender: NSMenuItem) {
        let which = (sender.representedObject as? String) ?? "web"
        let svc = which == "web" ? status.web : status.ssh
        let name = which == "web" ? "网页版" : "SSH"

        let a = NSAlert()
        a.messageText = "修改\(name)端口"
        a.informativeText = """
        当前端口：\(svc.port)

        输入新端口（1024–65535）。如果被其他程序占用，会提示并建议可用端口。
        """
        a.addButton(withTitle: "确定")
        a.addButton(withTitle: "取消")

        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 220, height: 24))
        input.stringValue = String(svc.port)
        a.accessoryView = input

        // 让输入框自动获得焦点，用户可以直接敲数字
        a.window.initialFirstResponder = input

        guard a.runModal() == .alertFirstButtonReturn else { return }

        let v = input.stringValue.trimmingCharacters(in: .whitespaces)
        guard let port = Int(v), port > 0, port <= 65535 else {
            alert("端口无效", "请输入 1024 到 65535 之间的数字。")
            return
        }

        guard let d = runCore(["set-port", which, String(port)]),
              let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any]
        else {
            alert("设置失败", "内核没有响应。")
            return
        }

        if let ok = obj["ok"] as? Bool, !ok {
            // 内核已经把原因和建议写好了，直接显示
            let msg = (obj["error"] as? String) ?? "未知错误"
            let sug = obj["suggestion"] as? Int
            let b = NSAlert()
            b.messageText = "无法使用端口 \(port)"
            b.informativeText = msg
            b.alertStyle = .warning
            if let s = sug {
                b.addButton(withTitle: "改用 \(s)")
                b.addButton(withTitle: "取消")
                if b.runModal() == .alertFirstButtonReturn {
                    _ = runCore(["set-port", which, String(s)])
                    restIfNeeded()
                    refresh()
                }
            } else {
                b.addButton(withTitle: "知道了")
                b.runModal()
            }
            return
        }

        restIfNeeded()
        refresh()
    }

    /// 端口改动后需要重启对应服务才生效。
    ///
    /// 只对正在运行的服务做重启，没跑的不动 —— 免得用户只是改个配置
    /// 却意外启动了服务。
    func restIfNeeded() {
        if status.web.running || status.ssh.running {
            stopAll()
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak self] in
                self?.startAll()
            }
        }
    }

    @objc func copyToken() {
        guard !status.token.isEmpty else { return }
        copyToPasteboard(status.token)
        notify("口令已复制", status.token)
    }

    /// 开关中文界面。
    @objc func toggleZh() {
        let turnOn = !status.zh.enabled
        let action = turnOn ? "on" : "off"
        dlog("toggleZh: action=\(action) 当前 enabled=\(status.zh.enabled)")

        guard let d = runCore(["zh", action]),
              let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any]
        else {
            dlog("toggleZh: 内核无响应")
            alert("中文界面设置失败", "内核没有返回结果，请看日志。")
            return
        }

        if let ok = obj["ok"] as? Bool, !ok {
            let msg = (obj["error"] as? String) ?? "未知错误"
            dlog("toggleZh: \(msg)")
            alert("中文界面设置失败", msg)
            return
        }

        // 直接按内核返回的结果更新勾选状态，不等下一次轮询。
        //
        // 为什么要这样：`zh off` 内部要跑 stop-zh.cjs（连 CDP + 刷新页面），
        // 可能要 1~2 秒。之前固定等 1.5 秒再 refresh，
        // 偶尔比它快，读到的还是旧状态 —— 用户看到的就是"点了勾没取消"。
        if let enabled = obj["enabled"] as? Bool {
            status.zh.enabled = enabled
            status.zh.running = (obj["running"] as? Bool) ?? false
        }
        rebuildMenu()

        // 界面是否真的还原了：只有关闭时才有这个字段
        if action == "off", let restored = obj["ui_restored"] as? Bool, !restored {
            dlog("toggleZh: 界面未还原，提示用户")
        }

        // 稍后再刷一次，兼顾后台进程真实状态的同步
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) { [weak self] in
            self?.refresh()
        }
    }

    // MARK: 证书

    @objc func installCert() {
        guard let core = corePath() else { return }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: core)
        p.arguments = ["trust-cert"]
        try? p.run()
        p.waitUntilExit()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak self] in
            self?.refresh()
        }
    }

    // MARK: 登录时启动

    func launchAtLoginEnabled() -> Bool {
        if #available(macOS 13.0, *) {
            return SMAppService.mainApp.status == .enabled
        }
        return false
    }

    @objc func toggleLaunchAtLogin() {
        guard #available(macOS 13.0, *) else {
            alert("系统版本过低", "需要 macOS 13 或更高版本。")
            return
        }
        let svc = SMAppService.mainApp
        do {
            if svc.status == .enabled {
                try svc.unregister()
            } else {
                try svc.register()
            }
        } catch {
            alert("设置失败", """
            \(error.localizedDescription)

            可以直接在「系统设置 → 通用 → 登录项」里手动添加：
            \(Bundle.main.bundlePath)
            """)
        }
        rebuildMenu()
    }

    // MARK: 其它

    @objc func openLog() {
        NSWorkspace.shared.open(logURL())
    }

    @objc func showAbout() {
        let a = NSAlert()
        a.messageText = "AgyHub 在菜单栏里，不在 Dock"
        a.informativeText = """
        它是一个纯菜单栏 App：不占 Dock、不在启动台、不进 Cmd+Tab。
        图标在屏幕右上角菜单栏，形状是一个带贯穿弧的 A。

        · 启动服务 → 下面会出现「启动网页版」「启动 SSH」两个开关
        · 点链接那一行 = 复制到剪贴板，发给手机就能打开
        · 端口被占用时会自动换一个，无需手动改

        程序位置：\(Bundle.main.bundlePath)
        """
        a.alertStyle = .informational
        a.addButton(withTitle: "知道了")
        a.addButton(withTitle: "在访达中显示")
        if a.runModal() == .alertSecondButtonReturn {
            NSWorkspace.shared.activateFileViewerSelecting([Bundle.main.bundleURL])
        }
    }

    @objc func quitApp() {
        NSApplication.shared.terminate(nil)
    }

    func logURL() -> URL {
        let dir = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Logs")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("AgyHub.log")
    }

    func copyToPasteboard(_ s: String) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(s, forType: .string)
    }

    /// 用现代 UserNotifications 框架（NSUserNotification 从 macOS 11 起废弃）。
    func notify(_ title: String, _ body: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            guard granted else { return }
            let content = UNMutableNotificationContent()
            content.title = title
            content.body = body
            let req = UNNotificationRequest(
                identifier: UUID().uuidString,
                content: content,
                trigger: nil
            )
            center.add(req, withCompletionHandler: nil)
        }
    }

    func alert(_ title: String, _ body: String) {
        let a = NSAlert()
        a.messageText = title
        a.informativeText = body
        a.alertStyle = .warning
        a.runModal()
    }
}

// ============================================================
// 程序入口（必须放文件最后：顶层代码按顺序执行）
// ============================================================

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = AppDelegate()
app.delegate = delegate
app.run()
