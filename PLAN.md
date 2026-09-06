# 完善计划（代码评审 + 分步路线）

> 基于 2025-09 全量阅读：6 crates / ~5.6k src LOC / ~9k 测试 LOC / 290 tests 全绿。
> 每个里程碑独立可提交、测试先行，与仓库既有 TDD 风格一致。

---

## 第一部分：评审发现

### A. 正确性 / 数据保真（最底层，影响所有上层）

`xml-core` 的 roundtrip 有损（已用 probe 实测复现）：

1. **实体双重转义**：`value="line1&#10;line2"` 经 parse→to_xml 后变成
   `value="line1&amp;#10;line2"` —— 多行标签在画布上会显示成字面 `&#10;` 文本。
   根因：quick-xml 属性读取不做 unescape，内存里留着 `&#10;`，写回时再转义一次。
2. **`mxfile` / `<diagram>` / `mxGraphModel` 层级属性全部丢弃**：`host`、`dx/dy`、
   `grid`、`pageWidth`、`pageHeight` 等一次 roundtrip 后消失。
3. **`mxGeometry` 信息丢失**：`relative="1"`、`exitX/exitY/entryX/entryY` 被丢弃；
   子元素 `mxPoint`（waypoints / sourcePoint / targetPoint）与 `<Array>` 整个丢失
   —— 折线路径、锚点会被静默重排。
4. 未建模的任意 XML 内容都会静默丢失（没有通用保真层）。

影响面：patch / agent-loop / subgraph 全路径都是 parse→to_xml 整图重写，
与产品核心承诺"其他元素坐标与连线维持不变 / 字节级保留"直接冲突。

### B. Agent Loop 的功能断点（决定生成质量）

1. **patch 看不到评审意见（最关键断点）**：runner 里 patch 的 instructions 只有
   `"Fix the N issue(s) flagged by the visual reviewer."`，评审出的 issues
   （kind / severity / description）完全没有传给 patch LLM ——
   `GenerateRequest.feedback` 字段存在但从无人填充。patch LLM 只能盲猜要修什么，
   视觉反思闭环在这里是断的。
2. **评审用量不计入轨迹**：`AgentDeps::review` 只返回 `ReviewResponse`，
   usage/duration 被丢掉，runner 里 `review_estimate_*` 是填 0 的 hack ——
   成本面板对 agent-loop 失真。
3. **agent-loop 不写服务器 trajectory store**：`run()` 内部自建本地 store，
   只经 progress_cb 发 WS。`GET /api/sessions/:id/trajectory` 与 usage 聚合
   完全看不到 agent-loop 的 LLM/render 调用。
4. **issue 无 cell_ids 时 patch 退化为全图重生成**：正是产品要避免的
   "牵一发而动全身"。review prompt 未强制要求回填 cell_ids；
   bbox 几何兜底（xml-core 可确定性算出 overlap 的 cell）也未做。
5. render 的 `duration_ms` 记录写死 0（`complete_event_bytes`），
   计时成本极低。

### C. 结构性问题

1. **patch 业务逻辑三处重复且已漂移**：`routes.rs::patch` 与
   `agent_deps.rs::patch` 各实现一遍 Plan-B（parse→subgraph→scope→LLM→
   apply_subgraph→serialize）；`routes.patch` 支持 `json_mode`，`agent_deps` 不支持。
   应抽取共享 service。
2. **轨迹事件身份分裂**：`record_and_emit` 里 `store.record()` 已生成
   (uuid, seq, at)，随后又 new 一个 uuid、seq 写死 0、另取时间戳构造 WS 事件。
   WS 上 seq 恒为 0，无法与 REST 轨迹关联。
3. **错误路径不统一**：generate/patch/render/review 的错误分支各自
   `record + emit` 一遍，与 `record_and_emit` 重复，且漏掉部分事件。
4. `StubLlm` + `MOCK_DIAGRAM` 在 `run.rs` 与 `tests/mock_diagram.rs` 复制；
   3.4MB viewer bundle 在 renderer/server 双份（当前 md5 一致，无防漂移机制）。
5. `max_iterations` 默认值、请求 DTO 等在 server / agent crate 双份定义
   （边界可接受，但默认值应单一来源）。

### D. 工程卫生

1. **运行时零日志**：binary 没有装 tracing subscriber，代码里所有
   `info!/warn!` 全部静默（源码注释自己都承认）。调试 agent loop 只能靠猜。
2. clippy --all-targets 有 ~20 个 warning（含 unused import）。
3. README / crate README 严重过期（仍写 "Phase 1 in progress"、
   "renderer planned"），与 60 个 commit 后的现实不符。
4. `state.rs::now_iso()` 输出伪 ISO 时间戳 `"1970-01-01T00:00:00Z+{secs}s"`；
   且与 `created_at: u64` epoch-ms 双轨。
5. `retry.rs` 写了完善的 `RetryTransport`，但 provider 从未接入 ——
   `ProviderConfig.max_retries` / `request_timeout_ms` 全是死字段，
   reqwest 请求没有 timeout（可能挂死）。

