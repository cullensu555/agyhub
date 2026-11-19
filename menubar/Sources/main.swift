// AgyHub —— Antigravity 网页访问网关的菜单栏外壳
//
// 分工：
//   这个文件（Swift）只做 macOS 原生部分：菜单栏图标、菜单、剪贴板、二维码、提示框。
//   真正的逻辑都在内核 Resources/agyhub（Rust）里：服务启停、端口、证书、注入。
//   Swift 只「显示」和「转发用户操作」，事实由 `agyhub status` 的 JSON 提供。
//
// ## 这一版改了什么（对照上一版的实机问题）
//
//   1. **所有内核调用都在后台队列**。上一版在主线程同步跑 status（每 3 秒）、
//      装证书（等用户输密码）、开中文（连 CDP），任何一个卡住整个菜单就卡死。
//   2. **服务进程由内核管理**（start-web / stop-web …）。上一版 Swift 自己 fork、
//      自己按 pid kill，状态一误判就重复拉起进程，或者杀不掉。
//   3. **守护不再弹窗**。上一版守护逻辑会每 20 秒弹一次「选择开发机」或
//      「SSH 网页版启动失败」，用户取消也没用。现在失败原因显示在菜单里，
//      并按 5s → 10s → … → 2min 退避重试。
//   4. **「停止」真的会停**。上一版「停止服务」不改配置，守护 3 秒后又拉起来了。
//      现在每个服务一个开关，关掉就是关掉。
//   5. 中文界面 / 桌面版 SSH 入口有了真正的巡检（ensure-inject），
//      Antigravity 重启或刷新后自动恢复。

import Cocoa
import CoreImage
import ServiceManagement
import Sparkle

// MARK: - 状态模型（宽松解码：缺字段用默认值，内核加减字段都不会让界面掉线）

private extension KeyedDecodingContainer {
    func v<T: Decodable>(_ key: Key, _ def: T) -> T {
        (try? decodeIfPresent(T.self, forKey: key)) ?? def
    }
}

struct ServiceStatus: Decodable {
    var enabled = false
    var running = false
    var starting = false
    var port = 0
    var pid = 0
    var url = ""
    var target = ""
    var lastError = ""
    var detail = ""
    /// 仅桌面版 SSH 入口：官方已开放，不需要注入
    var official = false

    enum K: String, CodingKey {
        case enabled, running, starting, port, pid, url, target, detail, official
        case lastError = "last_error"
    }

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        enabled = c.v(.enabled, false)
        running = c.v(.running, false)
        starting = c.v(.starting, false)
        port = c.v(.port, 0)
        pid = c.v(.pid, 0)
        url = c.v(.url, "")
        target = c.v(.target, "")
        lastError = c.v(.lastError, "")
        detail = c.v(.detail, "")
        official = c.v(.official, false)
    }
}

struct HubStatus: Decodable {
    var version = "-"
    var online = false
    var upstreamOk = false
    var lanIp = "127.0.0.1"
    var token = ""
    var caTrusted = false
    var agInstalled = true
    var agRunning = true
    /// 有开着的桌面版注入功能因为 Antigravity 没开调试端口而做不了
    var agNeedRelaunch = false
    var fwEnabled = false
    var fwBlocked = false
    var web = ServiceStatus()
    var ssh = ServiceStatus()
    var sshWeb = ServiceStatus()
    var zh = ServiceStatus()

    enum K: String, CodingKey {
        case version, token, web, ssh, zh, antigravity, firewall
        case upstreamOk = "upstream_ok"
        case lanIp = "lan_ip"
        case caTrusted = "ca_trusted"
        case sshWeb = "ssh_web"
    }

    struct Ag: Decodable {
        var installed = true
        var running = true
        var needRelaunch = false
        enum K: String, CodingKey {
            case installed, running
            case needRelaunch = "need_relaunch"
        }
        init() {}
        init(from d: Decoder) throws {
            let c = try d.container(keyedBy: K.self)
            installed = c.v(.installed, true)
            running = c.v(.running, true)
            needRelaunch = c.v(.needRelaunch, false)
        }
    }

    struct Fw: Decodable {
        var enabled = false
        var blocked = false
        enum K: String, CodingKey { case enabled, blocked }
        init() {}
        init(from d: Decoder) throws {
            let c = try d.container(keyedBy: K.self)
            enabled = c.v(.enabled, false)
            blocked = c.v(.blocked, false)
        }
    }

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        version = c.v(.version, "-")
        online = true
        upstreamOk = c.v(.upstreamOk, false)
        lanIp = c.v(.lanIp, "127.0.0.1")
        token = c.v(.token, "")
        caTrusted = c.v(.caTrusted, false)
        let ag = c.v(.antigravity, Ag())
        agInstalled = ag.installed
        agRunning = ag.running
        agNeedRelaunch = ag.needRelaunch
        let fw = c.v(.firewall, Fw())
        fwEnabled = fw.enabled
        fwBlocked = fw.blocked
        web = c.v(.web, ServiceStatus())
        ssh = c.v(.ssh, ServiceStatus())
        sshWeb = c.v(.sshWeb, ServiceStatus())
        zh = c.v(.zh, ServiceStatus())
    }
}

// MARK: - 日志

let logsDir: URL = {
    let d = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("Logs")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}()

private let logLock = NSLock()

/// 写诊断日志（GUI 进程的 stdout 会被系统吞掉，只能靠文件）。
func dlog(_ msg: String) {
    logLock.lock()
    defer { logLock.unlock() }
    let url = logsDir.appendingPathComponent("AgyHub-debug.log")
    // 超过 2MB 滚动一份，避免无限增长
    if let attr = try? FileManager.default.attributesOfItem(atPath: url.path),
       let size = attr[.size] as? NSNumber, size.intValue > 2_000_000 {
        let old = logsDir.appendingPathComponent("AgyHub-debug.log.1")
        try? FileManager.default.removeItem(at: old)
        try? FileManager.default.moveItem(at: url, to: old)
    }
    let line = "\(ISO8601DateFormatter().string(from: Date())) \(msg)\n"
    if let h = try? FileHandle(forWritingTo: url) {
        h.seekToEndOfFile()
        h.write(Data(line.utf8))
        try? h.close()
    } else {
        try? line.write(to: url, atomically: true, encoding: .utf8)
    }
}

/// 去掉日志里的访问口令（用户会把日志发给别人看）。
func redact(_ s: String) -> String {
    var out = s
    for pattern in ["([?&]t=)[0-9a-fA-F]{8,}", "(agygate=)[0-9a-fA-F]{8,}", "(\"token\":\")[0-9a-fA-F]{8,}"] {
        if let re = try? NSRegularExpression(pattern: pattern) {
            out = re.stringByReplacingMatches(
                in: out, range: NSRange(out.startIndex..., in: out), withTemplate: "$1<已隐藏>")
        }
    }
    return out
}

