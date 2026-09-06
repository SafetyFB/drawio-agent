# drawio-harness（`refactor/harness-cli` 分支）

一个"普通 agent harness"风格的 Draw.io 编辑工具：本地 chat 入口 + 工具调用
（read / locate / edit / draw / check / view），唯一工件是磁盘上一个规范
pretty-print 的 xml 文件。不再有 typed XML 模型、scope 子图协议、分阶段
Generate/Review/Patch 循环、sessions/trajectory —— 全部由
「文件 + 行区间文本编辑 + @file 式引用」替代。

设计文档：[`docs/harness-refactor.md`](./docs/harness-refactor.md)
原始愿景：[`initial_draft.md`](./initial_draft.md)（旧 6-crate 实现保留在
`main` 与 `pre-harness-refactor` tag 上）

## 概念

- **一个 xml 文件**：加载时压缩 payload 自动展开，重排为规范格式（每个
  元素一行、实体转义规范化），drawio 应用照常能打开。行号从此稳定、diff 可读。
- **span 索引**：每次保存后重建 `cell id → 行区间`。选中/框选/提及 cell 都经
  它翻译成 `@diagram.xml:120-156` 这类带行号的 xml 片段注入上下文。
- **局部性靠机制不靠协议**：编辑 = 行区间文本替换，替换后全量校验
  （XML 可解析 / id 唯一 / 引用完整），其余行字节级不动，diff 报告
  added/removed/changed 精确到 cell。
- **模型驱动**：每轮一个 JSON 信封 `{"tool": ..., "args": ...}` 或
  `{"reply": ..., "done": true}`；工具结果回填下一轮。`view` 会把渲染截图
  作为图像消息直接发给模型（视觉闭环在一条对话里完成，不再有独立的
  VLM review 阶段）。任意的 OpenAI-compatible 端点即可接入（需支持视觉
  时用多模态模型）。

## 构建与测试

```bash
cargo build
cargo test -p drawio-harness          # 27 tests
cargo test -p drawio-agent-renderer   # chromium 渲染（复用）
```

## 用法

```bash
# 浏览器是主入口：会话 = 一个 .drawio 文件（默认 ~/.drawio-harness/files）
cargo run -p drawio-harness -- web            # http://127.0.0.1:8787
cargo run -p drawio-harness -- web --dir ~/diagrams 4000

# 交互 REPL（无 LLM 也能用：/view /check /xml /sel /undo /save /reload）
cargo run -p drawio-harness -- demo.drawio

# 新建空图
cargo run -p drawio-harness -- new demo.drawio

# one-shot 对话（模型自主调用工具闭环）
DRAWIO_LLM_BASE_URL=<endpoint> DRAWIO_LLM_MODEL=<model> \
DRAWIO_LLM_API_KEY=<key> \
  cargo run -p drawio-harness -- demo.drawio "把 svc-b 改成绿色，加一条到 svc-a 的连线"
```

Web 端：**每个会话绑定一个 .drawio 文件，创建会话 = 创建文件**（下拉切换、
＋ 新建会话、🗑 删除）。每个会话有独立的图、多轮记忆、用量/预算与
`<name>.history.jsonl` 轨迹历史；⚙ 配置模型/上下文/价格/预算，`历史`
面板是只读时间线：自动展示当前会话的每次任务轨迹（工具调用/用量/回复），可导出会话 JSON；切换会话即切换历史。会话的记忆与用量随 `<name>.state.json` 持久化，重启后完整恢复；聊天实时流式渲染每轮工具
调用与 token 花费（发送中可「停止」）。

REPL（单文件模式）常用命令：`/history`（/history N 看轨迹）`/ctx-save x.json`
`/ctx-load x.json` `/sel` `/stop`。

## 代码布局

| 路径 | 内容 |
|---|---|
| `crates/harness/src/xmlfile.rs` | 规范化 / 解压 / 校验 / span 索引 / 行区间编辑 |
| `crates/harness/src/refs.rs` | @ 引用解析与上下文注入 |
| `crates/harness/src/tools.rs` | read locate edit draw check view |
| `crates/harness/src/chat.rs` | OpenAI-compatible chat（text + image parts） |
| `crates/harness/web/` + `web.rs` | 浏览器画布：mxGraph 渲染、框选 → cell ids、聊天 |
| `crates/harness/src/engine.rs` | JSON 信封循环 |
| `crates/harness/src/main.rs` | REPL + one-shot |
| `crates/renderer` | 保留的 chromium CDP 渲染 |
