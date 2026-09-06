# Harness 重构（分支 `refactor/harness-cli`）

> 目标：把 6-crate 的定制编排架构（typed XML 模型 + scope 子图协议 + 单上下文
> loop + sessions/versions/trajectory + SPA）**彻底简化**为一个"普通 agent harness"
> 风格的本地 CLI：LLM 驱动工具调用，唯一工件是磁盘上一个 XML 文件。

## 为什么可以简化

v2 架构里的复杂机制大多是为"无文件、无多轮、无工具"的约束服务的：

| v2 机制 | 解决的问题 | harness 下的替代 |
|---|---|---|
| typed mxfile 模型 + fidelity 保真层 | 服务端 merge 需要结构化 diff | 文件本身就是真相；编辑 = 行区间文本替换，未触碰字节天然不变 |
| scope 子图提取 / cell_id merge 协议 | 让模型只改局部、防全图重写 | 模型被要求只输出目标行区间的替换文本；剩余行原样保留 |
| Generate/Review/Fix 分阶段 prompt | 编排模型行为 | 工具协议（draw/edit/view/check）让模型自己决定何时看、何时改 |
| sessions / versions / trajectory | 无状态 HTTP 下的审计 | 本地文件 + `git`/`.bak` 就是版本与轨迹 |
| LLM 双模型（codegen + VLM review） | 视觉反思闭环 | 一个多模态 chat：模型自己 `view` 看图、自己改 |
| 3.4MB SPA + mxGraph 画布框选 | 可视化交互 | CLI + 渲染 PNG；框选降级为 cell id / @行区间 提示 |

## 工件模型：一个规范 pretty-print 的 xml 文件

Drawio 原始文件通常压缩且单行 —— 行号无意义、diff 不可读。
加载时统一转换为**规范格式**并落盘：

- 每个元素一行，缩进 2 空格；空元素自闭合；属性顺序保留；
- 属性值 unescape → re-escape（`& < > "` + 控制符 `\n \r \t` → `&#10; &#13; &#9;`），
  保证多行 label 字节级往返；
- 压缩 `mxfile` 解压展开（drawio 应用可正常打开非压缩 XML 的 `.drawio` 文件）。

**span 索引**：每次保存后重扫一遍，记录 `cell id → (起始行, 结束行)`。
选择/框选/提及 cell 时经索引翻译成 `@diagram.xml:120-156` 形式的文本指针注入
上下文 —— 模型被精确告知"是 xml 文件的这一部分"，而不是拿到一段脱离文件
位置的子图。

## 工具协议（模型侧）

模型每轮输出一个 JSON 信封（系统 prompt 约束，不做 response_format 强依赖）：

```json
{ "tool": "edit", "args": { "range": "120-156", "text": "<mxCell .../>" } }
{ "tool": "view", "args": {} }
{ "tool": "locate", "args": { "query": "order service" } }
{ "tool": "check", "args": {} }
{ "reply": "...", "done": true }
```

- `edit` — 行区间替换。替换后全文件重新规范化 + 校验 + 重扫索引；
  向模型报告 added/changed/removed cell 清单与 no-op 判定。
- `view` — chromium 渲染当前文件为 PNG（复用 renderer crate），macOS 上 `open` 展示。
- `locate` — 按 id/文本搜索返回 `@file:行区间`。
- `check` — 确定性校验：XML 可解析、id 唯一、引用完整、mxGraphModel 结构正确。
- 模型文本里的 `@diagram.xml:行区间` / `@cell:id` 记号在注入前由 harness
  解析成上下文片段（aider 的 @file 语义）。

## 代码布局（新工作区 = renderer + harness）

- `crates/renderer` — **保留**（chromium CDP 渲染，唯一硬骨头）。
- `crates/harness` — **新增**：
  - `xmlfile.rs` — 规范 pretty-print / 解压加载 / 校验 / span 索引 / 行区间编辑
  - `refs.rs` — @file / @cell 记号解析与上下文注入
  - `chat.rs` — OpenAI-compatible chat（文本 + 可选 data-URI 图像 part）
  - `tools.rs` — 工具注册与执行（edit/view/locate/check + 人类 /draw）
  - `loop.rs` — harness 主循环（信封解析、工具执行、结果回填、上限轮数）
  - `main.rs` — REPL：人类命令 `/view /select /undo /xml /quit`
- 删除 `xml-core` `llm-client` `agent` `trajectory` `server`（历史在 main 上）。