// MARK: - 内核调用

/// 找到 Rust 内核（只认 App 包内的，和开发时 cargo 的产物）。
func corePath() -> String? {
    var candidates: [String] = []
    if let res = Bundle.main.resourcePath {
        candidates.append(res + "/agyhub")
    }
    var dir = Bundle.main.bundleURL
    for _ in 0..<4 {
        candidates.append(dir.path + "/agyhub-rs/target/release/agyhub")
        dir = dir.deletingLastPathComponent()
    }
    return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
}

/// 跑一次内核命令，返回 stdout。**不要在主线程调用。**
///
/// 用临时文件接 stdout 而不是 Pipe：GUI 进程里 Pipe + readDataToEndOfFile
/// 与 waitUntilExit 的顺序问题曾导致读到空数据。
/// 带超时：内核任何一步卡住，都不能把 App 的后台队列永久堵死。
func runCore(_ args: [String], timeout: TimeInterval = 20) -> Data? {
    guard let core = corePath() else { return nil }
    let tmp = FileManager.default.temporaryDirectory
        .appendingPathComponent("agyhub-\(UUID().uuidString).out")
    FileManager.default.createFile(atPath: tmp.path, contents: nil)
    defer { try? FileManager.default.removeItem(at: tmp) }
    guard let fh = FileHandle(forWritingAtPath: tmp.path) else { return nil }

    let p = Process()
    p.executableURL = URL(fileURLWithPath: core)
    p.arguments = args
    p.standardOutput = fh
    p.standardError = FileHandle.nullDevice
    p.standardInput = FileHandle.nullDevice

    let done = DispatchSemaphore(value: 0)
    p.terminationHandler = { _ in done.signal() }
    do {
        try p.run()
    } catch {
        try? fh.close()
        dlog("runCore \(args.first ?? ""): 启动失败 \(error)")
        return nil
    }
    if done.wait(timeout: .now() + timeout) == .timedOut {
        dlog("runCore \(args.joined(separator: " ")): 超时 \(Int(timeout))s，已终止")
        p.terminate()
        _ = done.wait(timeout: .now() + 2)
        try? fh.close()
        return nil
    }
    try? fh.close()
    return try? Data(contentsOf: tmp)
}

func runCoreJSON(_ args: [String], timeout: TimeInterval = 20) -> [String: Any]? {
    guard let d = runCore(args, timeout: timeout) else { return nil }
    // 取最后一个非空行：防御性处理，避免前面混入别的输出
    let text = String(decoding: d, as: UTF8.self)
    let line = text.split(separator: "\n").last(where: { $0.hasPrefix("{") }).map(String.init) ?? text
    return (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any]
}

/// 跑内核命令并拿到全部输出文本（stdout + stderr），用于装证书、自检。
func runCoreText(_ args: [String], timeout: TimeInterval) -> (ok: Bool, text: String) {
    guard let core = corePath() else { return (false, "找不到内核") }
    let tmp = FileManager.default.temporaryDirectory
        .appendingPathComponent("agyhub-\(UUID().uuidString).txt")
    FileManager.default.createFile(atPath: tmp.path, contents: nil)
    defer { try? FileManager.default.removeItem(at: tmp) }
    guard let fh = FileHandle(forWritingAtPath: tmp.path) else { return (false, "") }
    let p = Process()
    p.executableURL = URL(fileURLWithPath: core)
    p.arguments = args
    p.standardOutput = fh
    p.standardError = fh
    p.standardInput = FileHandle.nullDevice
    let done = DispatchSemaphore(value: 0)
    p.terminationHandler = { _ in done.signal() }
    do { try p.run() } catch {
        try? fh.close()
        return (false, "\(error)")
    }
    let timedOut = done.wait(timeout: .now() + timeout) == .timedOut
    if timedOut {
        p.terminate()
        _ = done.wait(timeout: .now() + 2)
    }
    try? fh.close()
    let text = (try? String(contentsOf: tmp, encoding: .utf8)) ?? ""
    return (!timedOut && p.terminationStatus == 0, text)
}

// MARK: - 二维码

func qrImage(_ text: String, size: CGFloat = 240) -> NSImage? {
    guard let f = CIFilter(name: "CIQRCodeGenerator") else { return nil }
    f.setValue(Data(text.utf8), forKey: "inputMessage")
    f.setValue("M", forKey: "inputCorrectionLevel")
    guard let out = f.outputImage else { return nil }
    let scale = size / out.extent.width
    let img = out.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
    let rep = NSCIImageRep(ciImage: img)
    let ns = NSImage(size: rep.size)
    ns.addRepresentation(rep)
    return ns
}

