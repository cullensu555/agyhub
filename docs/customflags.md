# 网页版功能开关（customFlag）清单

> 调研时间：2026-11-11（基于 Antigravity 2.30.0 的 `main.js`）
>
> **这份文档是逆向调研记录，不是 AgyHub 的配置说明。**
> AgyHub 本身不依赖这些 flag —— 它只做转发与注入。
> 保留这份清单的原因是：将来要给网页版加新功能时，
> 先在这里查一下「是不是已经有现成开关」，比从头逆向省事得多。
>
> ⚠️ **flag 会随 Antigravity 版本变**。新版本可能删掉、改名或换门控方式。
> 用之前先确认当前版本里还有没有（搜 `main.js` 里的 keyString）。

---

## 先说三件影响结论的事

### 1. 页面默认就开着不少功能，你看到"能直接用"是对的

前端的开关判定是「三层 AND」：

```js
可用 = URL 参数允许 && 浏览器支持 && (远程模式 ? 远程能力表 : 默认能力表)
```

麦克风就是典型例子：

```js
y = Oi() && Ni() && (isRemoteControl || (inputComponent.enableMicrophone ?? true))
//                                              ↑ 注意 ?? true，默认就是开的
```

**`enableMicrophone` 默认值是真**，只有"遥控模式"下才会被强制关掉。
所以网页版里麦克风按钮一直存在 —— 这不是 bug，也不是你记错了。

**真正被远程模式关掉的，是 `remoteConversationCapabilities` 那张表里的 11 项**（见文末）。

### 2. 开关怎么写入

```js
localStorage['jetski.developer.customFlagOverrides']
= JSON.stringify({
    "enable-conversation-forking": true,
    "enable-pdf-viewer": true
  })
```

写完**必须刷新页面**才生效（配置在 React 初始化时读一次）。
想撤销某一项，把它设为 `false` 或从对象里删掉。

> ⚠️ 一次只开一两个，确认没问题再加下一批。
> 这些是**未完成/未发布**的功能，可能有界面错乱或报错。

### 3. 三类 flag，只有第一类你能用

| 类别 | 数量 | 说明 |
|---|---|---|
| **A 纯 flag 即可** | 70 | 只写 localStorage 就能开，无其他门槛 |
| **B 还要 host 命中** | 15 | 需要特定 host（网页版是 `web.desktop`/`web.mobile`），部分能用 |
| **C 还要 cohort** | 16 | 需要账号被分到特定实验组（google/enterprise），**基本打不开** |

判定依据是配置里的 `runtimeGating`：

```js
runtimeGating:{customFlag:"enable-pdf-viewer"}                    // A 类，只有 flag
runtimeGating:{customFlag:"enable-bottom-terminal-panel",
               host:[mi.Web.Desktop, mi.Electron]}                // B 类，还要 host
runtimeGating:{customFlag:"enable-pdf-viewer",
               userCohort:["google"]}                             // C 类，还要 cohort
```

---

## A 类：写进 localStorage 就能开（70 个里挑可感知的）

下面这些是我逐个从 `main.js` 里核对过 `feature` 映射的，
**在网页版有实际界面变化**，适合你去试。

### 明显能看到的界面变化

| flag | 功能 | 打开后你会看到 |
|---|---|---|
| `enable-user-message-bubbles` | 用户消息气泡 | 你发的消息变成气泡样式，和 AI 回复区分更明显 |
| `markdown-table-v2` | 表格渲染 V2 | 回复里的 markdown 表格样式变化 |
| `enable-pdf-viewer` | 内置 PDF 预览 | 点开 PDF 附件时在页面内预览，不再跳外部 |
| `enable-paste-text-as-snippet` | 长文本转附件 | 粘贴大段文字时自动折叠成附件（**可绕过 1MB 文本上限**） |
| `enable-new-themes` | 新主题 | 设置里出现新配色 |
| `enable-keybinding-customization` | 快捷键自定义 | 设置里可以改快捷键 |
| `enable-split-view` | 分屏 | 主区左右分栏 |
| `configure-artifact-width` | Artifact 宽度可调 | artifact 面板可以拖宽 |

### 输入框相关

| flag | 功能 | 说明 |
|---|---|---|
| `enable-input-autocomplete` | 输入自动补全 | 打字时出现补全建议 |
| `enable-composer-connectors` | 输入框 Connectors 菜单 | `+` 菜单里出现 Connectors（需先配置连接器） |
| `enable-help-slash-command` | `/help` 命令 | 斜杠菜单多一项 |
| `enable-conversation-selection-quoting` | 选中即引用 | 选中对话里的文字，可引用回复 |

### 对话相关

