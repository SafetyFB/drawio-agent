//! REPL entry: `drawio-harness <file.drawio>` — load/expand/save one
//! canonical xml file, chat with the model, or drive the tools by hand.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use drawio_harness::chat::{Chat, OpenAiChat};
use drawio_harness::engine::Harness;
use drawio_harness::refs;
use drawio_harness::tools::Tools;
use drawio_harness::xmlfile::{check_doc, lines_in, XmlDoc};

const HELP: &str = r#"drawio-harness REPL 命令：
  /view            渲染当前文件为 PNG 并打开（需 chromium）
  /check           确定性校验（结构 / id 唯一 / 引用完整）
  /xml [spec]      打印文件行（spec: 行号 / cell:id / @file:lines）
  /sel spec...     选中 cell（id / 行区间），注入下一轮对话上下文
  /undo            撤销上一次编辑
  /save            保存（编辑后自动保存，这里主要用于确认）
  /reload          重新从磁盘加载（外部改动后使用）
  /help /quit
其余输入作为对话消息发给模型（需配置 DRAWIO_LLM_* env）。"#;

const EMPTY_TEMPLATE: &str = r#"<mxfile host="app.diagrams.net" agent="drawio-harness"><diagram id="page-1" name="Page-1"><mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1" tooltips="1" connect="1" arrows="1" fold="1" page="1" pageScale="1" pageWidth="1169" pageHeight="826"><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel></diagram></mxfile>"#;

fn numbered(text: &str, start: usize) -> String {
    let mut out = String::new();
    for (i, l) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}| {}\n", start + i, l));
    }
    out
}

