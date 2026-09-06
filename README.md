# drawio-harness

一个 Draw.io AI 画图 agent：**浏览器是主入口**，本地 chat + 工具调用
（read / locate / edit / draw / check / view），唯一工件是磁盘上一个规范
pretty-print 的 `.drawio` 文件。局部性靠「文件 + 行区间文本编辑 + @file
式引用」实现，不引入 typed XML 模型与分阶段协议。

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
  作为图像消息直接发给模型（视觉闭环在一条对话里完成）。任意的
  OpenAI-compatible 端点即可接入（需支持视觉时用多模态模型）。

## 构建

```bash
cargo build
```

构建时自动下载钉住版本的 `chrome-headless-shell`（chrome-for-testing CDN，
SHA-256 校验后缓存在 `~/Library/Caches/drawio-agent/`（macOS）），供渲染与
画布截图使用。

> **注意：这个下载目前很慢**。CDN（storage.googleapis.com）直连在国内
> 网络环境下经常只有几十 KB/s，一次下载可能要十几分钟甚至超时。
> 建议开启代理的**增强模式 / TUN 模式**（让 cargo 与构建脚本的流量也走
> 代理）后再构建，通常一分钟内完成。纯离线环境用
> `DRAWIO_AGENT_OFFLINE=1 cargo build` 跳过（渲染/截图功能随之停用）。

```bash
cargo test -p drawio-harness          # 50 tests
cargo test -p drawio-agent-renderer   # 校验/渲染测试
```

## 用法（Web 主入口）

```bash
cargo run -p drawio-harness -- web                  # http://127.0.0.1:8787
cargo run -p drawio-harness -- web --dir ~/diagrams 4000   # 自定义会话目录与端口
```

打开浏览器地址即可。**会话 = 一个 `.drawio` 文件**：创建会话就是新建文件
（默认 `~/.drawio-harness/files/`），下拉切换、＋ 新建、🗑 删除。

### 首次配置（页内 ⚙ 面板）

打开页面右上角 ⚙ 设置面板，填好 LLM 接入信息即可开始：

- **Base URL / Model / API Key**：任意 OpenAI-compatible 端点（示例为
  智谱 GLM：`https://open.bigmodel.cn/api/paas/v4` + `glm-4.6v`）
- **上下文上限、输入/输出价格、预算**：控制每会话的 token 与花费统计

保存后立即生效，无需重启；Key 只显示打码形式。

### 聊天（AI 画图）

- 输入消息回车发送；模型自主调用 read / locate / edit / draw / check / view
  工具闭环改图，每轮工具调用与 token 花费**实时流式**渲染，发送中可「停止」。
- **多轮记忆**：每轮随上下文注入（超限自动裁剪），随会话持久化，重启/切换
  会话恢复。
- **选中即引用**：在画布上点选 / 框选 / Shift 追加选中的 cell 会随下一条
  消息自动附带（`@cell:boxA`），历史消息附带当时的选中信息。
- **用量与预算**：每个会话独立统计 token 与花费，超出预算拒绝执行；页面
  底部实时显示，历史轨迹存 `<name>.history.jsonl`。
- **历史 = 聊天流**：重新打开或切换会话时，上次对话、工具轨迹、用量按时间
  顺序以聊天样式重放，与实时消息同款，无感恢复。

### CLI（辅助）

```bash
# 交互 REPL（无 LLM 也能用：/view /check /xml /sel /undo /save /reload）
cargo run -p drawio-harness -- demo.drawio

# 新建空图
cargo run -p drawio-harness -- new demo.drawio

# one-shot 对话（模型自主调用工具闭环）
cargo run -p drawio-harness -- demo.drawio "把 svc-b 改成绿色，加一条到 svc-a 的连线"
```

REPL 常用命令：`/history`（`/history N` 看轨迹）`/ctx-save x.json`
`/ctx-load x.json` `/sel` `/stop`。

## 代码布局

| 路径 | 内容 |
|---|---|
| `crates/harness/src/xmlfile.rs` | 规范化 / 解压 / 校验 / span 索引 / 行区间编辑 |
| `crates/harness/src/refs.rs` | @ 引用解析与上下文注入 |
| `crates/harness/src/tools.rs` | read locate edit draw check view |
| `crates/harness/src/chat.rs` | OpenAI-compatible chat（text + image parts） |
| `crates/harness/web/` + `web.rs` | Web 入口：多会话、流式进度/打断、历史重放、mxGraph 画布 |
| `crates/harness/src/engine.rs` | JSON 信封循环、用量/预算/上下文守卫 |
| `crates/harness/src/main.rs` | CLI 入口（web / REPL / one-shot / new / config） |
| `crates/renderer` | chromium CDP 渲染（headless-shell 自动拉取 + SHA-256 校验） |