### E. 运行模型 / 产品级（后续里程碑）

1. 无 job 模型：agent-loop 是长 HTTP 请求，不可取消；前端刷新/断线后
   状态丢失，运行成了孤儿。
2. 无 per-session 串行队列：同会话并发请求可交错写版本。
3. 全内存无持久化：重启丢 session/版本/轨迹（代码注释已声明为取舍，
   但接口已为持久化预留）。
4. 前端无 token 流式显示（`generate_streaming` 实现了但无人用）。
5. 与 initial_draft 愿景的差距：无确定性几何检查器（overlap/overflow/
   crossing 全靠 VLM 目测 + LLM 盲修）、无选区裁剪图局部评审。

---

## 第二部分：分步路线

### 里程碑 0 — 基线（半天）
- 把 fidelity probe 固化为 xml-core 回归测试（当前**红**），作为 M1 的验收基准。
- 补 tracing-subscriber + EnvFilter（`RUST_LOG`），bin 接入。
- 清 clippy --all-targets 全部 warning。

### 里程碑 1 — xml-core 保真层（2~3 天，优先最高）
**改动最小的正确做法**（保持现有 typed API 不破坏调用方）：
1. 属性实体编解码：读取时对 attribute value 做 unescape，写回时自实现
   attribute 转义（`& < > "` + 控制符 `\n \r \t` → `&#10; &#13; &#9;`），
   保证 `&#10;` 等多行标签字节级往返。
2. `mxfile` / `diagram` / `mxGraphModel` 保留原始属性（attr bag）。
3. `Geometry` 增加 `relative` / `exit`/`entry` 点与保真的子元素（`mxPoint`、
   `<Array>` waypoints）—— edges 路径不被静默重排。
4. 通用兜底：任何未建模元素按"属性 + 子树原样保留"进入保真旁路，
   未知内容不丢。
5. 验收：扩展 roundtrip 测试 —— 多行标签、带 waypoint 的边、带模型属性
   的文件的 parse→serialize 与原文逐字节等价（或语义等价快照）。

### 里程碑 2 — 打通视觉闭环的数据流（1~2 天，功能增益最大）
1. issues → patch：`AgentDeps::patch` 增加 feedback（或 runner 把 issues
   序列化进 instructions）；cell_ids + 描述一起传给 patch LLM；
   review 无 cell_ids 的 issue 记录为警告事件。
2. review 用量真实化：`AgentDeps::review` 返回 usage/duration
   （改为返回 `LlmResponse<ReviewResponse>` 形状），runner 记录真实值，
   删掉 `review_estimate_*` hack；render 用 `Instant` 计时。
3. agent-loop 事件写入服务器 trajectory store：emitter 任务内
   `record_and_emit`，`GET /trajectory` 与 usage 聚合覆盖 agent-loop。
4. 统一轨迹记录：`TrajectoryStore::record` 返回完整 `Event`，
   WS 直接复用它（消灭 uuid/seq 分裂）；错误路径全走 `record_and_emit`。
5. 抽取共享 PatchService：手工 `/patch` 端点与 agent loop 的 patch
   走同一实现（保留 json_mode 行为），附集成测试防回归。

### 里程碑 3 — 健壮性（1~2 天）
1. provider 接入 `RetryTransport` + reqwest timeout（`request_timeout_ms`
   生效），失败原因进轨迹。
2. patch/agent-loop 产出先过 `xml-core::validate` + no-op 检测
   （与上一版本逐 id 对比），无效/无变化则不写版本、返回明确标记。
3. loop 内 review 失败不直接 abort：重试一次，仍失败则带最后渲染图
   "best effort" 结束并标记 degraded。

### 里程碑 4 — 局部化兜底（1 天）
- review prompt 强制要求 `cell_ids`（对照 XML 回填）；
- issue 缺 cell_ids 时，用 xml-core 确定性 bbox 几何推断（overlap 检测
  可精确算重叠的 cell），不再退化为全图重生成；
- "Refine" 路径的 patch 默认禁止 full-context 重生成。

### 里程碑 5 — 运行模型（3~5 天）
- per-session 串行 job 队列 + run_id；WS 推送 job 状态；POST 立即返回，
  结果经 WS/轮询获取；支持 cancel。
- SQLite/JSONL 持久化 session、版本、轨迹（接口已预留，替换 store 实现）。
- 前端断线重连后恢复 job 状态视图。

### 里程碑 6 — 愿景深化（可选，长期）
- 确定性几何检查器：overlap / text-overflow / edge-crossing 的 bbox 算法
  进 xml-core，review 变为"确定性检查 + VLM 审美补充"双轨，
  几何修复走小步确定性 diff（痛点 1、2 的直接解法，省 token）。
- 选区/issue 区域裁剪图（局部 PNG）喂 VLM，注意力集中、省 token。
- 前端 token 流式显示（`generate_streaming` 落地）。

---

## 建议执行顺序与理由

