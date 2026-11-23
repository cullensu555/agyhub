# Antigravity CLI（`agy`）的用量 / 上下文 / 价格数据调研

调研日期：2026-11-23 · 本机 CLI 版本 1.5.14（`~/.local/bin/agy`，独立 Go 二进制，209 MB）

## 一句话结论

**桌面版拿不到的东西，CLI 基本都能拿到，而且是官方字段，不用推算。**

| 数据 | 桌面版（网页/网关） | CLI |
| --- | --- | --- |
| 本轮 input / output / thinking | ✅ `steps[].metadata.modelUsage` | ✅ `usage.*_tokens` |
| **缓存读取** | ⚠️ 只能 `上下文 − inputTokens` 推算 | ✅ **官方 `cache_read_tokens`** |
| **缓存写入** | ❌ 认定「官方不下发」 | ⚠️ 字段存在（`cache_creation_input_tokens` / `cachedWriteTokens`），实测恒 0 |
| 上下文窗口上限 | 从 `contextWindowUsage.maxContextTokens` 读 | ✅ `context_window.context_window_size` |
| 占用百分比 | 自己算 | ✅ `used_percentage` / `remaining_percentage` |
| 配额（剩余额度） | 拿不到 | ✅ `quota` 32 个桶 / `-p "/usage"` 结构化 |
| 金额 | 本地 `pricing.json` 估算 | ❌ `cost` 字段被 flag 挡住（`enable-cost-accumulator`），恒 null |

---

## 三条可用通道（按实现成本排序）

### 1. `-p --output-format json` —— 最简单，一次性拿会话累计

```bash
agy -p "你的问题" --output-format json
```

返回（实测）：

```json
{
  "conversation_id": "<conversation-id>",
  "status": "SUCCESS",
  "response": "...",
  "duration_seconds": 15.1,
  "num_turns": 1,
  "usage": {
    "input_tokens": 13403,
    "output_tokens": 24,
    "thinking_tokens": 18,
    "cache_read_tokens": 0,
    "total_tokens": 13427
  }
}
```

**关键：`usage` 是会话累计，不是本轮。** 实测同一会话续聊：

| 轮次 | input | output | thinking | cache_read | total |
| --- | --- | --- | --- | --- | --- |
| T1 | 13403 | 24 | 18 | 0 | 13427 |
| T2（`--conversation` 续） | 15686 | 45 | 33 | **11349** | 15731 |

T2 的 input = T1 的 13403 + 本轮新增 2283。`cache_read_tokens` 独立累加。

注意 `output_tokens` 已包含 `thinking_tokens`（24 = 18 + 6）。

- 续聊：`--conversation <id>`
- 结构化约束：`--json-schema '<schema 或文件路径>'`

### 2. `-p --output-format stream-json` —— 逐事件，能做实时面板

```bash
agy -p "问题" --output-format stream-json
```

NDJSON，每行一个事件：

```json
{"event":"init","conversation_id":"...","init":{"cwd":"...","tools":[...],"permission_mode":"request-review"}}
{"event":"step_update","step_update":{"step_index":0,"state":"DONE","step_type":"user_input"}}
{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"OK"}}
{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","usage":{...}}}
{"event":"result","result":{"status":"SUCCESS","usage":{...}}}
```

`step_type` 是**封闭词表**，官方文档写明；`step_update.usage` 只在 `state=DONE` 时出现。
`result` 的 `finished_reason` 会说明提前结束的原因（`print_timeout` / `interrupted` / `error`），正常结束时不出现。

### 3. `agy acp` —— 唯一能拿到「上下文窗口上限 + 缓存读写」的通道 ★最有用

ACP = Agent Client Protocol，stdio JSON-RPC，官方给 JetBrains / Zed 这类编辑器接的。

```bash
agy acp        # stdin/stdout 上说 JSON-RPC
```

握手：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
```

**窗口用量**——服务端主动推送 `session/update`：

```json
{"method":"session/update","params":{"sessionId":"...","update":{
  "sessionUpdate":"usage_update","used":13420,"size":1048576}}}