| flag | 功能 | 说明 |
|---|---|---|
| `enable-conversation-forking` | **对话分叉** | 从任意历史点开新分支（远程模式默认禁，此 flag 可覆盖）|
| `enable-fork-at-historical-step` | 从历史步骤 fork | 上一条的配套 |
| `enable-conversation-navigator` | 对话导航器 | 长对话的章节跳转（回复多时才有感）|
| `enable-conversation-navigator-chapter-titles` | 导航器章节标题 | 上一条的增强 |
| `enable-conversation-groups` | 会话分组 | 侧栏会话可按项目分组 |
| `enable-thought-editing` | 思考过程可编辑 | 可修改 AI 的思考内容再继续 |
| `enable-thought-steering-from-selection` | 选中内容改思考 | 上一条的配套 |
| `enable-text-transitions` | 文字过渡动画 | 纯视觉 |

### 语音（你可能最关心）

| flag | 实际作用 | 网页版能否生效 |
|---|---|---|
| `enable-voice-relay` | **语音中继**：把浏览器麦克风音频转给后端识别 | ✅ A 类，可试 |
| `enable-voice-remote` | 远程对话的 `voice` 能力位 | ✅ A 类，可试 |
| `enable-voice-remote-control` | 遥控模式的语音中继 | ✅ A 类（但遥控模式才有意义）|
| `enable-remote-control-audio-transcription` | 遥控模式的麦克风 | ✅ A 类（同上）|

> **结论**：麦克风按钮本身就是默认开的（`?? true`），
> 你看到的按钮是真实可用的。上面四个 flag 是**加强项**，
> 让"语音转文字"真正工作起来，而不只是显示一个麦克风图标。
> 建议先单独开 `enable-voice-relay` 试，这是最可能让语音真正可用的一个。

### 开发者/调试

| flag | 功能 |
|---|---|
| `enable-developer-mode` | 开发者模式（暴露更多调试项）|
| `enable-code-search-palette` | 代码搜索面板 |
| `enable-trajectory-debug-tools` | 轨迹调试工具 |
| `enable-file-viewer-keyboard-navigation` | 文件查看器键盘导航 |
| `enable-notebook-rendering` | Notebook 渲染 |

### 远程能力增强

| flag | 功能 | 注意 |
|---|---|---|
| `enable-remote-conversation-terminal` | **远程对话终端** | 覆盖 `remoteConversationCapabilities.terminal=false`。**能出现终端 UI，但浏览器里没有真实 PTY，多半只能看不能真跑** |
| `enable-resume-conversations-from-restart` | 重启后恢复会话 | 刷新页面后能接上刚才的对话 |
| `enable-split-view` | 分屏 | 见上表 |

### 其余 A 类（用处不大或纯内部）

```
cde                              cli-hosted-session
disable-mcp-app-frame-mounter    enable-auto-permission-preset
enable-aux-pane-redesign         enable-battle-contrib-variants
enable-battle-mode               enable-browser-element-picker
enable-chatski-connect-page      enable-context-usage-indicator
enable-conversation-scoped-permissions  enable-file-explorer-tab
enable-fork-in-new-abfs-workspace       enable-fork-in-new-citc-workspace
enable-fork-in-new-worktree      enable-git-vcs-ui
enable-jetbox-terminal-v2        enable-marketplace-card-status
enable-mcp-oauth-redirect        enable-mcp-plugin-auth
enable-notifications-settings    enable-pane-selection-actions
enable-per-conversation-telemetry-toggle  enable-permissioned-github
enable-plugin-auth               enable-plugin-card-auto-resume
enable-plugin-context-providers  enable-plugin-sidebar-section
enable-plugins                   enable-project-4k
enable-settings-error-card       enable-side-question-rework
enable-sidecar-pill-auto-open    enable-upload-html-artifact-to-drive
enterprise-disabled              labs-concierge-agent
project-4k                       remove-conversation-history
show-all-models-in-gemini-app
```

---

## B 类：需要 host 命中（15 个）

网页版 host 是 `web.desktop`（电脑）或 `web.mobile`（手机，
访问同一个 8898 端口时由 UA 自动判定）。

| flag | 需要的 host | 网页版能否生效 |
|---|---|---|
| `enable-bottom-terminal-panel` | `Web.Desktop` / Electron / Extension | ✅ 电脑端可试 |
| `enable-context-menu-system` | `Web` / Electron | ✅ 可试 |
| `enable-conversation-search-v2` | `Web` / Electron / Extension | ✅ 可试 |
| `enable-permissions-selector` | `Web.Desktop` / Electron / Extension | ⚠️ 还需 enterprise cohort |
| `enable-hub-builtin-browser` | `Web.Desktop` / Electron | ⚠️ 还需 google cohort |
| `enable-browser-subagent-webmcp` | `Web.Desktop` / Electron | ⚠️ 还需 google cohort |
| `enable-gil-logging` | `Web.Desktop` | ✅ 可试（日志用）|
| `enable-remote-ai` | `Web` / Electron | ⚠️ 还需 google cohort |
| `enable-resource-manager` | `Web.Desktop` / Electron | ⚠️ 还需 google cohort |
| `enable-aux-pane-redesign-extension` | Extension | ❌ 网页版无效 |
| `enable-browser-subagent-webview` | Electron | ❌ |
| `enable-browser-subagent-webview-script` | Electron / Web.Desktop | ⚠️ 还需 google |
| `enable-browser-subagent-webview-ux` | Electron | ❌ |
| `enable-workspace-scoped-jetski-extension` | Extension | ❌ |
| `remote-control-setting-enabled` | 遥控模式 host | ❌ |