M1（保真）→ M2（数据流）→ M3（健壮性）是**性价比最高且相互独立**的三步：
M1 是一切质量的地基（不做它，M2 闭环越通，损坏扩散越快）；
M2 每一小项都是可独立验证的修复，其中 2.1（feedback 闭环）单点就能
显著改善生成质量；M3 之后才有底气接真实 LLM 长时间跑。
M5/M6 是产品化与愿景工程，建议在核心闭环数据可信之后再投入。

---

## 第三部分（2025-09 定稿）：Agent 核心 v2 —— 会话驱动的单上下文 Loop

### 决策

已确认方向：**单上下文 + 结构化编辑**（替代现在的 Generate/Review/Patch 三独立调用）。

- 每轮 1 次多模态 LLM 调用（不再是 review + patch 两次）；
- 模型产出仍是**结构化 mxfile**，服务端 merge —— 保留字节级局部性承诺；
- 视觉评审**内化**进修改调用（模型看图后直接修）；独立 VLM review 降级为
  手动 "再审" 端点 / 回归测试用。
- 默认单 vision 模型，provider 层保留可切换能力（不锁死双模型）。

### 每轮的消息结构（固定槽位，控制 token 增长）

```
system    绘图文法 + 输出协议（JSON envelope，json_mode）
memory    [用户意图(永久)] + 最近 K 轮 {assistant note, 应用结果摘要}
          —— 历史不携带整段 XML；早期轮次超窗后滚压成 design summary
state     当前已应用 XML 全文（每轮重新注入最新版，不进历史累积）
visual    最新渲染 PNG（缩放） + 可选上轮改动区域裁剪图
user      本轮指令：用户原话 / 确定性检查发现的问题列表 / "没问题则 done=1"
──────► 模型输出 JSON ──► validate + merge ──► 存版本 ──► 渲染新图 ──► 下一轮
```

### 模型输出协议（与现有 json_mode 兼容的 envelope 扩展）

```json
{ "done": false, "reasoning": "…", "xml": "<mxfile>…</mxfile>" }
```

- `xml` 恒为完整 mxfile：全图模式 = 模型重画全图；编辑模式 = 服务端注入
  scope 子图（仅目标 cell），模型只改这些 cell 并回传，服务端按 id merge
  （模型心智只有一种输出协议，协议分叉在服务端）。
- `done=true` + 确定性检查通过 → 收敛；模型自述不被单独采信。

### 收敛把关（替代独立 review 的 verdict）

1. parse / id 一致性 / no-op 检测（每轮必做，服务端）
2. xml-core 确定性 bbox 检查：overlap、label 溢出等可精确计算的项
   （逐步补；先做 overlap）→ 结果以 tool message 形式自动附加进下一轮
3. 视觉美学类（箭头错位、布局失衡）：模型自审 + 可选每 N 轮独立 VLM 抽查
4. max_rounds 兜底

### 迁移步骤（每步独立可测、可回退）

- **R1 单调用闭环（最小重构）**：不动会话记忆，先把 runner 的
  Render→Review→Patch 两调用合并为一次多模态 "ReviewAndPatch" 调用：
  入参 = scope 子图 + 最新渲染图 + issues 描述 + 用户指令（修掉 feedback
  断点，调用减半）。json_mode envelope 加 `done`。确定性检查先行上线
  （parse + no-op + id 一致），模型 done 且检查过 → 收敛。
- **R2 会话记忆**：server 为每 session 维护 conversation（用户消息、每轮
  assistant note、应用摘要、最新渲染图缓存）；turn 注入滑窗 + 当前状态槽。
  /generate、/patch、/agent-loop 统一走 chat 管线（HTTP 壳保留兼容）。
- **R3 图像预算**：渲染图缩放/降采样上限；diff 区域裁剪图（renderer 按 bbox
  截图）注入；超窗轮次滚压 design summary。
- **R4 确定性检查器 + 手动复核**：xml-core bbox overlap 检查器正式化；
  review 端点改为可选 "请再审"（无则跳过），前端可触发。

### v2 对现有代码的改动面

| 模块 | 改动 |
|---|---|
| llm-client | 新增多模态 chat（text+image parts + json_object，现两者已各自存在）；`GenerateRequest` 扩展图像/问题列表入参，或新增 `ChatRequest`；旧方法保留为薄封装 |
| agent | `AgentDeps` 收敛为 turn 驱动；runner 重写循环（memory 修剪、state 注入、确定性把关）；`LoopPhase` 语义调整 |
| server | session 增加 conversation 存储；路由改为驱动 turn；版本/轨迹语义不变 |
| renderer | 需要时增加 bbox 区域截图能力（R3 起） |
| xml-core | bbox overlap 等确定性检查（R1 起步版：parse/id/no-op；R4 完整版） |
| 前端 | 交互不变（prompt + 框选 refine）；可逐步展示轮次缩略图 |

R1 先行：改动集中在 agent/llm-client/xml-core，server 路由基本不动，
回归风险最小，且单点拿到 "review 内化" 的全部收益。