// MARK: - 应用主体

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    var statusItem: NSStatusItem!
    var status = HubStatus()
    var updaterController: SPUStandardUpdaterController?

    /// status 轮询（串行，同一时刻只有一个在跑）
    let pollQueue = DispatchQueue(label: "agyhub.poll")
    /// 用户操作与守护（串行：启动、停止不会互相打架）
    let actionQueue = DispatchQueue(label: "agyhub.action")
    /// 桌面版注入巡检（可能要连 CDP，单独一条队列，不拖慢 status）
    let injectQueue = DispatchQueue(label: "agyhub.inject")
    /// 装证书（要等用户在系统弹窗里输密码，可能好几分钟，不能堵住其它操作）
    let certQueue = DispatchQueue(label: "agyhub.cert")

    /// 用户操作计数：每次操作开始/结束都 +1。
    /// 一次 status 轮询跨过了用户操作的话，结果可能是"新旧混合"的
    /// （配置还没写进去、进程已经停了），这种结果直接丢弃重新查，
    /// 否则守护会把用户刚关掉的服务又拉起来。
    var actionEpoch = 0
    /// 用户最近一次手动开关某个服务的时间：之后几秒内守护不碰它
    var userTouched: [String: Date] = [:]
    /// 本实例是重复启动的（马上会退出）：退出时不能去停服务
    var isDuplicate = false

    var polling = false
    var injecting = false
    var busy: [String: String] = [:]          // 服务 → 正在进行的操作（菜单里显示）
    var retryAt: [String: Date] = [:]          // 守护退避：下次允许自动重启的时间
    var retryCount: [String: Int] = [:]
    var initialStartDone = false
    var lastInjectAt = Date.distantPast
    var flashUntil = Date.distantPast
    var timer: Timer?

    let firstRunKey = "AGYHub.onboarded"

    // MARK: 启动

    func applicationDidFinishLaunching(_ notification: Notification) {
        if quitIfDuplicate() { return }

        setupStatusItem()
        logEnvironment()

        if corePath() == nil {
            dlog("⚠️ 找不到内核")
            alert("App 已损坏", "包内缺少内核文件（Contents/Resources/agyhub）。请重新下载安装。")
        }

        // 从 DMG 里直接运行 / 被系统隔离到临时目录运行：登录项、自动更新都会出问题
        warnIfNotInApplications()

        refresh { [weak self] in
            self?.initialStart()
            self?.firstRunIfNeeded()
        }

        enableLaunchAtLoginIfFirstRun()

        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            self?.refresh()
        }

        if let feed = Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") as? String, !feed.isEmpty,
           let key = Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String, !key.isEmpty {
            updaterController = SPUStandardUpdaterController(
                startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
        }
    }

    /// 退出 App 时停掉服务。
    ///
    /// 上一版退出 App 后服务还在后台跑：菜单栏图标没了，局域网里却还开着
    /// 一个能操作 AI agent 的入口，用户完全看不到。
    func applicationWillTerminate(_ notification: Notification) {
        // 重复启动的那个实例退出时，服务是另一个实例在管，不能停
        guard !isDuplicate else { return }
        dlog("退出 App，停止服务")
        _ = runCore(["stop"], timeout: 8)
    }

    /// 同时只允许一个 AgyHub 在跑（比如 /Applications 和下载目录里各开了一个）。
    func quitIfDuplicate() -> Bool {
        guard let id = Bundle.main.bundleIdentifier else { return false }
        let me = ProcessInfo.processInfo.processIdentifier
        let others = NSRunningApplication.runningApplications(withBundleIdentifier: id)
            .filter { $0.processIdentifier != me }
        if !others.isEmpty {
            isDuplicate = true
            dlog("已有另一个 AgyHub 在运行（pid \(others[0].processIdentifier)），本实例退出")
            let a = NSAlert()
            a.messageText = "AgyHub 已经在运行"
            a.informativeText = "看屏幕右上角菜单栏里的 AgyHub 图标。"
            a.runModal()
            NSApp.terminate(nil)
            return true
        }
        return false
    }

    func logEnvironment() {
        let ver = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
        dlog("=== AgyHub \(ver) 启动 ===")
        dlog("系统: \(ProcessInfo.processInfo.operatingSystemVersionString)")
        #if arch(arm64)
        dlog("架构: arm64")
        #else
        dlog("架构: x86_64")
        #endif
        dlog("App: \(Bundle.main.bundlePath)")
        dlog("内核: \(corePath() ?? "未找到")")
    }

    func warnIfNotInApplications() {
        let path = Bundle.main.bundlePath
        let fromDMG = path.hasPrefix("/Volumes/")
        let translocated = path.contains("/AppTranslocation/")
        guard fromDMG || translocated else { return }
        dlog("运行位置异常: \(path)")
        let a = NSAlert()
        a.messageText = "请先把 AgyHub 拖进「应用程序」文件夹"
        a.informativeText = fromDMG
            ? "现在是直接从安装盘里运行的。推出安装盘后 App 就会消失，「登录时自动启动」和自动更新也不会生效。\n\n把 AgyHub 拖到「应用程序」，再从那里打开。"
            : "macOS 把这个 App 放在临时隔离目录里运行，「登录时自动启动」和自动更新都不会生效。\n\n请把 AgyHub 拖到「应用程序」文件夹后再打开；如果仍然提示，在终端执行：\nxattr -dr com.apple.quarantine /Applications/AgyHub.app"
        a.addButton(withTitle: "打开「应用程序」文件夹并退出")
        a.addButton(withTitle: "仍然继续")
        if a.runModal() == .alertFirstButtonReturn {
            NSWorkspace.shared.open(URL(fileURLWithPath: "/Applications"))
            NSApp.terminate(nil)
        }
    }

    /// 打开 App 时把配置里开着的服务拉起来。
    ///
    /// 无条件调用 start-web（而不是"没在跑才调"）：内核会判断在跑的是不是
    /// 当前版本，旧版本留下的进程会被替换 —— 否则升级后跑的还是旧代码。
    func initialStart() {
        initialStartDone = true
        if status.web.enabled {
            startService("web")
        }
        if status.sshWeb.enabled && !status.sshWeb.target.isEmpty {
            startService("ssh_web")
        }
        maybeEnsureInject(force: true)
    }

    // MARK: 首次运行引导

    func firstRunIfNeeded() {
        let d = UserDefaults.standard
        guard !d.bool(forKey: firstRunKey) else { return }
        d.set(true, forKey: firstRunKey)

        // 老用户升级上来（证书早就装好了）：不打扰
        if status.caTrusted && d.bool(forKey: "AGYHub.didSetupLaunchAtLogin") {
            return
        }

        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "AgyHub 已经在运行"
        a.informativeText = """
        它在屏幕右上角的菜单栏里（不在 Dock）。
        作用：让同一个 Wi-Fi 下的手机打开这台 Mac 上的 Antigravity。

        \(status.caTrusted ? "" : "还差一步：安装本地证书，浏览器才不会提示「不安全」。macOS 会要求输入一次电脑登录密码。\n")
        """
        if status.caTrusted {
            a.addButton(withTitle: "用手机扫码打开")
            a.addButton(withTitle: "知道了")
            if a.runModal() == .alertFirstButtonReturn {
                showQR(kind: "web")
            }
            return
        }
        a.addButton(withTitle: "安装证书")
        a.addButton(withTitle: "以后再说")
        if a.runModal() == .alertFirstButtonReturn {
            installCert(thenShowQR: true)
        }
    }

    // MARK: 菜单栏图标

    func setupStatusItem() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = statusItem.button {
            button.image = menuBarImage()
            button.imageScaling = .scaleProportionallyDown
            button.imagePosition = .imageLeading
        }
        let menu = NSMenu()
        menu.delegate = self
        menu.autoenablesItems = false
        statusItem.menu = menu
    }

    /// 菜单栏图标。不设 isTemplate：模板图会被渲染成单色剪影。
    func menuBarImage() -> NSImage? {
        if let img = Bundle.main.image(forResource: "MenuIcon") {
            img.isTemplate = false
            img.size = NSSize(width: 18, height: 18)
            return img
        }
        let sym = NSImage(systemSymbolName: "antenna.radiowaves.left.and.right", accessibilityDescription: "AgyHub")
        sym?.isTemplate = true
        return sym
    }

    func updateIcon() {
        guard let button = statusItem.button else { return }
        let anyRunning = status.web.running || status.sshWeb.running
        // 服务没在跑时图标变灰：一眼就能看出"现在手机打不开"
        button.appearsDisabled = !anyRunning
        if Date() >= flashUntil {
            button.title = ""
        }
        var tip = anyRunning ? "AgyHub · 运行中" : "AgyHub · 未运行"
        if !status.agRunning { tip += "（Antigravity 没有打开）" }
        button.toolTip = tip
    }

    /// 在图标旁边闪一下文字（代替系统通知：不需要通知权限）。
    func flash(_ text: String) {
        guard let button = statusItem.button else { return }
        button.title = " " + text
        flashUntil = Date().addingTimeInterval(1.8)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.9) { [weak self] in
            guard let self, Date() >= self.flashUntil else { return }
            self.statusItem.button?.title = ""
        }
    }

    // MARK: 状态刷新与守护

    func refresh(_ then: (() -> Void)? = nil) {
        if polling {
            if let then { DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.refresh(then) } }
            return
        }
        polling = true
        let epoch = actionEpoch
        pollQueue.async { [weak self] in
            var s: HubStatus? = nil
            var raw = ""
            if let d = runCore(["status"], timeout: 15) {
                do {
                    s = try JSONDecoder().decode(HubStatus.self, from: d)
                } catch {
                    raw = String(decoding: d.prefix(200), as: UTF8.self)
                }
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.polling = false
                if epoch != self.actionEpoch {
                    // 查询期间用户做了操作：结果可能新旧混合，重新查
                    self.refresh(then)
                    return
                }
                self.apply(s, raw: raw)
                then?()
            }
        }
    }

    func apply(_ s: HubStatus?, raw: String) {
        let prev = status
        if let s {
            status = s
        } else {
            if prev.online {
                dlog("⚠️ 读不到内核状态（\(raw.isEmpty ? "无输出" : redact(raw))）")
            }
            var off = HubStatus()
            off.web.enabled = prev.web.enabled
            status = off
        }
        logTransitions(prev, status)
        updateIcon()
        guardServices()
        maybeEnsureInject(force: false)
    }

    /// 只在状态跳变时记日志（每 3 秒记一次的话，有用的信息会被冲掉）。
    func logTransitions(_ a: HubStatus, _ b: HubStatus) {
        if a.web.running != b.web.running {
            dlog("状态变化: 网页版 → \(b.web.running ? "运行" : "停止") port=\(b.web.port) \(b.web.lastError)")
        }
        if a.sshWeb.running != b.sshWeb.running {
            dlog("状态变化: SSH网页版 → \(b.sshWeb.running ? "运行" : "停止") \(b.sshWeb.lastError)")
        }
        if a.caTrusted != b.caTrusted {
            dlog("状态变化: 根证书 → \(b.caTrusted ? "已信任" : "未信任")")
        }
        if a.agRunning != b.agRunning {
            dlog("状态变化: Antigravity → \(b.agRunning ? "运行" : "未运行")")
        }
        if a.fwBlocked != b.fwBlocked && b.fwBlocked {
            dlog("⚠️ 防火墙拦截了 agyhub 的入站连接")
        }
    }

    /// 服务掉线自动拉起（带退避，失败不弹窗）。
    func guardServices() {
        guard initialStartDone, status.online else { return }
        let now = Date()
        for key in ["web", "ssh_web"] {
            let svc = key == "web" ? status.web : status.sshWeb
            if svc.running || !svc.enabled {
                retryCount[key] = 0
                continue
            }
            if svc.starting || busy[key] != nil { continue }
            if let t = userTouched[key], now.timeIntervalSince(t) < 8 { continue }
            if key == "ssh_web" && svc.target.isEmpty { continue }
            if let t = retryAt[key], now < t { continue }

            let n = retryCount[key, default: 0]
            let delays: [TimeInterval] = key == "web" ? [5, 10, 20, 40, 60] : [10, 20, 40, 80, 120]
            retryAt[key] = now.addingTimeInterval(delays[min(n, delays.count - 1)])
            retryCount[key] = n + 1
            dlog("守护: \(key) 未运行，自动启动（第 \(n + 1) 次）")
            startService(key)
        }
    }

    /// 桌面版中文 / SSH 入口巡检：Antigravity 重启或刷新后自动补注入。
    func maybeEnsureInject(force: Bool) {
        guard status.zh.enabled || status.ssh.enabled else { return }
        // 用户刚点了开关、那条命令还在跑：别同时再巡检一次（会重复刷新页面）
        guard status.agRunning, !injecting, busy["ssh"] == nil, busy["zh"] == nil, busy["relaunch"] == nil else { return }
        // 没有调试端口时巡检必然失败，原来每 3–6 秒往日志里刷一行同样的错误。
        // 等用户点「重新打开 Antigravity」，或者自己带参数重开后 need_relaunch 变回 false。
        guard force || !status.agNeedRelaunch else { return }
        let interval: TimeInterval = (status.zh.running || !status.zh.enabled) && (status.ssh.running || !status.ssh.enabled) ? 6 : 3
        guard force || Date().timeIntervalSince(lastInjectAt) >= interval else { return }
        injecting = true
        lastInjectAt = Date()
        injectQueue.async { [weak self] in
            let r = runCoreJSON(["ensure-inject"], timeout: 60)
            DispatchQueue.main.async {
                guard let self else { return }
                self.injecting = false
                if let acts = r?["actions"] as? [String], !acts.isEmpty {
                    dlog("注入巡检: \(acts.joined(separator: "; "))")
                }
                if let ok = r?["ok"] as? Bool, !ok, let e = r?["error"] as? String, !e.isEmpty {
                    dlog("注入巡检失败: \(e)")
                }
            }
        }
    }

    // MARK: 服务操作（都在 actionQueue 上跑）

    func startService(_ key: String) {
        var args: [String]
        if key == "web" {
            args = ["start-web"]
        } else {
            guard !status.sshWeb.target.isEmpty else { return }
            args = ["start-ssh-web", "--target", status.sshWeb.target]
        }
        perform(key, "正在启动…", args, timeout: 30) { [weak self] obj in
            if let ok = obj?["ok"] as? Bool, !ok {
                dlog("启动 \(key) 失败: \(obj?["error"] as? String ?? "未知")")
            }
            self?.refresh()
        }
    }

    func stopService(_ key: String, then: (() -> Void)? = nil) {
        perform(key, "正在停止…", [key == "web" ? "stop-web" : "stop-ssh-web"], timeout: 15) { [weak self] _ in
            self?.refresh(then)
        }
    }

    /// 在后台跑一个内核命令，期间菜单显示"进行中"。
    func perform(_ key: String, _ label: String, _ args: [String], timeout: TimeInterval,
                 done: @escaping ([String: Any]?) -> Void) {
        busy[key] = label
        actionEpoch += 1
        actionQueue.async { [weak self] in
            let obj = runCoreJSON(args, timeout: timeout)
            DispatchQueue.main.async {
                guard let self else { return }
                self.busy[key] = nil
                self.actionEpoch += 1
                done(obj)
            }
        }
    }

    // MARK: 菜单（打开时才构建，内容永远是最新状态）

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        buildMenu(menu)
    }

    func menuWillOpen(_ menu: NSMenu) {
        refresh()
    }

    func item(_ title: String, _ action: Selector?, key: String = "", obj: Any? = nil,
              enabled: Bool = true, indent: Int = 0) -> NSMenuItem {
        let it = NSMenuItem(title: title, action: action, keyEquivalent: key)
        it.target = self
        it.representedObject = obj
        it.isEnabled = enabled && action != nil
        it.indentationLevel = indent
        return it
    }

    func info(_ title: String, indent: Int = 1) -> NSMenuItem {
        let it = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        it.isEnabled = false
        it.indentationLevel = indent
        return it
    }

    func buildMenu(_ menu: NSMenu) {
        let appVer = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
        menu.addItem(info("AgyHub \(appVer)", indent: 0))

        if !status.online {
            menu.addItem(info("⚠︎ 内核没有响应（点「查看日志」）", indent: 0))
        } else if !status.agInstalled {
            menu.addItem(info("⚠︎ 这台 Mac 没有安装 Antigravity", indent: 0))
        } else if !status.agRunning {
            menu.addItem(info("⚠︎ Antigravity 没有打开（手机上会看到提示页）", indent: 0))
        }
        if status.fwBlocked {
            menu.addItem(item("⚠︎ 防火墙拦截了连接，手机打不开（点击查看）", #selector(showFirewallHelp)))
        }
        if status.online && !status.caTrusted {
            if busy["cert"] != nil {
                menu.addItem(info("正在安装证书…（请在系统弹窗里输入密码）", indent: 0))
            } else {
                menu.addItem(item("⚠︎ 安装根证书（消除「不安全」警告）", #selector(installCertFromMenu)))
            }
        }

        // ---- 本机网页版 ----
        menu.addItem(.separator())
        let web = item("本机网页版（手机访问这台 Mac）", #selector(toggleWeb))
        web.state = status.web.enabled ? .on : .off
        menu.addItem(web)
        if status.web.enabled {
            addServiceDetail(menu, key: "web", svc: status.web)
        }

        // ---- SSH 网页版 ----
        menu.addItem(.separator())
        let sw = item("SSH 网页版（手机访问开发机）", #selector(toggleSSHWeb))
        sw.state = status.sshWeb.enabled ? .on : .off
        menu.addItem(sw)
        if status.sshWeb.enabled {
            let t = status.sshWeb.target.isEmpty ? "未选择（点击选择）" : status.sshWeb.target
            menu.addItem(item("开发机：\(t)", #selector(chooseSSHTarget), indent: 1))
            addServiceDetail(menu, key: "ssh_web", svc: status.sshWeb)
        }

        // ---- 桌面版增强 ----
        menu.addItem(.separator())
        let ssh = item("桌面版 SSH 入口", #selector(toggleSSH))
        ssh.state = status.ssh.enabled ? .on : .off
        ssh.toolTip = "让电脑上的 Antigravity 出现「Connect to SSH Host…」。不占端口。"
        menu.addItem(ssh)
        if status.ssh.enabled {
            // detail 由内核给：官方已开放 / 已解锁 / 卡住的原因
            let d: String
            if status.ssh.running {
                d = "\(status.ssh.detail.isEmpty ? "已解锁" : status.ssh.detail) · 在 Antigravity 按 ⌘⇧P 搜 ssh"
            } else {
                d = status.ssh.detail
            }
            if !d.isEmpty { menu.addItem(info(d)) }
        }
        let zh = item("中文界面（桌面版）", #selector(toggleZh))
        zh.state = status.zh.enabled ? .on : .off
        menu.addItem(zh)
        if status.zh.enabled, !status.zh.running, !status.zh.detail.isEmpty {
            menu.addItem(info(status.zh.detail))
        }
        // Antigravity 2.33.0 起不带参数启动就没有调试端口，中文注入做不了。
        // 只给按钮，绝不自动重启：会打断正在跑的 agent。
        if let b = busy["relaunch"] {
            menu.addItem(info(b))
        } else if status.agNeedRelaunch {
            let r = item("重新打开 Antigravity（开启调试端口）", #selector(relaunchAntigravity), indent: 1)
            r.toolTip = "从程序坞打开或自动更新后，Antigravity 不开调试端口，桌面版中文用不了。点这里会退出并重新打开它。"
            menu.addItem(r)
        }

        // ---- 设置 ----
        menu.addItem(.separator())
        menu.addItem(item("更换访问口令", #selector(rotateToken)))
        let login = item("登录时自动启动", #selector(toggleLaunchAtLogin))
        login.state = launchAtLoginEnabled() ? .on : .off
        menu.addItem(login)

        menu.addItem(.separator())
        if let u = updaterController {
            let check = NSMenuItem(title: "检查更新…",
                                   action: #selector(SPUStandardUpdaterController.checkForUpdates(_:)),
                                   keyEquivalent: "")
            check.target = u
            menu.addItem(check)
        }
        menu.addItem(item("查看日志（含自检）", #selector(openLog), key: "l"))
        menu.addItem(item("使用说明", #selector(showAbout)))
        menu.addItem(.separator())
        menu.addItem(item("退出 AgyHub（同时停止服务）", #selector(quitApp), key: "q"))
    }

    func addServiceDetail(_ menu: NSMenu, key: String, svc: ServiceStatus) {
        if let b = busy[key] {
            menu.addItem(info(b))
            return
        }
        if svc.running {
            if !svc.lastError.isEmpty {
                let first = svc.lastError.split(separator: "\n").first.map(String.init) ?? svc.lastError
                menu.addItem(item("⚠︎ \(String(first.prefix(60)))（点击查看）", #selector(showError(_:)), obj: svc.lastError, indent: 1))
            }
            let url = svc.url
            let u = item(url, #selector(openServiceURL(_:)), obj: url, indent: 1)
            u.toolTip = "点击在本机浏览器打开"
            menu.addItem(u)
            menu.addItem(item("复制链接", #selector(copyServiceURL(_:)), obj: url, indent: 1))
            menu.addItem(item("显示二维码（手机扫码打开）", #selector(showQRItem(_:)), obj: key, indent: 1))
        } else if svc.starting {
            menu.addItem(info(key == "ssh_web" ? "正在连接开发机…" : "正在启动…"))
        } else if !svc.lastError.isEmpty {
            let first = svc.lastError.split(separator: "\n").first.map(String.init) ?? svc.lastError
            let e = item("⚠︎ \(String(first.prefix(60)))（点击查看）", #selector(showError(_:)), obj: svc.lastError, indent: 1)
            menu.addItem(e)
            menu.addItem(item("重试", #selector(retryService(_:)), obj: key, indent: 1))
        } else if key == "ssh_web" && svc.target.isEmpty {
            // 上面已经有"未选择"那一行
        } else {
            menu.addItem(info("未运行"))
        }
        let p = item("端口 \(svc.port)（点击修改）", #selector(changePort(_:)), obj: key, indent: 1)
        menu.addItem(p)
    }

    // MARK: 菜单动作 —— 服务开关

    @objc func toggleWeb() {
        userTouched["web"] = Date()
        let on = !status.web.enabled
        status.web.enabled = on
        perform("web", on ? "正在启动…" : "正在停止…", ["set-enabled", "web", on ? "1" : "0"], timeout: 10) { [weak self] _ in
            guard let self else { return }
            self.retryCount["web"] = 0
            self.retryAt["web"] = nil
            if on { self.startService("web") } else { self.stopService("web") }
        }
    }

    @objc func toggleSSHWeb() {
        userTouched["ssh_web"] = Date()
        let on = !status.sshWeb.enabled
        if on {
            let target = status.sshWeb.target
            if target.isEmpty {
                promptSSHHost(current: "") { [weak self] t in
                    guard let self, !t.isEmpty else { return }   // 用户取消：保持关闭
                    self.status.sshWeb.enabled = true
                    self.status.sshWeb.target = t
                    self.enableAndStartSSHWeb(t)
                }
                return
            }
            status.sshWeb.enabled = true
            enableAndStartSSHWeb(target)
        } else {
            status.sshWeb.enabled = false
            perform("ssh_web", "正在停止…", ["set-enabled", "ssh_web", "0"], timeout: 10) { [weak self] _ in
                self?.stopService("ssh_web")
            }
        }
    }

    func enableAndStartSSHWeb(_ target: String) {
        retryCount["ssh_web"] = 0
        retryAt["ssh_web"] = nil
        perform("ssh_web", "正在连接开发机…", ["set-enabled", "ssh_web", "1"], timeout: 10) { [weak self] _ in
            self?.perform("ssh_web", "正在连接开发机…", ["start-ssh-web", "--target", target], timeout: 30) { obj in
                if let ok = obj?["ok"] as? Bool, !ok {
                    dlog("SSH 网页版启动失败: \(obj?["error"] as? String ?? "")")
                }
                self?.refresh()
            }
        }
    }

    @objc func chooseSSHTarget() {
        let cur = status.sshWeb.target
        promptSSHHost(current: cur) { [weak self] t in
            guard let self, !t.isEmpty, t != cur else { return }
            self.userTouched["ssh_web"] = Date()
            self.status.sshWeb.target = t
            self.enableAndStartSSHWeb(t)
        }
    }

    @objc func retryService(_ sender: NSMenuItem) {
        guard let key = sender.representedObject as? String else { return }
        retryCount[key] = 0
        retryAt[key] = nil
        startService(key)
    }

    /// SSH 主机选择（只在用户主动操作时弹出，守护逻辑永远不会弹）。
    /// 读主机列表放在后台，避免菜单卡住。
    func promptSSHHost(current: String, completion: @escaping (String) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let obj = runCoreJSON(["ssh-hosts"], timeout: 5)
            let hosts = (obj?["hosts"] as? [String]) ?? []
            DispatchQueue.main.async {
                completion(self?.pickHost(hosts, current: current) ?? "")
            }
        }
    }

    func pickHost(_ hosts: [String], current: String) -> String {
        NSApp.activate(ignoringOtherApps: true)
        if hosts.isEmpty {
            alert("没有可用的 SSH 主机", """
            没有在 ~/.ssh/config 里找到可用的 Host。

            请先配置一台开发机并确认能免密登录，例如：
              Host dev
                  HostName 1.2.3.4
                  User you

            在终端里执行 ssh dev 能直接登录（不问密码）就可以了。
            """)
            return ""
        }
        let a = NSAlert()
        a.messageText = "选择要转发的开发机"
        a.informativeText = """
        手机打开后看到的是这台开发机上的 Antigravity 界面。

        要求：
        · 这台 Mac 能免密 ssh 到它
        · 在 Mac 桌面版里连过它一次（让它装好 Antigravity 服务）
        · 它自己能访问 Google，否则界面能用、一问 AI 就超时
        """
        a.addButton(withTitle: "确定")
        a.addButton(withTitle: "取消")
        let popup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 260, height: 26))
        popup.addItems(withTitles: hosts)
        if !current.isEmpty { popup.selectItem(withTitle: current) }
        a.accessoryView = popup
        guard a.runModal() == .alertFirstButtonReturn else { return "" }
        return popup.titleOfSelectedItem ?? ""
    }

    @objc func toggleSSH() {
        let on = !status.ssh.enabled
        // 官方已开放时（2.33.0 起）不注入、不刷新窗口，就不用弹这个提示
        if on && !status.ssh.official {
            let a = NSAlert()
            a.messageText = "打开桌面版 SSH 入口"
            a.informativeText = "Antigravity 的窗口会刷新一次（不影响正在进行的对话记录），之后在命令面板（⌘⇧P）里搜 ssh 就能看到「Connect to SSH Host…」。"
            a.addButton(withTitle: "打开")
            a.addButton(withTitle: "取消")
            NSApp.activate(ignoringOtherApps: true)
            guard a.runModal() == .alertFirstButtonReturn else { return }
        }
        status.ssh.enabled = on
        perform("ssh", "", ["ssh-unlock"] + (on ? [] : ["off"]), timeout: 45) { [weak self] obj in
            if on, let ok = obj?["ok"] as? Bool, !ok {
                dlog("SSH 入口: \(obj?["error"] as? String ?? "")")
            }
            self?.refresh()
        }
    }

    @objc func relaunchAntigravity() {
        let a = NSAlert()
        a.messageText = "重新打开 Antigravity"
        a.informativeText = """
        Antigravity 会先退出再重新打开，正在运行的 agent 会被中断，对话记录不受影响。

        为什么需要：Antigravity 2.33.0 起，只有用特定参数启动才会开调试端口，桌面版中文界面要靠它。从程序坞打开，或者 Antigravity 自动更新后重启，都不会带这个参数，需要在这里再点一次。
        """
        a.addButton(withTitle: "重新打开")
        a.addButton(withTitle: "取消")
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn else { return }
        perform("relaunch", "正在重新打开 Antigravity…", ["relaunch-antigravity"], timeout: 90) { [weak self] obj in
            guard let self else { return }
            if let ok = obj?["ok"] as? Bool, ok {
                dlog("重新打开 Antigravity: \(obj?["message"] as? String ?? "")")
                self.flash("已重新打开，几秒后中文生效")
                self.maybeEnsureInject(force: true)
            } else {
                let e = obj?["error"] as? String ?? "内核没有响应"
                dlog("重新打开 Antigravity 失败: \(e)")
                self.alert("没能重新打开 Antigravity", e)
            }
            self.refresh()
        }
    }

    @objc func toggleZh() {
        let on = !status.zh.enabled
        status.zh.enabled = on
        perform("zh", "", ["zh", on ? "on" : "off"], timeout: 45) { [weak self] obj in
            dlog("中文界面 \(on ? "开" : "关"): \(obj?["message"] as? String ?? obj?["error"] as? String ?? "")")
            if !on, let restored = obj?["ui_restored"] as? Bool, !restored {
                self?.flash("刷新 Antigravity 后恢复英文")
            }
            self?.refresh()
        }
    }

    // MARK: 菜单动作 —— 链接、二维码

    @objc func openServiceURL(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String, let u = URL(string: raw) else { return }
        NSWorkspace.shared.open(u)
    }

    @objc func copyServiceURL(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String else { return }
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(raw, forType: .string)
        flash("已复制")
    }

    @objc func showQRItem(_ sender: NSMenuItem) {
        showQR(kind: (sender.representedObject as? String) ?? "web")
    }

    func showQR(kind: String) {
        let svc = kind == "web" ? status.web : status.sshWeb
        NSApp.activate(ignoringOtherApps: true)
        guard svc.running, !svc.url.isEmpty else {
            alert("服务还没有运行", svc.lastError.isEmpty ? "稍等几秒再试。" : svc.lastError)
            return
        }
        let a = NSAlert()
        a.messageText = "用手机相机扫码打开"
        var tips = "手机要和这台 Mac 连同一个 Wi-Fi。"
        if !status.caTrusted {
            tips += "\n首次打开会提示「不安全」：选「显示详细信息 → 访问此网站」（iPhone）或「高级 → 继续前往」（安卓），页面底部有安装证书的步骤。"
        }
        if status.fwBlocked {
            tips += "\n⚠︎ 防火墙正在拦截连接，先按菜单里的提示放行。"
        }
        a.informativeText = tips
        if let img = qrImage(svc.url) {
            let v = NSImageView(frame: NSRect(x: 0, y: 0, width: 240, height: 240))
            v.image = img
            a.accessoryView = v
        }
        a.addButton(withTitle: "完成")
        a.addButton(withTitle: "复制链接")
        if a.runModal() == .alertSecondButtonReturn {
            let pb = NSPasteboard.general
            pb.clearContents()
            pb.setString(svc.url, forType: .string)
            flash("已复制")
        }
    }

    @objc func showError(_ sender: NSMenuItem) {
        guard let msg = sender.representedObject as? String else { return }
        NSApp.activate(ignoringOtherApps: true)
        alert("启动失败的原因", msg)
    }

    @objc func showFirewallHelp() {
        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "macOS 防火墙拦截了 AgyHub"
        a.informativeText = """
        电脑上一切正常，但手机的连接会被防火墙挡掉。

        放行方法：
        1. 打开「系统设置 → 网络 → 防火墙 → 选项…」
        2. 找到 agyhub，改为「允许传入连接」
           （列表里没有就点 + 添加：/Applications/AgyHub.app/Contents/Resources/agyhub）
        3. 如果开了「阻止所有传入连接」，需要关掉它

        每次 AgyHub 升级后 macOS 可能会再问一次，选「允许」即可。
        公司电脑的防火墙如果由管理员锁定，需要联系 IT。
        """
        a.addButton(withTitle: "打开系统设置")
        a.addButton(withTitle: "知道了")
        if a.runModal() == .alertFirstButtonReturn,
           let u = URL(string: "x-apple.systempreferences:com.apple.Network-Settings.extension") {
            NSWorkspace.shared.open(u)
        }
    }

    // MARK: 菜单动作 —— 口令、端口

    @objc func rotateToken() {
        NSApp.activate(ignoringOtherApps: true)
        let c = NSAlert()
        c.messageText = "更换访问口令？"
        c.informativeText = "手机上已经打开的页面、装好的应用（PWA）都要用新链接重新打开一次。"
        c.addButton(withTitle: "更换")
        c.addButton(withTitle: "取消")
        guard c.runModal() == .alertFirstButtonReturn else { return }

        perform("token", "", ["rotate-token"], timeout: 10) { [weak self] obj in
            guard let self else { return }
            guard obj?["token"] is String else {
                self.alert("更换口令失败", "内核没有返回新口令，请看日志。")
                return
            }
            // 运行中的服务 1 秒内自动改用新口令，不需要重启
            self.refresh {
                let a = NSAlert()
                a.messageText = "口令已更换，立即生效"
                a.informativeText = "旧链接已经失效。用新链接（或二维码）重新打开即可。"
                a.addButton(withTitle: "显示二维码")
                a.addButton(withTitle: "复制新链接")
                a.addButton(withTitle: "知道了")
                let r = a.runModal()
                if r == .alertFirstButtonReturn {
                    self.showQR(kind: "web")
                } else if r == .alertSecondButtonReturn {
                    let pb = NSPasteboard.general
                    pb.clearContents()
                    pb.setString(self.status.web.url, forType: .string)
                    self.flash("已复制")
                }
            }
        }
    }

    @objc func changePort(_ sender: NSMenuItem) {
        let key = (sender.representedObject as? String) ?? "web"
        let svc = key == "web" ? status.web : status.sshWeb
        let name = key == "web" ? "本机网页版" : "SSH 网页版"

        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "修改\(name)端口"
        a.informativeText = "当前端口：\(svc.port)\n输入 1024–65535 之间的数字。改端口后手机上的旧链接会失效。"
        a.addButton(withTitle: "确定")
        a.addButton(withTitle: "取消")
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 220, height: 24))
        input.stringValue = String(svc.port)
        a.accessoryView = input
        a.window.initialFirstResponder = input
        guard a.runModal() == .alertFirstButtonReturn else { return }

        let v = input.stringValue.trimmingCharacters(in: .whitespaces)
        guard let port = Int(v), port >= 1024, port <= 65535 else {
            alert("端口无效", "请输入 1024 到 65535 之间的数字。")
            return
        }
        applyPort(key, port)
    }

    func applyPort(_ key: String, _ port: Int) {
        perform(key, "正在修改端口…", ["set-port", key, String(port)], timeout: 10) { [weak self] obj in
            guard let self else { return }
            guard let obj else {
                self.alert("设置失败", "内核没有响应。")
                return
            }
            if let ok = obj["ok"] as? Bool, !ok {
                let msg = (obj["error"] as? String) ?? "未知错误"
                let b = NSAlert()
                b.messageText = "无法使用端口 \(port)"
                b.informativeText = msg
                b.alertStyle = .warning
                if let s = obj["suggestion"] as? Int {
                    b.addButton(withTitle: "改用 \(s)")
                    b.addButton(withTitle: "取消")
                    if b.runModal() == .alertFirstButtonReturn { self.applyPort(key, s) }
                } else {
                    b.addButton(withTitle: "知道了")
                    b.runModal()
                }
                return
            }
            // 正在运行的服务要重启才会换到新端口
            if (obj["restart_needed"] as? Bool) == true {
                self.stopService(key) { self.startService(key) }
            } else {
                self.refresh()
            }
        }
    }

    // MARK: 证书

    @objc func installCertFromMenu() {
        installCert(thenShowQR: false)
    }

    /// 安装根证书（后台运行：要等用户在系统弹窗里输密码，可能要很久）。
    func installCert(thenShowQR: Bool) {
        dlog("installCert: 开始")
        if busy["cert"] != nil { return }
        busy["cert"] = "正在安装证书…"
        certQueue.async { [weak self] in
            let r = runCoreText(["trust-cert"], timeout: 240)
            for line in r.text.split(separator: "\n") {
                let t = line.trimmingCharacters(in: .whitespaces)
                if !t.isEmpty { dlog("installCert| \(t)") }
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.busy["cert"] = nil
                self.refresh {
                    NSApp.activate(ignoringOtherApps: true)
                    if self.status.caTrusted {
                        if thenShowQR {
                            self.showQR(kind: "web")
                        } else {
                            self.alert("证书已安装", """
                            这台 Mac 上的浏览器已经信任 AgyHub。若仍提示不安全，完全退出浏览器再打开。

                            手机要单独装一次：手机打开链接后，按页面底部的提示操作。
                            """)
                        }
                    } else {
                        self.alert("证书没有装上", """
                        可能是在密码框点了取消，或者这台电脑不允许修改证书设置（公司电脑常见）。

                        可以稍后在菜单里点「安装根证书」再试。没有证书也能用，只是浏览器会提示「不安全」，并且不能装成手机应用。
                        """)
                    }
                }
            }
        }
    }

    // MARK: 登录时启动

    func launchAtLoginEnabled() -> Bool {
        SMAppService.mainApp.status == .enabled
    }

    /// 首次运行时打开「登录时自动启动」。只做一次，之后尊重用户的选择。
    func enableLaunchAtLoginIfFirstRun() {
        let key = "AGYHub.didSetupLaunchAtLogin"
        guard !UserDefaults.standard.bool(forKey: key) else { return }
        // 不在 /Applications 里运行时不注册：登录项会指向一个之后不存在的路径
        guard Bundle.main.bundlePath.hasPrefix("/Applications/") else { return }
        UserDefaults.standard.set(true, forKey: key)
        let svc = SMAppService.mainApp
        if svc.status != .enabled {
            do {
                try svc.register()
                dlog("已注册登录项")
            } catch {
                dlog("注册登录项失败: \(error.localizedDescription)")
            }
        }
    }

    @objc func toggleLaunchAtLogin() {
        let svc = SMAppService.mainApp
        do {
            if svc.status == .enabled {
                try svc.unregister()
            } else {
                try svc.register()
            }
        } catch {
            NSApp.activate(ignoringOtherApps: true)
            alert("设置失败", """
            \(error.localizedDescription)

            可以在「系统设置 → 通用 → 登录项」里手动添加 AgyHub。
            """)
        }
        if svc.status == .requiresApproval {
            NSApp.activate(ignoringOtherApps: true)
            alert("还需要你确认一下", "请在「系统设置 → 通用 → 登录项」里打开 AgyHub 的开关。")
            SMAppService.openSystemSettingsLoginItems()
        }
    }

    // MARK: 日志

    /// 打开日志：汇总 App、网页版、SSH 网页版三份日志，附上自检结果，口令已隐藏。
    /// 用户把这一个文件发过来就能定位问题。
    @objc func openLog() {
        flash("正在生成…")
        actionQueue.async {
            let doctor = runCoreText(["doctor"], timeout: 30).text
            var out = """
            ========================================================
             AgyHub 日志汇总（访问口令已隐藏，可以直接发给别人）
             生成时间：\(Date())
             系统：\(ProcessInfo.processInfo.operatingSystemVersionString)
             App 版本：\(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?")
             程序路径：\(Bundle.main.bundlePath)
            ========================================================

            ---------- 自检（agyhub doctor）----------
            \(doctor)

            """
            for name in ["AgyHub-debug.log", "AgyHub.log", "AgyHub-sshweb.log"] {
                out += "---------- \(name) ----------\n"
                let u = logsDir.appendingPathComponent(name)
                if let txt = try? String(contentsOf: u, encoding: .utf8), !txt.isEmpty {
                    let lines = txt.split(separator: "\n", omittingEmptySubsequences: false)
                    if lines.count > 1500 { out += "（只显示最后 1500 行）\n" }
                    out += lines.suffix(1500).joined(separator: "\n")
                    if !txt.hasSuffix("\n") { out += "\n" }
                } else {
                    out += "（文件不存在或为空）\n"
                }
                out += "\n"
            }
            let url = logsDir.appendingPathComponent("AgyHub-汇总.txt")
            try? redact(out).write(to: url, atomically: true, encoding: .utf8)
            DispatchQueue.main.async {
                NSWorkspace.shared.open(url)
            }
        }
    }

    // MARK: 其它

    @objc func showAbout() {
        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "AgyHub 使用说明"
        a.informativeText = """
        AgyHub 在屏幕右上角菜单栏（不在 Dock）。

        · 本机网页版：手机打开这台 Mac 上的 Antigravity。
          点「显示二维码」，手机相机扫一下即可（需同一个 Wi-Fi）。
        · SSH 网页版：手机打开开发机上的 Antigravity（需要免密 ssh）。
        · 桌面版 SSH 入口：让电脑版 Antigravity 出现 Connect to SSH Host。
        · 中文界面：电脑版 Antigravity 显示中文，重启后自动恢复。

        手机提示「不安全」：按页面底部的步骤装一次证书。
        打不开：点「查看日志」，把生成的文件发给作者。

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
