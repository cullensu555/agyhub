# agyhub-rs

用 Rust 重写的 Antigravity 网页访问网关。单二进制、无运行时依赖。

**当前实测于 Antigravity 2.32.2。**

## 为什么重写

原版 `agygate.py` 能跑，但有三个绕不过去的问题：

| 问题 | Python 版 | Rust 版 |
|---|---|---|
| 分发 | 需要 Python + 依赖装在用户机器上 | **单个 2.6 MB 二进制**，拷过去就能跑 |
| 并发模型 | 每连接一线程，6 个长连接 + 大量短请求时线程数爆炸 | tokio 异步，一个连接一个轻任务 |
| 流控正确性 | 手写分片 + 条件变量等待，踩过两次坑 | 用 `reserve_capacity` + `poll_capacity` 做真背压 |

## 架构

```
浏览器 --HTTPS/HTTP2--> agyhub --HTTP/1.1--> language_server
```

### 浏览器侧必须 HTTP/2（不可妥协）

前端一打开就同时建 6 个**永不结束**的流式订阅。HTTP/1.1 下浏览器对同一域名
只给 6 个并发连接，被订阅占满后，后续请求（发消息、取认证状态、加载 main.js）
全部排队超时。

**现象：页面能看，一操作就卡死。** 这不是性能优化，是能不能用的问题。

### 上游侧走 HTTP/1.1

每个请求自己建连、自己管，不受浏览器连接数限制。

## 跑起来

```bash
./agyhub                    # 默认端口 8898
./agyhub --port 9000        # 指定端口
./agyhub doctor             # 检查环境
./agyhub trust-cert         # 把根证书装进钥匙串
./agyhub trust-cert --force # 已装过也要重写
```

启动后会打印可访问地址，直接复制到手机。

## 关键实现细节

### 1. 复用已有的根 CA（重要）

`cert.rs` 会**优先复用 Python 版 `agygate` 留下的 `.agy-certs`**。

原因：那份根 CA 早就装进系统与登录钥匙串，手机上也装过。如果另起炉灶
生成新 CA，浏览器立刻报「不受信任」，手机上已装的根证书全部失效 ——
等于逼用户把所有设备重装一遍，只因为它们看不到的 CA 换了一张。

搜索顺序：
1. `AGEHUB_LEGACY_CERTS` 环境变量
2. 可执行文件往上找 `.agy-certs`
3. 当前工作目录往上找 `.agy-certs`

（不写死任何个人目录；找不到就生成新的 CA。生成过程有文件锁，多个进程同时启动也只会生成一张。）

### 2. 流控：`send_flow()` 是核心

h2 crate 的 `send_data` 是**同步函数**，不返回 future。文档原话：
> 不 reserve 直接 send_data 时…**这种缓冲是无界的（unbounded）**

也就是说直接 send 不会丢数据（这点比 Python 的 hyper-h2 好），
但会把整个响应堆进内存 —— 转发大文件时内存直接爆。

正确姿势：

```rust
send.reserve_capacity(n);                    // 申请容量
poll_fn(|cx| send.poll_capacity(cx)).await   // 等窗口
send.send_data(head, false)                  // 只发可用量
```

**所有对外发送都必须走 `send_flow()`，不要直接 `send_data`。**

### 3. 必须下发 cookie

`/?t=<口令>` 首次进入时，HTML 响应要带 `set-cookie: agygate=<口令>`。

漏了这一步 → 后续 `main.js` / css / api（都不带 `?t=`）全部 401 →
**页面就是白屏**。这个坑实际踩过。

### 4. 必须 `release_capacity`

接收方向同理，读完数据要 `body.flow_control().release_capacity(n)`。
不释放的话窗口停在 64KB，上传附件超过 64KB 就永久卡住
（现象：发送按钮一直转圈）。

### 5. WebSocket 隧道

RFC 8441 extended CONNECT（`:protocol: websocket`）→ 上游 HTTP/1.1 Upgrade。

两个要点：
- **握手响应 `\r\n\r\n` 之后可能已经带着上游推的首批数据**，必须原样转发。
  丢了会让界面建连后停在旧状态，用户只能手动刷新才看到最新。
- 不做 WebSocket 帧解析。握手后两端都是裸字节流，直接互转即可，
  天然兼容压缩扩展、ping/pong、分片帧。

### 6. 图标必须白底合成

App 原始 icns 转出的 PNG 四角是**完全透明**。Chrome 在 Android 上画桌面
图标时会把透明区**填成黑色** → 图标一圈黑边。

`flatten_on_white()` 把透明像素合成到白底，输出无 alpha 的 RGB PNG。
纯 `flate2` 实现，不依赖 image/sips。

**判据**：`sips -g hasAlpha` 若为 `yes` 就有风险。

**改图标后必须两件事都做，否则用户看不到变化：**
1. 服务端：图标头 `no-cache` + manifest 图标 URL 带 `?v=<mtime>`
2. 客户端：**卸载 App 后重装**。WebAPK 把图标固化在本地，
   刷新页面/清缓存都无效。

### 7. ALPN 必须 h2 在前

`cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()]`

浏览器挑 ALPN 里第一个自己能用的。把 http/1.1 放前面就等于放弃了多路复用。

## 已验证

| 项 | 结果 |
|---|---|
| HTTP/2 协商 | 协议 2 ✅ |
| 证书链严格校验（不加 -k） | 通过 ✅ |
| 12 MB main.js 完整转发 | 59ms ✅ |
| 8 并发流 | 全 200 ✅ |
| 页面渲染 | 完整 UI + 侧栏 + 79 个对话 ✅ |
| 对话历史加载 | markdown/diff/按钮全正常 ✅ |
| 图标白底 | hasAlpha: no ✅ |
| 独立运行 | 拷到 /tmp 可跑，无第三方动态库 ✅ |

## 还没做

- 菜单栏 App（当前是命令行程序）
- launchd 开机自启（`install-service.sh` 还是 Python 版的）
- 远端模式（走 Antigravity 官方 SSH，见 `Antigravity远程开发-界面入口解锁.md`）

## 踩坑记录

**磁盘空间**：首次编译失败报 `No space left on device`。
`rustls` 默认用 `aws-lc-rs` 后端，要编译一大堆 C，产物几百 MB。
改用 `ring` 后端解决：

```toml
rustls = { version = "0.23", default-features = false,
           features = ["std", "ring", "tls12", "logging"] }
```

**rcgen feature 门控**：`CertificateParams::from_ca_cert_pem` 被
`x509-parser` feature 控制，不加会报「方法不存在」。

**raw string 截断**：`r#"..."#` 里出现 `"#FAF9F5"` 会被 `"#` 提前终止。
含十六进制颜色值时要用 `r##"..."##`。