```

`size` 就是该模型的上下文窗口上限。实测 `gemini-4-carbon-medium` 和 `gemini-3.1-pro-high` **都是 1048576**。

**每轮 token**——`session/prompt` 的返回值：

```json
{"id":3,"result":{"stopReason":"end_turn","usage":{
  "inputTokens":13400,"outputTokens":20,
  "cachedReadTokens":0,"cachedWriteTokens":0,
  "thoughtTokens":15,"totalTokens":13420}}}
```

**这是本次调研最有价值的发现**：ACP 明面上给了 `cachedReadTokens` **和 `cachedWriteTokens`**。

三轮实测（同一会话，窗口 size 恒定 1048576）：

| 轮次 | inputTokens | cachedReadTokens | cachedWriteTokens | totalTokens | usage_update.used |
| --- | --- | --- | --- | --- | --- |
| 1 | 13409 | 0 | 0 | 13433 | 13433 |
| 2 | 2153 | **11355** | 0 | 13539 | 13539 |
| 3 | 2153 | **11459** | 0 | 13632 | 13632 |

规律：`inputTokens + cachedReadTokens = totalTokens = usage_update.used`（第 2 轮 2153 + 11355 = 13508 ≈ 13539，差 31 是 output/thinking 的口径差）。

**注意 `inputTokens` 的口径**跟桌面版一致：它是「本步实际计入输入的 token」，不含命中缓存的老内容 —— 与之前的实测结论吻合，别拿它跟上下文总量比。

`_meta` 里还带账号档位：`{"antigravity":{"tier":{"name":"Gemini Enterprise Standard","id":"gcp-ge-standard-tier"}}}`。

### 4. 自定义状态行（仅交互式 TUI）—— 字段最全，但拿不到 `cost`

`/statusline <command>` 配一个 shell 命令，CLI 会把一份 JSON 从 stdin 喂给它。实测捕获到的完整结构：

```json
{
  "cwd": "/tmp",
  "session_id": "73d7954e-...",
  "conversation_id": "73d7954e-...",
  "transcript_path": ".../transcript.jsonl",
  "model": {"id": "Gemini 4 Carbon (Medium)", "display_name": "Gemini 4 Carbon (Medium)", "effort": "medium"},
  "workspace": {"current_dir": "/tmp", "project_dir": "/tmp"},
  "version": "1.5.14",
  "context_window": {
    "total_input_tokens": 19133,
    "total_output_tokens": 37,
    "context_window_size": 1048576,
    "used_percentage": 1.8246650695800781,
    "remaining_percentage": 98.17533493041992,
    "current_usage": {
      "input_tokens": 13402,
      "output_tokens": 37,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0
    }
  },
  "exceeds_200k_tokens": false,
  "product": "antigravity",
  "quota": { "gemini-4-carbon-medium": {"remaining_fraction":1,"reset_time":"...","reset_in_seconds":0}, "...": {} },
  "agent_state": "idle",
  "sandbox": {"enabled": false},
  "plan_tier": "...",
  "email": "...",
  "terminal_width": 140
}
```

**这份 JSON 是「上下文窗口 + 配额」最完整的单一来源**：`context_window_size`、`used_percentage`、`current_usage.cache_creation_input_tokens` / `cache_read_input_tokens`、外加 32 个模型的 `remaining_fraction` 全都有。

限制：
- 只在交互式 TUI 生效，`-p` 模式**不启动**状态行（changelog 明确说了 headless 不跑 custom status line）。
- **`cost` 字段实测是 `null`**。changelog 1.1.21 说有 `cost`（未取整的会话估算成本），但当前企业账号取不到 —— 二进制里有 `enable-cost-accumulator` flag 和 `CostAggregator`、`ConversationCostSummary` 结构，属于被 flag 挡住的功能。
- 状态行每个 tick 都会调用一次脚本，**要自己限流**，别做重活。连续失败会被自动禁用（`Statusline disabled after %d consecutive failures`）。

### 5. 只读斜杠命令：`-p "/usage"` —— 拿配额，不耗配额

```bash
agy -p "/usage" --output-format json
```

返回结构化 `command.data.groups[].buckets[]`，每桶含 `remaining_fraction` + `reset_time`。
官方说明：这类只读命令**不启动 agent turn、不花配额、不留会话**（`conversation_id` 为空）。

同理可用：`/quota`、`/model`、`/effort`、`/skills`、`/credits`。

注意：
- `/context` **在 print 模式下不可用**，会直接报错让你用 `--disable-slash-commands` 当字面文本发。
- `/credits` 在当前账号直接失败（`no credits info found`）—— 企业账号没有 credits 概念。

---

## 价格：CLI 这条线也拿不到

CLI 内部确实有 `ModelPricingInfo`（`inputPricePerMillionTokens` / `outputPricePerMillionTokens` / `cacheReadPricePerMillionTokens`，都是 `fixed32`），
也有 `ConversationCostSummary` / `TurnCostSummary` / `CostAggregator`，但：

- 状态行 `cost` = null；
- `/credits` 报错；
- 会话 SQLite（`~/.gemini/antigravity-cli/conversations/*.db`）里**没有** `cost_summary` 字段，只有 protobuf 编码的 steps metadata；
- `agy models` 无价格字段，也不支持 `--output-format`。

**结论：价格表继续用本地 `agyhub-rs/assets/pricing.json`。** 这个结论和之前桌面版的调研一致，CLI 没有打开新路。

---

## 对 AgyHub 的可用性

### 立刻能用上的

1. **缓存读取不用再推算了**：只要走 CLI 通道，`cache_read_tokens` / `cachedReadTokens` 是官方值，比现在 `Σ(当帧上下文 − inputTokens)` 的近似算法准。
2. **上下文窗口上限有官方字段**：`context_window.context_window_size`，不用再从网关的 `contextWindowUsage.maxContextTokens` 读，而且直接带 `used_percentage` / `remaining_percentage`。
3. **配额（剩余额度）首次可拿**：`quota` 32 个桶 / `-p "/usage"`。桌面版完全没这个能力，面板可以加"本周还剩多少"。
4. **账号档位**：`_meta.antigravity.tier` → `Gemini Enterprise Standard`，可以解释"为什么这个账号某些字段没有"。

### 不能直接用上的

1. **价格仍是死路**，继续本地表。
2. **缓存写入量**：字段存在但实测恒 0，跟"官方不下发"的旧结论**实质一致**。不要因为看到了 `cache_creation_input_tokens` 就以为能算钱——先验证非 0 再加进 UI。
3. **ACP / 状态行都是"另一个进程"**：它们统计的是**CLI 自己的会话**，不是桌面版/网页版正在跑的那个会话。想拿桌面版的用量，仍然得走网关注入那条路。两者是互补，不是替代。

### 如果需要接入，最小可行路径

- **要窗口上限和百分比** → 走 ACP `session/new` 后的 `usage_update`（注意：会真的起一个 CLI 会话，消耗配额）。
- **要配额** → `agy -p "/usage" --output-format json`，**不花配额**，可定时跑。
- **要单次批处理用量** → `agy -p "..." --output-format json`，读 `usage`（会话累计语义）。

---

## 复现命令

```bash
# 1. 一次性 JSON（会话累计）
agy -p "只回复OK" --output-format json | python3 -m json.tool

# 2. 逐事件流
agy -p "只回复OK" --output-format stream-json

# 3. 配额（不花配额）
agy -p "/usage" --output-format json | python3 -c "import json,sys;d=json.load(sys.stdin);print(json.dumps(d['command']['data']['groups'][0]['buckets'][:3],indent=1))"

# 4. ACP 窗口用量（需要自己写 JSON-RPC 握手，参考 docs 里的 python 片段）
agy acp   # 然后按上面的 initialize / session/new / session/prompt 顺序发

# 5. 状态行捕获（仅交互式）
#    ~/.gemini/antigravity-cli/settings.json 加：
#    {"statusLine": {"command": "/tmp/cap.sh"}}
#    cap.sh 里 `cat > /tmp/last.json`
agy        # 用 PTY 启动，确认信任目录后发一条消息
```

## 附：版本与开关信息

- CLI 版本 `1.5.14`；二进制里嵌着大量 `google3/third_party/jetski/...` 路径，说明 CLI 和桌面版共用同一套 `jetski_cortex` proto（`ContextWindowUsage`、`CostSummary` 都是同名结构）。
- 被 flag 挡住的能力：`enable-cost-accumulator`（成本累加）。相关结构 `CostAggregator`、`ConversationCostSummary`、`ModelPricingInfo` 都在二进制里，但当前账号拿不到值。
- `--remote-agent`、`/swarm`、`/teamwork` 等是另一套东西，与用量统计无关。
