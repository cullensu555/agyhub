# 上下文与用量展示：官方现状与自绘实现

> 调研：2026-11-20（Antigravity 2.33.0，网页版经网关访问）
> 实现：2026-11-20（AgyHub 1.3.0 起内置，菜单栏「显示上下文与用量（网页版）」）

**一句话**：官方网页版没有可用的用量 UI，但数据完整实时推送，所以 AgyHub 自己画了一个。

效果：网页版右下角发送按钮旁有一个百分比圆环（无数据时显示 `--`），点开是一张明细面板。
上半部分是「上下文占用」（当前 token / 上限 + 分组条形图 + 7 组明细），
下半部分是「本会话 token」按计费口径列的累计（输入 / 缓存读取 / 缓存写入 / 输出 / 合计，以及按公开价换算的金额）。

---

## 怎么用

菜单栏 AgyHub → **「显示上下文与用量（网页版）」** 勾选即开（默认开启），**刷新网页后生效**。

命令行等价：

```bash
agyhub usage on|off|status
```

- 只影响**网页版**（手机、电脑浏览器），不碰桌面版 Antigravity
- 不改网关转发逻辑、不需要调试端口，手机端一样能用
- 数据全部来自页面自己拉取的同一条流，纯读旁路

---

## 一、官方为什么指望不上

### 1. 设置 →「模型与用量」：有面板，是空壳

网页版**能打开**这个面板，但配额区块只剩一句引导文案。

关键证据：**打开面板时没有发出任何配额请求**（`retrieveUserQuotaSummary` / `loadCodeAssist` 一次都没被调用）——
不是请求失败，而是对企业账号整体禁用了数据拉取。

实际画面：设置 →「模型与用量」面板里，「方案」显示 `Your Plan: …`（由组织统一管理），
下方的「模型配额」区块只有一句指向文档的引导文案，没有任何用量数字。

### 2. 「Context and Quota Indicator」：有 flag，没组件

文档 `customflags.md` 里以 `enable-context-usage-indicator` 列在 A 类，看起来只差一个 flag。

实际是三层门槛，卡在第二层：

| 层 | 现状 | 结果 |
|---|---|---|
| flag 开关 | `contextUsageIndicator: {feature:!1, applyAll:[customFlag→!0]}` | 写 flag 能置 true ✅ |
| **组件来源** | 网页版宿主 `getAppVariant` 写死 `contextUsageIndicator: void 0` | **组件不存在** ❌ |
| 设置项可见性 | `contextUsageIndicatorEnabled: {feature:{visible:!1}}` | 设置页里不出现 ❌ |

渲染处是 `kr?.contextUsageIndicator?.Indicator`，`kr` 即宿主 buildBindings，网页版为 `undefined`，条件恒假。
Electron 侧 `app.asar` 全量扫描同样没有注入。

**实测**：基线无控件 → 写 flag + 刷新 → 仍无控件，设置页也搜不到该项。
唯一效果是往 localStorage 写了一条废数据。

### 3. 「对话步骤：默认 / 详细」：能用，但没有数字

设置 → 外观 → 对话步骤，网页版生效。
但它只是展示详细程度，**不含任何 token 数字**，替代不了用量展示。

---

## 二、数据来源（实测抓包）

拦截 `StreamAgentStateUpdates` 的 gRPC-Web 响应流，逐帧解析（`0x00` 数据帧 + 大端 u32 长度）拿到：

### `contextWindowUsage` —— 上下文窗口占用

```json
{
  "lastStepIndex": 1,
  "estimatedTokensUsed": 22331,
  "maxContextTokens": 512000,
  "tokenGroups": [
    { "name": "System Prompt", "type": "TOKEN_TYPE_SYSTEM_PROMPT", "numTokens": 4610 },
    { "name": "Subagents",     "type": "TOKEN_TYPE_SUBAGENTS",      "numTokens": 509  },
    { "name": "Skills",        "type": "TOKEN_TYPE_SKILLS",         "numTokens": 4824 },
    { "name": "Plugins",       "type": "TOKEN_TYPE_PLUGINS",        "numTokens": 1181 },
    { "name": "Rules",         "type": "TOKEN_TYPE_RULES",          "numTokens": 5172 },
    { "name": "Tools",         "type": "TOKEN_TYPE_TOOLS",          "numTokens": 5887 },
    { "name": "Chat Messages", "type": "TOKEN_TYPE_CHAT_MESSAGES",  "numTokens": 148  }
  ]
}
```

