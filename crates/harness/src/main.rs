//! REPL entry: `drawio-harness <file.drawio>` — load/expand/save one
//! canonical xml file, chat with the model, or drive the tools by hand.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::path::PathBuf;
use drawio_harness::chat::{Chat, OpenAiChat};
use drawio_harness::history::{self, HistoryRec, SessionBundle};
use drawio_harness::engine::Harness;
use drawio_harness::turn_loop::HarnessRunExt;
use drawio_harness::refs;
use drawio_harness::tools::Tools;
use drawio_harness::xmlfile::{lines_in, XmlDoc};

const HELP: &str = r#"drawio-harness REPL 命令：
  /view            渲染当前文件为 PNG 并打开（需 chromium）
  /check           确定性校验（结构 + 布局 lint 摘要）
  /xml [spec]      打印文件行（spec: 行号 / cell:id / @file:lines）
  /sel spec...     选中 cell（id / 行区间），注入下一轮对话上下文
  /undo            撤销上一次编辑
  /save            保存（编辑后自动保存，这里主要用于确认）
  /reload          重新从磁盘加载（外部改动后使用）
  /help /quit
其余输入作为对话消息发给模型（需配置 DRAWIO_LLM_* env）。"#;

fn numbered(text: &str, start: usize) -> String {
    let mut out = String::new();
    for (i, l) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}| {}\n", start + i, l));
    }
    out
}

fn print_doc_summary(doc: &XmlDoc) {
    let cells = doc.cells().iter().filter(|c| c.tag == "mxCell").count();
    println!(
        "已加载 {}: {} 行, {} 个元素（其中 {} 个 mxCell）",
        doc.path.display(),
        doc.canonical().lines().count(),
        doc.cells().len(),
        cells
    );
}