---

## C 类：需要 cohort，基本打不开（16 个）

这些锁定在 `userCohort: ["google"]` 或 `["enterprise"]`，
你的账号不在对应实验组，写 flag 也没用（会被 gating 挡掉）。

```
enable-catch-file-viewer                enable-conversation-scoped-permissions
enable-conversation-sharing-prototype   enable-critique-preview
enable-developer-mode                   enable-file-viewer-keyboard-navigation
enable-git-vcs-ui                       enable-gob-cog-vcs-ui
enable-gob-repo-vcs-ui                  enable-keybinding-customization
enable-notebook-rendering               enable-paste-text-as-snippet
enable-pdf-viewer                       enable-role-onboarding
google-workspace-mentions               labs-inline-actions
```

> 注意：其中几个（如 `enable-pdf-viewer`、`enable-developer-mode`）
> **同时存在 A 类和 C 类的 gating 条目**。只要有一条是"纯 flag"，
> 就能通过那条生效 —— 上面 A 类表里我已经把它们算进 A 类了。
> 这张 C 类表只列**仅**有 cohort 门槛的那些。

---

## 真正改不掉的 11 项（`remoteConversationCapabilities`）

这些是硬编码 `false`，**只有 `terminal` 和 `voice` 两个绑了 flag**：

| 能力 | 网页版 | 能否用 flag 打开 |
|---|---|---|
| `terminal` | ❌ | ✅ `enable-remote-conversation-terminal`（UI 能出，但无真实终端）|
| `voice` | ❌ | ✅ `enable-voice-remote` |
| `localPaths` | ❌ | ❌ 硬编码 |
| `fork` | ❌ | ✅ `enable-conversation-forking`（A 类，实测可开）|
| `conversationMentions` | ❌ | ❌ |
| `sideQuestion` | ❌ | ⚠️ `enable-side-question-rework` 可试 |
| `subagentMessaging` | ❌ | ❌ |
| `revealInFileManager` | ❌ | ❌（浏览器没有访达）|
| `extensions` | ❌ | ⚠️ `enable-plugins` 可试 |
| `connectors` | ❌ | ⚠️ `enable-composer-connectors` 可试 |
| `browserSubagent` | ❌ | ⚠️ 需 google cohort |

---

## 建议的试用顺序

先只开一个，看效果，再决定要不要继续：

```js
// 第一步：语音（你最关心的）
localStorage['jetski.developer.customFlagOverrides'] =
  JSON.stringify({"enable-voice-relay": true});

// 第二步：确认语音没问题后，加对话分叉
localStorage['jetski.developer.customFlagOverrides'] =
  JSON.stringify({"enable-voice-relay": true,
                  "enable-conversation-forking": true});

// 第三步：再加视觉类（风险最低）
localStorage['jetski.developer.customFlagOverrides'] =
  JSON.stringify({"enable-voice-relay": true,
                  "enable-conversation-forking": true,
                  "enable-user-message-bubbles": true,
                  "markdown-table-v2": true,
                  "enable-paste-text-as-snippet": true});
```

每次写完**刷新页面**。想全撤销：

```js
localStorage.removeItem('jetski.developer.customFlagOverrides');
location.reload();
```

> ⚠️ 这些开关写的是**当前 origin**（含端口）的 localStorage。
> 8898 和别的端口互不影响，清浏览器数据也会一起清掉。

---

## 附：怎么自己查某个 flag

在浏览器控制台跑（需要能访问页面）：

```js
// 看当前实际生效的 flag 列表
JSON.parse(localStorage['jetski.developer.customFlagOverrides'] || '{}')
```

前端源码里的判定逻辑（`main.js`）：

```js
function Ula(a,b){            // 返回 true = 不满足条件，该条目不生效
  if(!a.runtimeGating) return false;
  var c = a.runtimeGating;
  if(c.host && !c.host.some(m => ni(m, b.host))) return true;      // host 不匹配
  if(c.customFlag && !b.customFlags.includes(c.customFlag)) return true;  // flag 没开
  // userCohort / viewportWidthCohort / os / isDevMode 同理
  return false;
}
```