启示：真正对话内容只有 148，大头是 工具+规则+技能 的系统开销（约 1.5 万）。
**这正是官方完全没暴露的归因**，面板把它列出来了。

### `modelUsage` —— 每步实际消耗

在 `steps[].metadata.modelUsage`：

```json
{
  "model": "MODEL_PLACEHOLDER_M427",
  "inputTokens": "21580",
  "outputTokens": "137",
  "thinkingOutputTokens": "113",
  "responseOutputTokens": "24"
}
```

注意：**数值是字符串**；`outputTokens = thinkingOutputTokens + responseOutputTokens`，
思考消耗能单独拆出来（官方 UI 也没露）。

### `GetUserStatus` —— 方案配额

`userStatus.planStatus` 下：`maxNumChatInputTokens` 16384、`monthlyPromptCredits` 50000、
`availablePromptCredits` 500、`availableFlowCredits` 100。

> ⚠️ `available*` 与 `monthly*` 量级差 100 倍，单位存疑，**没查实**。
> 所以面板里暂时不显示 credits，避免换算成错误的百分比吓人。

### `executorMetadata.cascadeConfig` —— 各类预算

`plannerConfig.maxContextTokens` 200000、`checkpointConfig.maxTokenLimit` 512000 等。
⚠️ **`maxContextTokens` 有两个不同值**（planner 200000 / checkpoint 512000），含义不同别混用。
显示窗口上限必须用 `contextWindowUsage.maxContextTokens`。

---

## 三、实现要点

脚本：`agyhub-rs/assets/usage-indicator.js`（约 18 KB，网关内联注入首页）。

```
页面加载 → 脚本包裹 window.fetch
        → 只认 StreamAgentStateUpdates
        → res.clone() 后异步读流（绝不阻塞原流）
        → 增量帧解析，取 contextWindowUsage / modelUsage
        → 更新发送按钮旁的小圆环 + 明细面板
```

### 必须守住的四条（都是实测踩出来的）

1. **不能阻塞主流程**。页面同时有 6 个常驻流式订阅，卡一个就拖死 UI。所以一律 `res.clone()` 旁路读。
2. **必须跨 chunk 保留残留字节**。一帧会跨多次 `read()`（实测 62 KB 分 11 个 chunk），解析器要留 `rest`。
3. **数值是字符串**，`inputTokens: "21580"`，用前要转数字。
4. **只更新有 update 的帧**。27 帧里常常只有 1 帧带 `contextWindowUsage`。

### DOM 上踩过的坑

| 坑 | 现象 | 解法 |
|---|---|---|
| 插错层级 | 指示器跑到输入框左上角 | 插到 `flex items-center gap-1.5`（发送按钮所在按钮组）的**最前面** |
| **只认 send-button** | **agent 运行中 / 弹出 AskQuestion 时，圆环跑到题卡的「Other (write your answer)」那一行里** | 运行中按钮会换成 `input-cancel-button`，锚点必须**同时认停止按钮**；兜底找输入框时要**排除题卡内部** |
| 重绘清掉面板 | 点开面板没反应 | `root.innerHTML = ...` 会连面板一起清空；只重绘内部的 `.face` |
| 挂载位置不纠正 | 首次渲染时发送按钮还没出来，永远停在兜底位置 | 守护逻辑校验「父节点是否就是当前锚点」，不只看 `isConnected` |
| 全局样式加边框 | 圆环外出现方框 | 页面给 `svg` 加了 `box-shadow: ... 0 0 0 1px`，必须 `box-shadow:none !important` |
| 面板被裁 | 面板显示不全 | 面板挂到 `body`、用 `fixed` 定位；且要**先渲染内容再算高度**，否则高度算成 0 会溢出屏幕 |
| 分组名中英混杂 | 规则/技能被中文注入翻了，Tools 没翻 | 按 `type` 统一映射中文（`GROUP_NAMES`），不用后端 `name` |
| **数据不按会话隔离** | **退回首页还显示上个会话的 4.2%**，看着像「新对话自带 4.2%」 | `contextWindowUsage` 与 `conversationId` 同级、按会话存；比对 id 不同就 `resetUsage()`，首页（`home-greeting` 存在）直接清空 |
| 面板在 SPA 里残留 | 返回首页后面板还浮着 | 面板挂 body 不会随路由销毁；守护轮询发现已在首页就 `closePanel()`，另加 `popstate` |
| 无数据显示 0.0% | 误以为「已统计、占用为 0」 | 无 `max` 时显示 `--` |