fn main() {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "用法:\n  drawio-harness <file> [one-shot 消息…]      本地 REPL\n  drawio-harness new <file>                       创建空图\n  drawio-harness web [port]                       浏览器入口：会话=文件 (默认 8787)\n  drawio-harness config show|set|clear|path      查看/保存 LLM 配置"
        );
        std::process::exit(2);
    }
    if args[0] == "config" {
        config_cli(&args[1..]);
        return;
    }

    if args[0] == "metrics" {
        let Some(path) = args.get(1) else {
            eprintln!("用法: drawio-harness metrics <file.drawio>");
            std::process::exit(2);
        };
        match std::fs::read_to_string(path) {
            Ok(xml) => match drawio_harness::metrics::analyze(&xml) {
                Ok(report) => {
                    let mut out = serde_json::json!({
                        "stats": report.stats,
                        "errors": report.errors,
                        "warnings": report.warnings,
                        "info_count": report.info.len(),
                    });
                    // 效率指标：读同目录 <stem>.history.jsonl
                    let stem = std::path::Path::new(path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let hist = std::path::Path::new(path)
                        .with_file_name(format!("{stem}.history.jsonl"));
                    if let Ok(raw) = std::fs::read_to_string(&hist) {
                        let mut asks = 0usize;
                        let mut rounds = 0usize;
                        let mut tokens_in = 0u64;
                        let mut failures = 0usize;
                        for line in raw.lines() {
                            if let Ok(d) = serde_json::from_str::<serde_json::Value>(line) {
                                asks += 1;
                                rounds += d
                                    .get("events")
                                    .and_then(|e| e.as_array())
                                    .map(|e| e.len())
                                    .unwrap_or(0);
                                tokens_in += d.get("usage_in").and_then(|v| v.as_u64()).unwrap_or(0);
                                if d.get("error").and_then(|v| v.as_str()).is_some() {
                                    failures += 1;
                                }
                            }
                        }
                        out["efficiency"] = serde_json::json!({
                            "asks": asks,
                            "total_events": rounds,
                            "tokens_in": tokens_in,
                            "failed_asks": failures,
                        });
                    }
                    match serde_json::to_string_pretty(&out) {
    Ok(s) => println!("{}", s),
    Err(e) => {
        eprintln!("json serialize: {e}");
        std::process::exit(1);
    }
}
                }
                Err(e) => {
                    eprintln!("metrics 失败: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("读文件失败: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args[0] == "web" {
        // drawio-harness web [port]
        // 会话目录不再对外暴露：统一在 ~/.drawio-agent/files（DRAWIO_DIR
        // 仅作测试用内部开关，不写入文档）。
        let mut port = 8787u16;
        for a in &args[1..] {
            match a.parse::<u16>() {
                Ok(p) => port = p,
                Err(_) => {
                    eprintln!("未知参数: {a}");
                    std::process::exit(2);
                }
            }
        }
        let dir = std::env::var("DRAWIO_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                drawio_harness::config::home_dir()
                    .unwrap_or_default()
                    .join(".drawio-agent")
                    .join("files")
            });
        // 老版本把会话放在 ~/.drawio-harness/files：一次性迁到统一目录
        if !dir.exists() {
            if let Some(home) = drawio_harness::config::home_dir() {
                let legacy = home.join(".drawio-harness").join("files");
                if legacy.is_dir() {
                    if let Some(parent) = dir.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::rename(&legacy, &dir) {
                        Ok(()) => println!(
                            "已迁移会话目录: {} -> {}",
                            legacy.display(),
                            dir.display()
                        ),
                        Err(e) => eprintln!(
                            "会话目录迁移失败 {} -> {}: {e}",
                            legacy.display(),
                            dir.display()
                        ),
                    }
                }
            }
        }
        if let Err(e) = rt.block_on(drawio_harness::web::serve(dir, port)) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    let (path, one_shot_args) = if args[0] == "new" {
        // drawio-harness new <file> [one-shot 消息…]
        let Some(file) = args.get(1) else {
            eprintln!("用法: drawio-harness new <file>");
            std::process::exit(2);
        };
        let path = PathBuf::from(file);
        if !path.exists() {
            if let Err(e) = std::fs::write(&path, drawio_harness::EMPTY_TEMPLATE) {
                eprintln!("写文件失败: {e}");
                std::process::exit(1);
            }
            println!("已创建空图 {}", path.display());
        }
        (path, args[2..].to_vec())
    } else {
        (PathBuf::from(&args[0]), args[1..].to_vec())
    };
    let one_shot = !one_shot_args.is_empty();

    if !path.exists() {
        eprintln!(
            "文件不存在: {}。\n  先创建空图: cargo run -p drawio-harness -- new {}",
            path.display(),
            path.display()
        );
        std::process::exit(2);
    }

    let doc = match XmlDoc::load_with_legacy(&path, !drawio_agent_renderer::drawio_app_cached()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("加载失败: {e}");
            std::process::exit(1);
        }
    };
    // Persist the canonical form immediately (original stays on disk as
    // the compressed source of truth until the first real edit overwrites).
    if let Err(e) = doc.save() {
        eprintln!("警告: 保存规范化文件失败: {e}");
    }
    print_doc_summary(&doc);

    // CLI（one-shot/REPL）与 web 一样需要静态渲染服务器：view/导出要靠它
    // 拉起 drawio webapp。失败不致命——view 会报出同样的错误。
    match rt.block_on(drawio_agent_renderer::driver::drawio_server::init_static_server()) {
        Ok(p) => println!("静态渲染服务器: http://127.0.0.1:{p}"),
        Err(e) => eprintln!("警告: 静态渲染服务器启动失败: {e}"),
    }

    let chat: Option<OpenAiChat> = OpenAiChat::from_effective().ok();
    if chat.is_none() {
        println!("提示: LLM 未配置。配置方式: drawio-harness config set --base-url … --model …，或用 DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL / DRAWIO_LLM_API_KEY 环境变量。当前进入本地工具模式（/view /check /xml /sel 仍可用）。");
    }

    // ---- R4: interactive REPL with live trace + /stop -------------------
    // All mutable session state lives behind one tokio Mutex so a chat turn
    // can run as a background task while the main thread keeps reading
    // stdin (its only job while busy is to accept /stop).
    let repl = Arc::new(tokio::sync::Mutex::new(ReplSession {
        harness: {
            let mut h = Harness::default();
            if let Some(cfg) = drawio_harness::config::effective_settings() {
                h.max_turns = cfg.max_turns.max(1);
            }
            h
        },
        tools: Tools::new(true),
        doc,
        usage: drawio_harness::SessionStats::default(),
        pending_ctx: String::new(),
        chat: chat.map(|c| Box::new(c) as Box<dyn Chat>),
    }));

    let single_msg = if one_shot {
        Some(one_shot_args.join(" "))
    } else {
        None
    };
    if let Some(msg) = single_msg {
        // one-shot: synchronous run with live trace on stderr
        let outcome = rt.block_on(run_one_ask(repl.clone(), msg, true));
        print_turn_result(&outcome);
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<Result<drawio_harness::TurnOutcome, String>>();
    let mut running: bool = false;
    let mut abort: Option<tokio::task::AbortHandle> = None;
    let mut stdin = std::io::stdin().lock();

    loop {
        // Collect a finished turn before prompting again.
        while let Ok(res) = rx.try_recv() {
            running = false;
            abort = None;
            print_turn_result(&res);
        }
        print!("{}", if running { "(运行中… 输入 /stop 打断) diagram> " } else { "diagram> " });
        std::io::stdout().flush().ok();
        let mut l = String::new();
        match stdin.read_line(&mut l) {
            Ok(0) => {
                // stdin closed: if a turn is running, wait for its result.
                if running {
                    if let Ok(res) = rx.recv() {
                        print_turn_result(&res);
                    }
                }
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
        let line = l.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if running {
            if line == "/stop" || line == "/cancel" {
                if let Some(h) = abort.take() {
                    h.abort();
                }
                running = false;
                println!("⏹ 已停止。文件保持最近一次一致状态。");
            } else {
                eprintln!("任务运行中——输入 /stop 可打断（其它命令稍后再试）。");
            }
            continue;
        }

        if let Some(cmd) = line.strip_prefix('/') {
            let mut parts = cmd.splitn(2, char::is_whitespace);
            let cmd = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            match cmd {
                "help" => println!("{HELP}"),
                "quit" | "q" | "exit" => {
                    if running {
                        eprintln!("任务运行中——先 /stop 再退出。");
                    } else {
                        break;
                    }
                }
                "view" => {
                    let st = repl.clone();
                    let r = rt.block_on(async move {
                        let mut r = st.lock().await;
                        let ReplSession { tools, doc, .. } = &mut *r;
                        tools.view(doc, &serde_json::json!({"open": true})).await
                    });
                    match r {
                        Ok(out) => println!("{}", out.text),
                        Err(e) => eprintln!("{e}"),
                    }
                }
                "check" => {
                    let st = repl.clone();
                    rt.block_on(async move {
                        let r = st.lock().await;
                        let ReplSession { tools, doc, .. } = &*r;
                        // 走 Tools::check 而非裸 check_doc：附带布局 lint 摘要，
                        // 与模型看到的 check 结果一致。
                        match tools.check(doc) {
                            Ok(out) => println!("{}", out.text),
                            Err(e) => eprintln!("{e}"),
                        }
                    });
                }
                "xml" => {
                    let st = repl.clone();
                    let rest = rest.to_string();
                    rt.block_on(async move {
                        let r = st.lock().await;
                        let (a, b) = if rest.is_empty() {
                            (1, r.doc.canonical().lines().count())
                        } else {
                            match r.doc.resolve_range(&rest) {
                                Ok(x) => x,
                                Err(e) => {
                                    eprintln!("{e}");
                                    return;
                                }
                            }
                        };
                        println!("{}", numbered(&lines_in(r.doc.canonical(), a, b), a));
                    });
                }
                "sel" | "select" => {
                    if rest.is_empty() {
                        eprintln!("用法: /sel cell:id1 cell:id2 或行区间");
                        continue;
                    }
                    let st = repl.clone();
                    let rest = rest.to_string();
                    rt.block_on(async move {
                        let mut r = st.lock().await;
                        let tokens: Vec<String> = rest
                            .split_whitespace()
                            .flat_map(|t| t.split(','))
                            .filter(|t| !t.is_empty())
                            .map(|t| if t.starts_with('@') { t.to_string() } else { format!("@{t}") })
                            .collect();
                        let text = tokens.join(" ");
                        let (resolved, errors, snippet) = refs::resolve_refs(&text, &r.doc);
                        for e in &errors {
                            eprintln!("警告: {e}");
                        }
                        r.pending_ctx = snippet.trim().to_string();
                        if resolved.is_empty() {
                            eprintln!("没有解析到任何 cell。");
                        } else {
                            println!("已选中 {} 个引用，将附加到下一轮对话:", resolved.len());
                            println!("{snippet}");
                        }
                    });
                }
                "undo" => {
                    let st = repl.clone();
                    rt.block_on(async move {
                        let mut r = st.lock().await;
                        match r.doc.undo() {
                            Some(_) => {
                                let _ = r.doc.save();
                                println!("已撤销，文件已回滚并保存。");
                            }
                            None => println!("没有可撤销的编辑。"),
                        }
                    });
                }
                "save" => {
                    let st = repl.clone();
                    rt.block_on(async move {
                        let r = st.lock().await;
                        let _ = r.doc.save();
                        println!("已保存 {}", r.doc.path.display());
                    });
                }
                "reload" => {
                    let st = repl.clone();
                    let path = path.clone();
                    rt.block_on(async move {
                        let mut r = st.lock().await;
                        match XmlDoc::load_with_legacy(&path, !drawio_agent_renderer::drawio_app_cached()) {
                            Ok(d) => {
                                r.doc = d;
                                println!("已重新加载。");
                                print_doc_summary(&r.doc);
                            }
                            Err(e) => eprintln!("重载失败: {e}"),
                        }
                    });
                }
                "history" => {
                    let st = repl.clone();
                    let detail = rest.parse::<usize>().ok();
                    rt.block_on(async move {
                        let r = st.lock().await;
                        let p = history::history_path(&r.doc.path);
                        let recs = history::list(&p, 50);
                        if recs.is_empty() {
                            println!("还没有历史记录（文件: {}）", p.display());
                            return;
                        }
                        match detail {
                            None => {
                                println!("最近 {} 条（{}）:", recs.len(), p.display());
                                for (i, rec) in recs.iter().enumerate() {
                                    println!("[{}] {}", i, rec.summary());
                                }
                                println!("查看详情: /history <序号>");
                            }
                            Some(i) => match recs.get(i) {
                                Some(rec) => {
                                    println!("时间: {}", rec.summary());
                                    println!("轨迹:");
                                    for ev in &rec.events {
                                        let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                                        match t {
                                            "tool" => println!(
                                                "  → {} {}",
                                                ev.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                                ev.get("args").and_then(|v| v.as_str()).unwrap_or("")
                                            ),
                                            "tool_result" => println!(
                                                "  ↳ {}: {}",
                                                ev.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                                ev.get("preview").and_then(|v| v.as_str()).unwrap_or("")
                                            ),
                                            "usage" => println!(
                                                "  · tokens +{}/+{}",
                                                ev.get("in").and_then(|v| v.as_u64()).unwrap_or(0),
                                                ev.get("out").and_then(|v| v.as_u64()).unwrap_or(0)
                                            ),
                                            "reply" => println!(
                                                "  回复: {}",
                                                ev.get("reply").and_then(|v| v.as_str()).unwrap_or("")
                                            ),
                                            "error" => println!(
                                                "  错误: {}",
                                                ev.get("error").and_then(|v| v.as_str()).unwrap_or("")
                                            ),
                                            _ => {}
                                        }
                                    }
                                    println!("（xml {} 字符，可用 /restore {i} 恢复）", rec.xml.chars().count());
                                }
                                None => eprintln!("没有第 {i} 条"),
                            },
                        }
                    });
                }
                "ctx-save" => {
                    let st = repl.clone();
                    let target = rest.to_string();
                    rt.block_on(async move {
                        let r = st.lock().await;
                        let path = std::path::PathBuf::from(&target);
                        let bundle = SessionBundle {
                            version: 1,
                            file: r.doc.path.display().to_string(),
                            saved_at: history::now_secs(),
                            messages: SessionBundle::strip_images(&r.usage.transcript),
                            usage_in: r.usage.usage.input_tokens,
                            usage_out: r.usage.usage.output_tokens,
                            cost_yuan: r.usage.cost_yuan,
                            xml: r.doc.canonical().to_string(),
                        };
                        match serde_json::to_string_pretty(&bundle)
                            .map_err(|e| e.to_string())
                            .and_then(|json| std::fs::write(&path, json).map_err(|e| e.to_string()))
                        {
                            Ok(()) => println!(
                                "已保存会话上下文 -> {}（{} 条记忆消息，xml {} 字符）",
                                path.display(),
                                bundle.messages.len(),
                                bundle.xml.chars().count()
                            ),
                            Err(e) => eprintln!("保存失败: {e}"),
                        }
                    });
                }
                "ctx-load" => {
                    let st = repl.clone();
                    let target = rest.to_string();
                    rt.block_on(async move {
                        let mut r = st.lock().await;
                        let raw = match std::fs::read_to_string(&target) {
                            Ok(x) => x,
                            Err(e) => {
                                eprintln!("读取失败: {e}");
                                return;
                            }
                        };
                        let bundle: SessionBundle = match serde_json::from_str(&raw) {
                            Ok(b) => b,
                            Err(e) => {
                                eprintln!("不是有效的会话 JSON: {e}");
                                return;
                            }
                        };
                        let p = r.doc.path.clone();
                        match XmlDoc::from_text_at_with_legacy(&bundle.xml, &p, !drawio_agent_renderer::drawio_app_cached()) {
                            Ok(d) => {
                                r.doc = d;
                                let _ = r.doc.save();
                            }
                            Err(e) => {
                                eprintln!("会话 xml 无法加载: {e}");
                                return;
                            }
                        }
                        r.usage.transcript = SessionBundle::strip_images(&bundle.messages);
                        r.usage.usage = drawio_harness::chat::Usage {
                            input_tokens: bundle.usage_in,
                            output_tokens: bundle.usage_out,
                        };
                        r.usage.cost_yuan = bundle.cost_yuan;
                        println!(
                            "已加载会话 {}（{} cells，{} 条记忆消息）",
                            target,
                            r.doc.cells().len(),
                            r.usage.transcript.len()
                        );
                    });
                }
                other => println!("未知命令 /{other}（/help 查看）"),
            }
            continue;
        }

        // ---- plain text: start a chat turn in the background -------------
        let has_llm = rt.block_on(async { repl.lock().await.chat.is_some() });
        if !has_llm {
            eprintln!("未配置 LLM。可用命令: /view /check /xml /sel /undo（或 `drawio-harness config set …`）");
            continue;
        }
        let repl2 = repl.clone();
        let tx2 = tx.clone();
        running = true;
        let handle = rt.spawn(async move {
            let res = run_one_ask(repl2, line, true).await;
            let _ = tx2.send(res);
        });
        abort = Some(handle.abort_handle());
    }
}

struct ReplSession {
    harness: Harness,
    tools: Tools,
    doc: XmlDoc,
    usage: drawio_harness::SessionStats,
    pending_ctx: String,
    chat: Option<Box<dyn Chat>>,
}

/// Run one ask inside the session lock, with optional live trace.
async fn run_one_ask(
    repl: Arc<tokio::sync::Mutex<ReplSession>>,
    line: String,
    trace: bool,
) -> Result<drawio_harness::TurnOutcome, String> {
    use drawio_harness::engine::EngineEvent;
    let mut r = repl.lock().await;
    let ctx = std::mem::take(&mut r.pending_ctx);
    let ctx = if ctx.is_empty() {
        let (_, errs, snippet) = refs::resolve_refs(&line, &r.doc);
        for e in &errs {
            eprintln!("警告: {e}");
        }
        snippet
    } else {
        ctx
    };
    let cfg = drawio_harness::config::effective_settings().unwrap_or_default();
    let budget = cfg.budget_yuan;
    let opts = {
        let mut o = drawio_harness::RunOpts::from_settings(&cfg);
        o.legacy_viewer = !drawio_agent_renderer::drawio_app_cached();
        if let Some(b) = budget {
            o.budget_remaining = (b - r.usage.cost_yuan).max(0.0);
        }
        o
    };
    if r.chat.is_none() {
        return Err("LLM 未配置".into());
    }
    let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let events_cb = events.clone();
    let progress: Option<drawio_harness::engine::ProgressFn> = Some(Arc::new(move |ev: EngineEvent| {
        let evj = match &ev {
            EngineEvent::Turn { index } => serde_json::json!({ "type": "turn", "index": index }),
            EngineEvent::ModelOutput { raw } => serde_json::json!({ "type": "model", "preview": raw.chars().take(300).collect::<String>() }),
            EngineEvent::Tool { name, args } => serde_json::json!({ "type": "tool", "name": name, "args": args.chars().take(200).collect::<String>() }),
            EngineEvent::ToolResult { name, text, has_image } => serde_json::json!({
                "type": "tool_result", "name": name,
                "preview": text.lines().next().unwrap_or("").chars().take(200).collect::<String>(),
                "has_image": has_image,
            }),
            EngineEvent::Usage { usage: u, cost_yuan } => serde_json::json!({
                "type": "usage", "in": u.input_tokens, "out": u.output_tokens, "cost_yuan": cost_yuan,
            }),
            EngineEvent::Final { reply } => serde_json::json!({ "type": "reply", "reply": reply }),
        };
        if let Ok(mut v) = events_cb.lock() {
            v.push(evj.clone());
        }
        if trace {
            match ev {
                EngineEvent::Turn { index } => eprintln!("  · 模型轮次 {} …", index + 1),
                EngineEvent::ModelOutput { .. } => {}
                EngineEvent::Tool { name, args } => {
                    let a: String = args.chars().take(160).collect();
                    eprintln!("  → {name} {a}");
                }
                EngineEvent::ToolResult { name, text, has_image } => {
                    let first = text.lines().next().unwrap_or("");
                    let t: String = first.chars().take(200).collect();
                    eprintln!("  ↳ {name}: {t}{}", if has_image { " 📷" } else { "" });
                }
                EngineEvent::Usage { usage, cost_yuan } => eprintln!(
                    "  · tokens +{}/+{} ≈ ¥{:.4}",
                    usage.input_tokens, usage.output_tokens, cost_yuan
                ),
                EngineEvent::Final { .. } => {}
            }
        }
    }));
    let ReplSession { harness, tools, doc, usage, chat, .. } = &mut *r;
    let chat: &mut dyn Chat = match chat.as_mut() {
        Some(c) => c.as_mut(),
        None => {
            eprintln!("chat not available");
            return Err("LLM 未配置".into());
        }
    };
    let outcome = harness
        .run(chat, tools, doc, &line, &ctx, &opts, usage, &progress)
        .await;
    // R5: durable per-file history with the trajectory.
    let (reply, error, tool_calls) = match &outcome {
        Ok(o) => (o.reply.clone(), None, o.tool_calls),
        Err(e) => (String::new(), Some(e.clone()), 0),
    };
    let rec = HistoryRec {
        ts: history::now_secs(),
        user: line.clone(),
        cell_ids: Vec::new(), // REPL 的引用在文本里（@refs），无独立选择集
        reply,
        tool_calls: tool_calls as u32,
        usage_in: usage.usage.input_tokens,
        usage_out: usage.usage.output_tokens,
        cost_yuan: usage.cost_yuan,
        events: events.lock().map(|v| v.clone()).unwrap_or_default(),
        xml: doc.canonical().to_string(),
        error,
    };
    if let Err(e) = history::append(&history::history_path(&doc.path), &rec) {
        eprintln!("警告: 写入历史失败: {e}");
    }
    if let Err(e) = history::save_session_state(&doc.path, usage) {
        eprintln!("警告: 保存会话状态失败: {e}");
    }
    outcome
}

fn print_turn_result(res: &Result<drawio_harness::TurnOutcome, String>) {
    match res {
        Ok(outcome) => {
            if !outcome.reply.is_empty() {
                println!("── {}\n", outcome.reply);
            }
            if outcome.tool_calls > 0 {
                println!("（本轮工具调用 {} 次）", outcome.tool_calls);
            }
            let spent = outcome.cost_yuan;
            println!(
                "用量: {} in + {} out tokens{}",
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
                if spent > 0.0 { format!(" ≈ ¥{spent:.4}") } else { String::new() }
            );
        }
        Err(e) => eprintln!("对话出错: {e}"),
    }
}

// ---------------------------------------------------------------------------
// `drawio-harness config` subcommand
// ---------------------------------------------------------------------------

fn config_cli(args: &[String]) {
    use drawio_harness::config;
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("show");
    match cmd {
        "show" => match config::effective_settings() {
            Some(s) => {
                let source = match config::effective_source() {
                    config::ConfigSource::File => "配置文件",
                    config::ConfigSource::Env => "环境变量",
                    config::ConfigSource::None => "无",
                };
                println!("来源: {source}");
                if let Some(p) = config::config_file_path() {
                    if p.exists() {
                        println!("文件: {}", p.display());
                    }
                }
                println!("base_url: {}", s.base_url);
                println!("model:    {}", s.model);
                println!("api_key:  {}", s.api_key_masked());
                println!("context_length: {:?}", s.context_length);
                println!("thinking: {:?}", s.thinking);
                println!("price:    ¥{}/百万 in, ¥{}/百万 out", s.price_input_per_m, s.price_output_per_m);
                println!("budget:   {:?}", s.budget_yuan);
            }
            None => {
                eprintln!("未配置 LLM。保存方式: drawio-harness config set --base-url <url> --model <model> [--api-key <key>]");
                std::process::exit(1);
            }
        },
        "set" => {
            let mut base_url = String::new();
            let mut model = String::new();
            let mut api_key: Option<String> = None;
            let mut context_length: Option<u64> = None;
            let mut no_think = false;
            let mut price_in: Option<f64> = None;
            let mut price_out: Option<f64> = None;
            let mut budget: Option<Option<f64>> = None; // Some(None) = 清除
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--base-url" | "-b" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            base_url = v.clone();
                        }
                    }
                    "--model" | "-m" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            model = v.clone();
                        }
                    }
                    "--api-key" | "-k" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            api_key = Some(v.clone());
                        }
                    }
                    "--context-length" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            context_length = v.parse().ok();
                        }
                    }
                    "--no-think" => no_think = true,
                    "--think-default" => no_think = false,
                    "--price-in" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            price_in = v.parse().ok();
                        }
                    }
                    "--price-out" => {
                        i += 1;
                        if let Some(v) = args.get(i) {
                            price_out = v.parse().ok();
                        }
                    }
                    "--budget" => {
                        i += 1;
                        match args.get(i).map(|s| s.as_str()) {
                            Some("none") | Some("") => budget = Some(None),
                            Some(v) => budget = Some(v.parse().ok()),
                            None => {}
                        }
                    }
                    other => {
                        eprintln!("未知参数: {other}");
                        std::process::exit(2);
                    }
                }
                i += 1;
            }
            if base_url.is_empty() || model.is_empty() {
                eprintln!("需要 --base-url 与 --model。可选: --api-key --context-length --no-think|--think-default --price-in --price-out --budget <元|none>");
                std::process::exit(2);
            }
            let mut s = config::effective_settings().unwrap_or_default();
            s.base_url = base_url.trim_end_matches('/').to_string();
            s.model = model;
            if let Some(k) = api_key {
                s.api_key = k;
            }
            if context_length.is_some() {
                s.context_length = context_length;
            }
            if no_think {
                s.thinking = drawio_harness::ThinkingMode::NoThink;
            } else if args.iter().any(|a| a == "--think-default") {
                s.thinking = drawio_harness::ThinkingMode::Default;
            }
            if let Some(v) = price_in {
                s.price_input_per_m = v;
            }
            if let Some(v) = price_out {
                s.price_output_per_m = v;
            }
            if let Some(v) = budget {
                s.budget_yuan = v;
            }
            match config::config_file_path() {
                Some(p) => match config::save_config_file(&p, &s) {
                    Ok(()) => println!("已保存: {}\\nbase_url: {}\\nmodel:    {}\\napi_key:  {}", p.display(), s.base_url, s.model, s.api_key_masked()),
                    Err(e) => {
                        eprintln!("保存失败: {e}");
                        std::process::exit(1);
                    }
                },
                None => {
                    eprintln!("找不到配置文件路径");
                    std::process::exit(1);
                }
            }
        }
        "clear" => match config::config_file_path() {
            Some(p) if p.exists() => {
                if let Err(e) = std::fs::remove_file(&p) {
                    eprintln!("删除配置文件失败: {e}");
                } else {
                    println!("已删除 {}", p.display());
                }
            }
            Some(p) => {
                println!("配置文件不存在: {}", p.display());
            }
            None => eprintln!("找不到配置文件路径"),
        },
        "path" => match config::config_file_path() {
            Some(p) => println!("{}", p.display()),
            None => {
                eprintln!("找不到主目录（HOME/USERPROFILE 未设置）");
                std::process::exit(1);
            }
        },
        other => {
            eprintln!("未知子命令: {other}（可用: show set clear path）");
            std::process::exit(2);
        }
    }
}
