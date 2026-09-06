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
  `{"reply": ..., "done": true}`；工具结果回填下一轮。任意的
  OpenAI-compatible 端点即可接入。

## 构建与测试

```bash
cargo build
cargo test -p drawio-harness          # 27 tests
cargo test -p drawio-agent-renderer   # chromium 渲染（复用）
```

## 用法

```bash
# 交互 REPL（无 LLM 也能用：/view /check /xml /sel /undo /save /reload）
cargo run -p drawio-harness -- demo.drawio

# 新建空图
cargo run -p drawio-harness -- new demo.drawio

# one-shot 对话（模型自主调用工具闭环）
DRAWIO_LLM_BASE_URL=<endpoint> DRAWIO_LLM_MODEL=<model> \
DRAWIO_LLM_API_KEY=<key> \
  cargo run -p drawio-harness -- demo.drawio "把 svc-b 改成绿色，加一条到 svc-a 的连线"
```

对话中可用 `@cell:svc-a` / `@120-156` / `@demo.drawio:10-12` 精确指到文件
某一部分；`/sel` 把选中引用附加到下一轮。

## 代码布局

| 路径 | 内容 |
|---|---|
| `crates/harness/src/xmlfile.rs` | 规范化 / 解压 / 校验 / span 索引 / 行区间编辑 |
| `crates/harness/src/refs.rs` | @ 引用解析与上下文注入 |
| `crates/harness/src/tools.rs` | read locate edit draw check view |
| `crates/harness/src/chat.rs` | OpenAI-compatible chat |
| `crates/harness/src/engine.rs` | JSON 信封循环 |
| `crates/harness/src/main.rs` | REPL + one-shot |
| `crates/renderer` | 保留的 chromium CDP 渲染 |