### 关于「只认 send-button」这个坑的细节

源码里的渲染分支是：

```js
w ? <Y1c/>                                    // ← agent 运行中：红方块停止按钮
  : <button data-testid="send-button">…        // ← 空闲：发送按钮
```

`Y1c` 的 testid 是 `input-cancel-button`。所以**只要 agent 在干活，send-button 就消失了**。

旧代码在找不到 send-button 时兜底执行 `querySelector('[contenteditable="true"], textarea')`，
而 AskQuestion 卡片的「Other (write your answer)」正是一个 `textarea[data-testid="ask-question-writein"]`，
且它在 DOM 顺序上排在主输入框之前 —— 于是圆环就被插进了选项行里。

现在做法：
- `BUTTON_IDS` 数组依次尝试 `send-button` / `input-cancel-button` / `send-button-loading`
- `mainEditor()` 遍历所有输入框，**跳过任何 `closest('[data-testid^="ask-question"]')` 命中的**
- `inRightPlace()` 额外加一条硬检查：圆环落在题卡里就判定为「位置错误」，强制重挂

### 面板显示什么

```
上下文占用
361,971 / 512,000 · 71%
模型 gemini-4-carbon-high
[堆叠条]

工具      5,914   2%
规则      5,196   1%
...
────────────────
本会话 token（138 步）

输入      2,602,753   $5.21
缓存读取  31,540,361  $3.15
输出         195,664  $1.96
────────────────
合计      34,338,778  $10.32

当前单价（每百万）
输入                 $2.00
缓存读取            $0.100
输出                $10.00

[实付约 $5.16（返 50%） · 12-01 起 $20.63]
按公开价估算，非实际账单。
```

**「当前单价」区块**（2026-11-22 新增）：把实际生效的费率列出来，
用户拿「token × 单价」能自己核对上面的金额，不用去翻网页。
- 单价右边缘和上面的金额列对齐（复用 `.money` 那一列的宽度）
- 模型有缓存写入科目时额外多一行（Neon / Hydrogen）
- 触发长上下文档时（3.1 Pro 输入 >20 万）会加一句切换说明
- 单价格式化：`<1` 一律 3 位小数（`$0.100`/`$0.075`），`≥1` 两位，第三位有效则保留（`$1.875`）

### 为什么价格是本地表，不是接口

**问过，没有可用接口。** 查证过程与结论：

1. **Google 官方定价页没有公开的价格查询 API。** Gemini API 的 `models.list`
   只返回模型元信息（名字、上下文长度、支持的生成方法），**不含任何价格字段**。
2. **Cloud Billing Catalog API** 确实有 SKU 价格，但需要项目启用 Cloud Billing、
   配置 OAuth 服务账号、拉 `cloudbilling.googleapis.com` 的 SKU 列表再按 description 匹配模型 ——
   对本地菜单栏 App 来说成本远超收益，且价格变动是"人工维护 SKU"，未必比静态表及时。