fn print_doc_summary(doc: &XmlDoc) {
    let cells = doc.cells.iter().filter(|c| c.tag == "mxCell").count();
    println!(
        "已加载 {}: {} 行, {} 个元素（其中 {} 个 mxCell）",
        doc.path.display(),
        doc.canonical().lines().count(),
        doc.cells.len(),
        cells
    );
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "用法:\n  drawio-harness <file> [one-shot 消息…]      本地 REPL\n  drawio-harness new <file>                       创建空图\n  drawio-harness web <file> [port]                浏览器画布 + 框选 + 聊天 (默认 8787)\n  drawio-harness config show|set|clear|path      查看/保存 LLM 配置 (~/.drawio-agent/config.json)"
        );
        std::process::exit(2);
    }
    if args[0] == "config" {
        config_cli(&args[1..]);
        return;
    }

    if args[0] == "web" {
        if args.len() < 2 {
            eprintln!("用法: drawio-harness web <file> [port]");
            std::process::exit(2);
        }
        let path = PathBuf::from(&args[1]);
        if !path.exists() {
            eprintln!(
                "文件不存在: {}。\n  先创建空图: cargo run -p drawio-harness -- new {}",
                path.display(),
                path.display()
            );
            std::process::exit(2);
        }
        let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(8787);
        if let Err(e) = rt.block_on(drawio_harness::web::serve(path, port)) {
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
            std::fs::write(&path, EMPTY_TEMPLATE).expect("写文件失败");
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

    let mut doc = match XmlDoc::load(&path) {
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

    let chat: Option<OpenAiChat> = match OpenAiChat::from_effective() {
        Ok(c) => Some(c),
        Err(_) => None,
    };
    if chat.is_none() {
        println!("提示: LLM 未配置。配置方式: drawio-harness config set --base-url … --model …，或用 DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL / DRAWIO_LLM_API_KEY 环境变量。当前进入本地工具模式（/view /check /xml /sel 仍可用）。");
    }

    let harness = Harness::default();
    let mut tools = Tools::new(true);
    let mut pending_ctx: String = String::new();
    let mut chat = chat.map(|c| Box::new(c) as Box<dyn Chat>);

    let single_msg = if one_shot {
        Some(one_shot_args.join(" "))
    } else {
        None
    };
    let mut stdin = std::io::stdin().lock();

    loop {
        let line = if let Some(m) = single_msg.clone() {
            Some(m)
        } else {
            print!("diagram> ");
            std::io::stdout().flush().ok();
            let mut l = String::new();
            match stdin.read_line(&mut l) {
                Ok(0) => None,
                Ok(_) => Some(l),
                Err(_) => None,
            }
        };
        let Some(line) = line else { break };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(cmd) = line.strip_prefix('/') {
            let mut parts = cmd.splitn(2, char::is_whitespace);
            let cmd = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            match cmd {
                "help" => println!("{HELP}"),
                "quit" | "q" | "exit" => break,
                "view" => match rt.block_on(tools.view(&doc, true)) {
                    Ok(out) => println!("{}", out.text),
                    Err(e) => eprintln!("{e}"),
                },
                "check" => match check_doc(doc.canonical()) {
                    Ok(r) => println!("{}", r.summarize()),
                    Err(e) => eprintln!("校验失败: {e}"),
                },
                "xml" => {
                    let (a, b) = if rest.is_empty() {
                        (1, doc.canonical().lines().count())
                    } else {
                        match doc.resolve_range(rest) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("{e}");
                                continue;
                            }
                        }
                    };
                    println!("{}", numbered(&lines_in(doc.canonical(), a, b), a));
                }
                "sel" | "select" => {
                    if rest.is_empty() {
                        eprintln!("用法: /sel cell:id1 cell:id2 或行区间");
                        continue;
                    }
                    let tokens: Vec<String> = rest
                        .split_whitespace()
                        .flat_map(|t| t.split(','))
                        .filter(|t| !t.is_empty())
                        .map(|t| if t.starts_with('@') { t.to_string() } else { format!("@{t}") })
                        .collect();
                    let text = tokens.join(" ");
                    let (resolved, errors, snippet) = refs::resolve_refs(&text, &doc);
                    for e in &errors {
                        eprintln!("警告: {e}");
                    }
                    pending_ctx = snippet.trim().to_string();
                    if resolved.is_empty() {
                        eprintln!("没有解析到任何 cell。");
                    } else {
                        println!("已选中 {} 个引用，将附加到下一轮对话:", resolved.len());
                        println!("{snippet}");
                    }
                }
                "undo" => match doc.undo() {
                    Some(_) => {
                        let _ = doc.save();
                        println!("已撤销，文件已回滚并保存。");
                    }
                    None => println!("没有可撤销的编辑。"),
                },
                "save" => {
                    let _ = doc.save();
                    println!("已保存 {}", doc.path.display());
                }
                "reload" => match XmlDoc::load(&path) {
                    Ok(d) => {
                        doc = d;
                        println!("已重新加载。");
                        print_doc_summary(&doc);
                    }
                    Err(e) => eprintln!("重载失败: {e}"),
                },
                other => println!("未知命令 /{other}（/help 查看）"),
            }
        } else {
            match chat.as_mut() {
                None => {
                    eprintln!(
                        "未配置 LLM。可用命令: /view /check /xml /sel /undo（或设置 DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL）"
                    );
                }
                Some(c) => {
                    let ctx = std::mem::take(&mut pending_ctx);
                    let ctx = if ctx.is_empty() {
                        // user typed @refs inline -> let the engine resolve
                        let (_, errs, snippet) = refs::resolve_refs(&line, &doc);
                        for e in &errs {
                            eprintln!("警告: {e}");
                        }
                        snippet
                    } else {
                        ctx
                    };
                    match rt.block_on(harness.run(c.as_mut(), &mut tools, &mut doc, &line, &ctx)) {
                        Ok(outcome) => {
                            if !outcome.reply.is_empty() {
                                println!("── {}\n", outcome.reply);
                            }
                            if outcome.tool_calls > 0 {
                                println!("（本轮工具调用 {} 次）", outcome.tool_calls);
                            }
                        }
                        Err(e) => eprintln!("对话出错: {e}"),
                    }
                }
            }
        }

        if one_shot {
            break;
        }
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
                    other => {
                        eprintln!("未知参数: {other}");
                        std::process::exit(2);
                    }
                }
                i += 1;
            }
            if base_url.is_empty() || model.is_empty() {
                eprintln!("需要 --base-url 与 --model（--api-key 可选）");
                std::process::exit(2);
            }
            let mut s = config::effective_settings().unwrap_or_default();
            s.base_url = base_url.trim_end_matches('/').to_string();
            s.model = model;
            if let Some(k) = api_key {
                s.api_key = k;
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
                std::fs::remove_file(&p).expect("删除配置文件失败");
                println!("已删除 {}", p.display());
            }
            Some(p) => {
                println!("配置文件不存在: {}", p.display());
            }
            None => eprintln!("找不到配置文件路径"),
        },
        "path" => match config::config_file_path() {
            Some(p) => println!("{}", p.display()),
            None => {
                eprintln!("HOME 未设置");
                std::process::exit(1);
            }
        },
        other => {
            eprintln!("未知子命令: {other}（可用: show set clear path）");
            std::process::exit(2);
        }
    }
}
