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
        eprintln!("用法: drawio-harness <file.drawio|file.xml> [one-shot 对话消息…]\n       drawio-harness new <file>   创建空图");
        std::process::exit(2);
    }

    let mut path = PathBuf::from(&args[0]);
    let one_shot = args.len() > 1;

    if path.to_string_lossy() == "new" && args.len() >= 3 {
        path = PathBuf::from(&args[2]);
        if !path.exists() {
            std::fs::write(&path, EMPTY_TEMPLATE).expect("写文件失败");
            println!("已创建空图 {}", path.display());
        }
    }
    if !path.exists() {
        eprintln!("文件不存在: {}（用 `drawio-harness new {}` 创建空图）", path.display(), path.display());
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

    let chat: Option<OpenAiChat> = match OpenAiChat::from_env() {
        Ok(c) => Some(c),
        Err(_) => None,
    };
    if chat.is_none() {
        println!(
            "提示: 未检测到 DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL（可用 DRAWIO_LLM_API_KEY 选填），进入本地工具模式。"
        );
    }

    let harness = Harness::default();
    let mut tools = Tools::new(true, true);
    let mut pending_ctx: String = String::new();
    let mut chat = chat.map(|c| Box::new(c) as Box<dyn Chat>);

    let single_msg = if one_shot {
        Some(args[1..].join(" "))
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
                "view" => match rt.block_on(tools.view(&doc)) {
                    Ok(msg) => println!("{msg}"),
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