3. **最强证据：Google 自己的前端也是硬编码价格表。** `main.js` 里
   `costSummary.pricing` 是后端下发的一次性快照，而它旁边写着
   `"Pricing is based on public API pricing"` —— 说明连官方都没走实时接口。
   而且这份 `costSummary` 对企业账号不推送（实测抓不到），所以也接不到。

> 结论：**维持本地 `pricing.json` 是当前最优解**。
> 真要自动化，可行的是"定期抓官方定价页解析"（外部 cron），
> 而不是"调用接口拿价"—— 但那样引入了网页解析的脆弱性，暂不做。

### ⚠️ 文案要极简（用户 2026-11-22 明确要求）

> "你不用写这么多文案了，没必要，多少就多少吧，按实际的来好了，尽量简约一些。"

**曾经写了 13 条说明文字**（解释缓存为什么大、涨价怎么算、赠金是什么机制…），
面板被文字淹没，用户明确要求砍掉。现在**只剩两行**：

| 位置 | 内容 |
|---|---|
| 促销结论 | `实付约 $5.16（返 50%） · 12-01 起 $20.63` |
| 免责 | `按公开价估算，非实际账单。` |

原则：**数字自己会说话，不要解释机制**。仅在这几种情况下留一句短提示：
- 缓存写入未生效/无数据（`缓存写入 2027-01-12 起按 $1.875/百万计费`）
- 价格表里没有该模型（`价格表无 xxx`）
- 上下文压缩过

删除的内容（不要再加回来）：
- "缓存读取是逐步累计的：每一步都要重发整段上下文……" 长解释
- "这不是叠加折扣：账单先按 $10.32 全额收取，再把一半以赠金打回"
- "2026-11-30 起单价翻倍为 $4.00 / $0.200 / $20.00 每百万且不再返还……"
- "上限随模型变化（128K / 512K / 1M），由 Antigravity 下发"
- "第一轮几乎没有缓存可命中……"

### 排版约定

- token 和金额都是**定宽右对齐列**（`.tok` 82px / `.money` 62px），
  否则 `8,448,807` 和 `129` 位数差太多会整列参差不齐。实测右边缘像素完全一致。
- **输出不拆「思考 / 回复」** —— 两者都按输出价计费，拆分对"花了多少钱"没有意义。

### ⚠️ 缓存写入量拿不到，别用读取量顶替

**踩过的坑**：一开始图省事，直接让「缓存写入」显示 `ss.cached`（缓存读取量），
结果两行永远一模一样（用户截图看到都是 57,596），而且会算出一个假的写入费。

实际上这两个是**完全不同的量**：

| 科目 | 含义 | 量级 |
|---|---|---|
| 缓存读取 | 每次请求命中缓存、按缓存价计费的 token | 逐步累加，会话下来可达数百万 |
| 缓存写入 | 把上下文写进缓存的一次性 token | 每段上下文写一次，小得多 |

实测官方**完全不下发写入量** —— `modelUsage` 里没有任何 `cacheWrite` /
`cacheCreation` / `cacheCreationTokens` 字段（全库搜过，零命中）。
所以现在：

- UI 显示 **`未提供`**，不编数字
- 金额不计入合计（`calc()` 里 `cacheWrite: 0` 恒定）
- 附一句说明：官方不下发写入量，2027-01-12 起按 $1.875/百万计费

### ⚠️ 缓存读取累计为什么会远大于上下文

用户最容易误解的一点：上下文 425K，缓存读取却 8,448,807（20 倍）。

**这是对的，不是算错。** 计费口径是**每一步都把当前全部上下文重新算一遍**：

```
累计缓存读取 ≈ Σ(各步时的上下文 − 该步未缓存输入)
```

N 步下来自然 ≈ N × 平均上下文。实测 5 步：上下文 22,805，累计缓存 102,145（4.5 倍，平均每步 2 万）。
用户截图 20 步 / 上下文 425K / 缓存 8.44M ≈ 19.8 倍，完全吻合。

它跟"当前占用"是两个不同维度的数：
- **上下文占用** = 现在这一刻窗口里有多少（一个时间点的快照）
- **累计缓存读取** = 整段会话累计被计费的缓存量（对时间求和）

