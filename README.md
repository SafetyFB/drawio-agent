# drawio-harness

一个 Draw.io AI 画图 agent：**浏览器是主入口**，本地 chat + 工具调用
（read / edit / draw / check / view / layout），唯一工件是磁盘上一个规范
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
- **工具五类**：
  - 查询 `read`（`range` 读行区间 / `query` 按文本搜 cell）
  - 内容 `edit`（单区间或批量 `ranges`，全或无原子落盘）、`draw`（整图重建）
  - 几何 `layout`（`align` 对齐/等距、`move` 批量平移）
  - 校验 `check`（结构 + 布局 lint 摘要：重叠/交叉/标签溢出/越界/分支平行）
  - 感知 `view`（截图，`annotate` id 徽章标注、`focus` 局部裁剪放大）

## 构建与首次运行

```bash
cargo build
cargo test -p drawio-harness          # 66 tests
cargo test -p drawio-agent-renderer   # 校验/渲染测试
```

首次使用时会按需下载两个依赖（均为钉住版本 + SHA-256 校验，缓存于
`~/Library/Caches/drawio-agent/`（macOS））：

1. **drawio webapp**（首次运行 web 时）：官方 GitHub release 的
   `draw.war`（~54MB，解压后完整的最新版 drawio 编辑器），供画布编辑器
   与无头渲染共用
2. **chrome-headless-shell**（~90MB，web 启动时与 war **并行预热**下载，
   避免任务中第一次 view/导出卡网络）：无头渲染宿主。**如果系统已装
   Chrome / Chromium / Edge / Brave，则直接复用、完全不下载**
   （解析顺序：显式路径 → 已缓存 bundle → 系统浏览器 → 下载）

> **注意：这两个下载在国内网络下都很慢**（storage.googleapis.com /
> github.com）。建议开启代理的**增强模式 / TUN 模式**（让运行时流量也
> 走代理）后再启动。纯离线环境用 `DRAWIO_AGENT_OFFLINE=1`；web 启动时
> 若 war 未缓存，画布自动回退到内置的简化 mxGraph 画布（编辑能力受限，
> 模型提示词会同步附上旧版形状约束）。

## 用法（Web 主入口）

```bash
cargo run -p drawio-harness -- web       # http://127.0.0.1:8787
cargo run -p drawio-harness -- web 4000  # 自定义端口
```

打开浏览器地址即可。**会话 = 一个 `.drawio` 文件**：创建会话就是新建文件。
所有数据统一放在一个目录下（`~/.drawio-agent/`）：`config.json`（配置）与
`files/`（会话 `.drawio` + `<name>.history.jsonl` 轨迹 + `<name>.state.json`
记忆/用量）。下拉切换、＋ 新建、🗑 删除。

### 首次配置（页内 ⚙ 面板）

打开页面右上角 ⚙ 设置面板，填好 LLM 接入信息即可开始：

- **Base URL / Model / API Key**：任意 OpenAI-compatible 端点
- **上下文上限、单次最大轮数、输入/输出价格、预算**：控制每次任务的
  token 统计、轮数上限与花费

保存后立即生效，无需重启；Key 只显示打码形式。

### 聊天（AI 画图）

- 输入消息回车发送；模型自主调用 read / edit / draw / check / view / layout
  工具闭环改图，每轮工具调用与 token 花费**实时流式**渲染，发送中可「停止」。
- **多轮记忆**：每轮随上下文注入（超限自动裁剪），随会话持久化，重启/切换
  会话恢复。
- **选中即引用**：画布上点选 / 框选的 cell 会随下一条消息自动附带
  （`@cell:boxA`），历史消息附带当时的选中信息。
- **用量与预算**：每个会话独立统计 token 与花费，超出预算拒绝执行；页面
  底部实时显示，历史轨迹存 `<name>.history.jsonl`。
- **历史 = 聊天流**：重新打开或切换会话时，上次对话、工具轨迹、用量按时间
  顺序以聊天样式重放，与实时消息同款，无感恢复。

### 画布（最新 drawio 原生编辑器）

画布内嵌完整的最新版 drawio：拖动、连线、文字、样式面板、对齐、图库等
全部原生可用；会话切换与模型编辑后自动加载最新文件。

- **手动编辑自动落盘**：改动防抖 600ms 同步到服务端 canonicalize 后写回
  文件（任务运行中暂缓），不写聊天历史
- **选中随消息附带**：内置 sel 桥插件，点选/框选即进入下一条消息的引用
- **导出**：用 drawio 自带的 File → Export（PNG/SVG/XML 原生精确导出）
- 模型 view 截图与画布同一引擎渲染，所见即所得

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
`/ctx-load x.json` `/sel` `/stop`。`/view` 用 drawio 原生导出渲染 PNG
（需要 webapp 已缓存）。

## 代码布局

| 路径 | 内容 |
|---|---|
| `crates/harness/src/xmlfile.rs` | 规范化 / 解压 / 校验 / span 索引 / 行区间编辑 |
| `crates/harness/src/refs.rs` | @ 引用解析与上下文注入 |
| `crates/harness/src/tools.rs` | read edit draw check view layout |
| `crates/harness/src/chat.rs` | OpenAI-compatible chat（text + image parts） |
| `crates/harness/web/` + `web.rs` | Web 入口：多会话、流式进度/打断、历史重放、drawio iframe 编辑器 + sel 插件桥（离线回退 mxGraph 画布） |
| `crates/harness/src/engine.rs` | JSON 信封循环、用量/预算/上下文守卫 |
| `crates/harness/src/main.rs` | CLI 入口（web / REPL / one-shot / new / config） |
| `crates/renderer` | 无头渲染：drawio webapp 获取（draw.war）+ 原生 export 协议驱动（headless-shell 为宿主） |