## 里程碑

- [x] M0 分支 + 本文档
- [x] M1 xmlfile：规范化/往返/span 索引/校验（单元测试先行）
- [x] M2 refs：@记号解析 → 上下文注入
- [x] M3 工具 + REPL：人工驱动 edit/view/undo 全流程可用（无 LLM 也能干活）
- [x] M4 chat + loop：接入任意 OpenAI-compatible 端点（`DRAWIO_LLM_*` env），
      模型驱动工具闭环（已用 GLM 真实端点 E2E 验证：模型自主 3-4 次工具调用，
      只有目标 cell 变化，其余 cell 字节级不变）
- [x] M5 view 带图像 part 注入模型（视觉自审闭环回到一条 chat 里）：
      `Message` 支持 text+image parts（OpenAI 风格 data-URI，GLM-4.6v 实测）；
      view 工具把截图作为图像消息回填，模型真正“看见”再编辑；旧截图在
      新截图到达后自动折叠，上下文至多保留最近一张。E2E：两个重叠节点，
      模型自主 7 次工具调用（view→定位→edit→view 核对→done）修复对齐，
      svc-a 位置零改动
- [x] M6 web 画布瘦壳：`drawio-harness web <file> [port]`（默认 8787）。
      单进程 = REPL 同款 XmlDoc/Tools/engine；浏览器里 mxGraph 渲染 + 框选
      （复用旧 UI 验证过的 marquee/hitTest/patch 模式），框选 cell ids 随
      下一条消息 POST，服务端经 span 索引翻译成 @cell 引用上下文 —— 与
      REPL 的 /sel 完全同一条路径。另有 /api/check /undo /reload 与检查/撤销/
      重载按钮；viewer bundle 从 renderer assets 单份读取不复制。
      E2E：POST /api/chat 携带 cell_ids=[svc-b]，模型 4 次工具调用完成
      "橙色虚线边框 + 不重叠"修改，文件落盘。

## 并发模型（web）

引擎运行期间**不持有大锁**：任务开始时把会话（doc+stats）从状态里"拿走"，
结束时（成功/失败/取消/客户端断开）统一走一条收尾路径"归还"。
- 忙 = 槽位为空（没有 running 标志、没有 try_lock、没有锁顺序 —— 卡死类
  竞态在构造上不存在）
- 取消 = 置位 AtomicBool，引擎的 select 监视器（500ms）取消当前 await 的
  网络调用后自然收尾；客户端断开走同一监视器（Sender::is_closed）
- /api/state 运行中用快照即时响应；/api/file 直读磁盘（每次 edit 即时落盘）
- 断开/取消的任务不写入历史，刷新重放不会被污染

## 当前仓库布局

```
Cargo.toml          workspace = renderer + harness
crates/renderer     保留：chromium CDP 渲染（find/launch/render + 3.4MB viewer bundle）
crates/harness     新：二进制 drawio-harness
  src/xmlfile.rs    唯一工件：规范 pretty-print mxfile + span 索引 + 行区间编辑
  src/refs.rs       @cell:id / @lines / @file:lines → 带行号的 xml 片段（aider 风格）
  src/tools.rs      read locate edit draw check view（模型信封与 REPL 共用）
  src/chat.rs       OpenAI-compatible chat（DRAWIO_LLM_BASE_URL/MODEL/API_KEY）
  src/engine.rs     JSON 信封循环：{tool,args} | {reply,done}，解析容错 + 一次纠错重试
  src/main.rs       REPL：/view /check /xml /sel /undo /save /reload + one-shot 模式
```

旧 crate（xml-core/llm-client/agent/trajectory/server）保留在
`pre-harness-refactor` tag 与 `main` 上，可随时对照。

## 用法

```bash
cargo run -p drawio-harness -- path/to/diagram.drawio        # 交互 REPL
cargo run -p drawio-harness -- new fresh.drawio              # 空图
cargo run -p drawio-harness -- web demo.drawio [port]        # 浏览器画布+框选+聊天
DRAWIO_LLM_BASE_URL=… DRAWIO_LLM_MODEL=… DRAWIO_LLM_API_KEY=… \
  cargo run -p drawio-harness -- demo.drawio "把 svc-b 改成绿色"  # one-shot
```

文件首次加载即被展开/规范化为每行一个元素的 mxfile（`compressed="false"`），
drawio 应用仍可正常打开；后续所有编辑都是行区间文本替换，未触碰的行
字节级不变。