面板会在这两个数差距大时自动加一句解释。

### 促销数字的含义（写文案时别搞混，但 UI 上不必解释）

> 本段是给改代码的人看的口径说明。**UI 上不需要展示这些解释**（见上面「文案要极简」）。

三个数各是什么：

| 数字 | 是什么 |
|---|---|
| **$10.32** | 按**当前单价**开票的金额。Carbon/Argon 现在 $2/$0.1/$10，这**本身就是促销期费率**，不是"折后价" |
| **$5.16** | 额外打到账单的**赠金抵扣**，基数是上面这个开票额（净支出 × 50%），所以实付约一半 |
| **$20.63** | 2026-12-01 起单价翻倍（$4/$0.2/$20）后，**同样这批 token 的开票额**，且不再返还 |

三者**不叠加**：$10.32 与 $20.63 是「同一批 token 在两个时期的开票额」，$5.16 是「现在这期额外返的赠金」。

> 曾经因此被用户质疑（"10.32 和 20.63 冲突"），当时的做法是加了一大段解释文字 ——
> 结果被要求"文案太臃肿，砍掉"。现在 UI 只用一行 `实付约 $5.16（返 50%） · 12-01 起 $20.63`
> 表达完，靠数字本身传达，不解释机制。

### 措辞注意（早期踩的坑）

`after.total` 是「这么多 token 在涨价后**要花多少钱**」，不是单价。
写成「单价涨到 $2.12」会让人误以为 $2.12 是每个 token 的价 —— 曾经就这么写过，用户当场提出歧义。

### 价格换算

价格表：`agyhub-rs/assets/pricing.json`（12 个模型，每 100 万 token 美元）。
**来源是 Google Cloud Model Garden 官方定价页**，不是 ai.google.dev（后者信息更少、缺缓存写入）。

`https://cloud.google.com/model-garden/pricing?hl=zh_cn`

**同一个文件两处共用**，不会出现菜单一个价、面板另一个价：
- 内核用 `include_str!` 注入网页版 → 面板算钱
- build.sh 拷一份到 `AgyHub.app/Contents/Resources/pricing.json` → 菜单「查看 Gemini 价格表…」

#### 现行价格（2026-11-21 核对官方页）

| 模型 | 输入 | 缓存输入 | **缓存写入** | 输出 | 促销 |
|---|---|---|---|---|---|
| Gemini 4 Neon | $3.00 | $0.150 | **$3.75**※ | $15.00 | — |
| Gemini 4 Argon | $2.00 | $0.100 | 不适用 | $10.00 | 至 11-30 返 50% |
| Gemini 4 Carbon | $2.00 | $0.100 | 不适用 | $10.00 | 至 11-30 返 50% |
| Gemini 4 Hydrogen | $1.50 | $0.075 | **$1.875**※ | $7.50 | — |
| Gemini 3.8 Flash Cyber | $1.50 | $0.150 | 不适用 | $7.50 | — |
| Gemini 3.8 / 3.7 / 3.6 Flash | $0.75 | $0.075 | 不适用 | $3.75 | 至 12-31 |
| Gemini 3.5 Flash | $1.50 | $0.150 | 不适用 | $9.00 | — |
| Gemini 3.5 Flash-Lite | $0.30 | $0.030 | 不适用 | $2.50 | — |
| Gemini 3.1 Pro | $2.00 | $0.200 | 不适用 | $12.00 | — |
| Gemini 3.1 Flash-Lite | $0.25 | $0.025 | 不适用 | $1.50 | — |

※ **缓存写入价格 2027 年 1 月 12 日生效**（官方脚注原文：「缓存写入价格将于 2027 年 1 月 12 日生效」）。
在此之前不适用该价格 —— 面板显示 `暂免` 并注明生效日，不写 `$0`（否则看不出"以后会收"）。

计价档位关系（官方全页通用）：Priority = 标准 ×1.8；Flex/Batch = 标准 ×0.5；非全球端点 = ×1.10。

#### ⚠️ Carbon / Argon 的"50%"不是打折

官方原话是「**净支出 50% 赠金返还**」，不是单价减半：

- 单价照 **$2 / $10** 收
- 周期结束后按净支出返 **50% 额度**
- Cyber Permissive 版本不享此优惠

面板里因此显示「享 50% 赠金返还（约返 $X）」，而**不能**把金额除以 2 显示 —— 那会让人
以为当下只花了一半钱。Flash 系列的 5 折才是真促销（单价直接 $0.75），所以文案用「促销价」。

#### 其他实现要点

- **模型名要前缀匹配**：`state.model` 是 `gemini-4-argon-high`，价格表 id 是 `gemini-4-argon`。
  用「前缀匹配 + 最长优先」，这样 `gemini-3.8-flash-cyber` 不会被 `gemini-3.8-flash` 抢走。
- **长上下文档**：Gemini 3.1 Pro 输入 >20 万 token 时**全部**按高档计（$4 / $0.4 / $18），
  不是只对超出部分加价。
- **匹配不到就不显示钱**，只显示 token，并提示「价格表里没有这个模型」。
- **金额精度跟着量级走**：≥$1 两位、≥$0.01 三位、更小四位 —— 否则单次几厘钱全显示成 $0.00。
- **中文双宽对齐**：菜单价格表用等宽字体，中文按显示宽度算 2 个字符，
  不能直接用 `String(format: "%-20@")`（按字符数补空格，中文列会把价格顶歪）。
- **面板挂在 body 上**，深色模式的 CSS 变量必须同时设在 root 和 panel 上。

#### ⚠️ 缓存输入必须"挂起结算"

`contextWindowUsage` 常常**晚于** steps 到达（同一帧里没有 cw）。
最初按 `frameCtx = 0` 处理，结果那一步缓存永久算 0 —— 实测出现「输入 21,579、上下文 21,689，
缓存却显示 0」的诡异结果。

现在做法：
- 同帧有 cw → 立刻结算 `cached += (frameCtx - inputTokens)`
- 同帧没 cw → 把该步 inputTokens 推进 `state.pending`
- 下一个 cw 到达时，统一给所有挂起的步结算，然后清空 `pending`

第一轮几乎没有缓存可命中（内容都是新的），所以单步时 `缓存输入` 接近 0 是正常的，
面板会加一句说明，免得被当成 bug。

### 三个数的口径（都和账单对得上）

| 科目 | 怎么来的 | 说明 |
|---|---|---|
| **输入** | Σ 各步 `inputTokens` | 实测它是「本步真正新喂进去的 token」，**命中隐式缓存的部分不算** |
| **缓存输入** | Σ（该步时的上下文总量 − 该步 `inputTokens`） | 官方不直接下发，靠上下文推算，所以标「约」 |
| **输出** | Σ `outputTokens` = 思考 + 回复 | 两个子项可单独拆出 |

`inputTokens` 到底是不是完整 prompt —— 实测数据（每步一条 `modelUsage`）：

| 步 | 消息长度 | `inputTokens` | 同时的上下文总量 |
|---|---|---|---|
| 1 | "hi" | 21,576 | 21,677 |
| 2 | "好的" | 2,224 | 22,033 |
| 3 | 4,506 字长文 | 10,916 | 32,877 |
| 4 | "嗯" | 2,124 | 26,044 |

- 第 2 步只发两个字，`inputTokens` 却还有 2,224 → **不是纯增量**
- 第 3 步发 4,506 字，跳到 10,916 → **新内容让一部分缓存失效，那部分才计入**

所以它 ≈（本轮新内容 + 失效缓存），命中隐式缓存的老内容不计入。

> ⚠️ **`steps[].metadata` 里没有 `contextWindowUsage`**（实测只有 `createdAt` /
> `modelUsage` / `executionId` / `sourceTrajectoryStepInfo` 等）。
> 所以「该步时的上下文」只能取**当帧**的 `update.contextWindowUsage`。
> 流是顺序到达的，当帧值近似等于该步值；**绝不能沿用最后一个值**，
> 否则早期小上下文会被后期大值盖掉，缓存累计严重虚高。

### 缓存 token：能推算，但不是官方口径

`modelUsage` 的完整字段里**没有任何 cache 字段**（实测 payload）：

```json
{
  "model": "MODEL_PLACEHOLDER_M463",
  "inputTokens": "22285",
  "outputTokens": "35",
  "thinkingOutputTokens": "22",
  "responseOutputTokens": "13",
  "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
  "messageId": "bot-...",
  "responseHeader": { "sessionID": "..." },
  "responseId": "knAAa-..."
}
```

官方的缓存数据走另一条路：`update.costSummary` → `turnCosts[executionId]`
→ `{ input, cacheRead, output, thinking, response, estimatedCostUsd }`，
配合 `cacheReadPricePerMillionTokens` 算钱。

**但这份 `costSummary` 对企业账号不推送** —— 实测连发多条消息，抓到的帧里一次都没出现；
界面也只有 👍/👎 没有 Turn/Tot。另外 `inputComponent.showModelUsage` 在
`userCohort: ["enterprise"]` 下被强制 `false`（`main.js` 里有这条 gating）。

所以「缓存输入」用 `上下文总量 − 未缓存输入` 推算，标「约」——
`estimatedTokensUsed` 本身是估算值，不比官方计费口径精确。

> 缓存输出：**不存在这个概念**。缓存只作用于输入（把历史 prompt 缓存起来复用），
> 输出每次都是新生成的，没有"缓存输出"这一科目。

圆环配色按占用分档：<50% 绿、50–80% 黄、>80% 红（容量语义，与涨跌配色无关）。

**没有数据时显示 `--` 而不是 `0.0%`**：前者是「还没统计过」，后者会让人误以为「统计过了，占用是 0」。

### 常见疑问

**Q：为什么有时上限是 512K、有时是 128K？不是 1M 吗？**

上限不是固定值，由后端**按当前模型**下发（`contextWindowUsage.maxContextTokens`）。
本项目里 `maxContextTokens` 是「当前这个模型这一次给的窗口」，不是账号级的统一配额。
所以换模型（比如 Argon → Hydrogen）上限就会变，面板里显示的「模型 xxx」就是用来解释这件事的。

前端也没有硬编码 1M：`1048576` 在 `main.js` 里出现的位置全是文件大小 / 位运算，与上下文无关。
模型选择器里的 `contextWindowSelector` 需要 `custom-window-selector` flag + Google cohort，网页版打不开，
所以**没有手动切窗口档位的入口**。

**Q：还没进会话就显示 4.2%，是默认就带的吗？**

不是默认值，是**上一个会话的残留**。`contextWindowUsage` 和 `conversationId` 同级，
是按会话存的状态，SPA 返回首页时 React 只换内容、不重建 body，所以旧数字会留在内存里。

1.3.0 之后的处理：
- `absorbUpdate` 里比对 `conversationId`，不一致就整份 `resetUsage()`
- 守护轮询发现 `[data-testid="home-greeting"]`（首页问候语）存在时，清空数据 + 收起面板

**Q：为什么面板里还有「对话消息」的 token，明明还没对话？**

那 100~200 的「对话消息」是你**刚发出去的那句话**本身。
首轮 `contextWindowUsage` 的 7 个分组里，`TOKEN_TYPE_CHAT_MESSAGES` 统计的是已有对话内容，
刚发一条 "hi" 就是百来个 token。真正的大头一直是 工具/规则/技能 这些系统开销（约 1.5 万）。

---

## 四、没查实 / 待观察

- `availablePromptCredits` 与 `monthlyPromptCredits` 量级不一致，单位存疑。
- compaction 触发时的 `compactedAtStepIndices` 实际取值（代码已处理，本次没触发过）。
- 手机窄屏下（UA 判定 `web.mobile`）的布局还没实测，面板会贴边。
- 首次打开页面时指示器显示 `0.0%`，要发一条消息才拿到数据 —— 老会话的历史用量不会自动回填。

