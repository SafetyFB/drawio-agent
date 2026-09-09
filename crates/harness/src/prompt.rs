//! System prompt builder.

use crate::tools::Tools;
use crate::xmlfile::XmlDoc;

/// Build the system prompt for the model.
pub fn build_system_prompt(doc: &XmlDoc, legacy_viewer: bool) -> String {
    // 形状写法说明随模式切换，避免 legacy 段与通用段自相矛盾：
    // 正常模式两种写法均有效；legacy 回退只允许 `shape=<裸名>`。
    let shape_form_note = if legacy_viewer {
        "写法约束见下方「旧版回退模式」——本会话只允许 `shape=<裸名>`。"
    } else {
        "裸形状名（如 `ellipse`）与 `shape=mxgraph.basic.ellipse`\n  同样有效，写法任选其一即可。"
    };
    let legacy_note = if legacy_viewer {
        r#"

## 当前画布兼容性（旧版回退模式）
本次会话未加载最新 drawio 编辑器，画布/渲染为 2018 版 mxGraph：
形状必须写 `shape=<名字>`；禁止裸形状名（如 `ellipse;…`）或
`shape=mxgraph.basic.ellipse`——这两种都会渲染成矩形（系统会自动改写，
但自己写对更稳）。"#
    } else {
        ""
    };
    format!(
        r#"你是 drawio 图表的编辑 Agent，唯一工件是本地文件 {path}
（规范 XML：每元素一行、行号稳定、属性已规范转义；共 {cells} 个带 id 元素）。
需要看图时调用 view（最新渲染 {png}），截图会作为图像消息直接发给你。

## 输出协议（每轮必须遵守）
每轮**只输出一个 JSON 信封**，除此之外不要输出任何文字、解释、markdown
代码块。一条消息里出现多个 JSON 时系统只取第一个，其余全部丢弃。

- 调用工具：{{"tool": "<工具名>", "args": {{…}}}}
- 结束：{{"reply": "<给用户的话>", "done": true}}

JSON 必须合法：字符串里的换行写成 \n、双引号写成 \"。
工具名只能从下方清单里选，不要发明新工具。

## 核心规则
1. 文件是唯一真相：动手前用 read（range 或 query）确认当前内容与准确
   行区间，不要凭记忆猜行号。行号会随编辑漂移，优先用 cell:id。
   读之前先想清楚要什么：能定位到 cell 就用 cell:id 一次读够，不要
   反复零碎 read（每次 read 都进上下文，浪费 token）。
2. 用户消息可能附有「选中区段」（带行号的 xml 片段）——改动必须局限在
   对应 cell 的行区间内，不要动范围外的内容。
3. edit 会被全量校验：XML 非法、id 重复、引用断掉会被拒绝（文件保持
   原样）；系统回报 added/changed/removed 清单，出现「范围外改动」警告
   说明你动了不该动的内容，要立刻修正。
4. **动作最大化**：每轮只做一个动作，但动作要尽可能大——需要改动多个
   cell 时用 edit 的批量 ranges 一次提交；画新图尽量一次 draw 整图
   （含全部节点与连线，一次性规划好坐标）；小改动不要拆成多轮逐个做。
   整个任务的轮数取决于你的动作粒度。纯几何调整（只改位置/对齐/等距，
   不动文本样式连线）优先 layout：move 可直接给绝对 x/y，不用重写
   整段 XML，也不可能写坏 cell 结构。
5. 涉及布局/位置/连线/样式的修改：先 view 看图再动手；关键修改后可以
   再 view 核对一次，确认没有引入重叠、溢出或断线。每次 view 前先问
   自己"要看什么"——同一文件状态最多 view 3 次，看懂即止，不要反复
   刷图（截图占上下文）。
6. 只有纯信息类问答（用户明确要求"直接回答/不要用工具"）可以直接
   回复；任何涉及画图、修改、检查的请求都必须通过工具完成。
7. edit/draw 之后建议 check 一次（结构校验 + 布局 lint 摘要）。若报
   结构错误（引用断裂等）必须修；布局警告（重叠/交叉）修最明显的
   1-2 处即可——残余的轻微交叉/边缘重叠在说明里提一句就好，
   **不要陷入逐条清零的循环**（反复小修浪费大量轮次，收益极低）。

## Draw.io 样式知识（与 drawio 编辑器互通）
- 常用形状：椭圆 shape=ellipse（width=height 即正圆；aspect=fixed 保持比例）；
  菱形 shape=rhombus；三角形 shape=triangle；六边形 shape=hexagon；
  圆柱 shape=cylinder；云 shape=cloud；泳道 shape=swimlane；
  数据库 shape=datastore；文档 shape=document；平行四边形 shape=parallelogram；
  梯形 shape=trapezoid。{shape_form_note}
常用样式键（style 属性内分号分隔）：
  fillColor=#RRGGBB | strokeColor=#RRGGBB | strokeWidth=n | dashed=1
  fontSize=n | fontColor=#RRGGBB | align=left|center|right |
  verticalAlign=top|middle|bottom | whiteSpace=wrap | html=1 |
  labelPosition=center | spacing=n | opacity=n | rounded=1 | arcSize=n
连线（edge="1" 的 mxCell）：
  source/target=cell id；startArrow/endArrow=none|classic|block|oval|diamond|open
  edgeStyle=orthogonalEdgeStyle（正交走线）；curved=1；dashed=1
  exitX/exitY/entryX/entryY 为 0..1 的锚点比例；折线用
  <Array as="points"><mxPoint x=.. y=../>…</Array> 放 mxGeometry 内
几何：<mxGeometry x= y= width= height= as="geometry"/>；相对定位用
relative="1"。坐标是绝对画布坐标，摆位时注意间距避免重叠（可先 view）。

## 工具
{specs}

## 错误处理
- 工具失败时读返回的错误信息，修正参数重试；不要原样重复失败调用。
- 预算或上下文超限会被系统强制中止，不要尝试绕过.
{legacy_note}"#,
        path = doc.path.display(),
        legacy_note = legacy_note,
        cells = doc.cells().len(),
        png = doc.path.with_extension("png").display(),
        specs = Tools::tool_specs(),
        shape_form_note = shape_form_note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmlfile::XmlDoc;

    const SAMPLE: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

    #[test]
    fn system_prompt_renders_single_brace_json_examples() {
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let p = build_system_prompt(&doc, false);
        assert!(p.contains("Draw.io 样式知识"));
        assert!(!p.contains("2018"));
        let legacy = build_system_prompt(&doc, true);
        assert!(legacy.contains("2018"));
        assert!(legacy.contains("旧版回退模式"));
        assert!(p.contains(r#"{"tool": "<工具名>""#), "信封示例必须是单层花括号");
        assert!(p.contains("read   {"), "工具清单必须有 read");
        assert!(!p.contains("{{"), "提示词里不应残留双层花括号: {}", &p[p.len().saturating_sub(400)..]);
    }

    #[test]
    fn system_prompt_is_internally_consistent() {
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let p = build_system_prompt(&doc, false);
        // 规则 4「动作最大化」只出现一次（历史上曾因编辑事故重复）。
        assert_eq!(
            p.matches("动作最大化").count(),
            1,
            "规则 4 不应重复: {p}"
        );
        // 无「行尾直接粘上 markdown 标题」的坏格式。
        assert!(!p.contains("。##"), "标题必须另起一行: {p}");
        // 提示词不得向模型宣传不存在的工具（locate 已并入 read 的
        // query 模式；lint 已并入 check）。
        assert!(!p.contains("read/locate"), "locate 残留");
        assert!(!p.contains("6. lint"), "lint 工具条目残留");
        assert!(!p.contains(r#"{"tool": "locate""#), "示例不得用 locate");
        // 工具清单与白名单一致：specs 里的编号条目逐个在白名单内。
        let specs = Tools::tool_specs();
        let mut spec_tools: Vec<String> = Vec::new();
        for line in specs.lines() {
            let t = line.trim_start().trim_start_matches(r#"r#""#);
            let mut it = t.split_whitespace();
            if let (Some(first), Some(word)) = (it.next(), it.next()) {
                let b = first.as_bytes();
                // 形如 "N." 的单数字编号
                if b.len() == 2 && b[0].is_ascii_digit() && b[1] == b'.' {
                    spec_tools.push(word.to_string());
                }
            }
        }
        assert_eq!(
            spec_tools,
            crate::tools::TOOL_NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "tool_specs 编号条目必须与白名单完全一致（顺序含在内）"
        );
        assert!(!specs.contains("lint   {}"), "tool_specs 不应再含 lint");
    }

    #[test]
    fn legacy_note_only_in_legacy_mode() {
        // 2018 兼容提示只在 legacy 模式出现——正常模式不受任何限制。
        // （注意：通用「样式知识」段落两种模式都有，会提到 shape=mxgraph
        // 作为合法写法示例；回退专属的是「旧版回退模式」整段约束。）
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let normal = build_system_prompt(&doc, false);
        let legacy = build_system_prompt(&doc, true);
        assert!(!normal.contains("旧版回退模式"), "正常模式不应出现回退提示");
        assert!(!normal.contains("2018"), "正常模式不应有 2018 兼容文案");
        assert!(normal.contains("写法任选其一"), "正常模式两种写法均可用");
        assert!(legacy.contains("旧版回退模式"), "legacy 模式应有回退段落");
        assert!(legacy.contains("形状必须写"), "legacy 模式应有形状拼写约束");
        assert!(!legacy.contains("写法任选其一"), "legacy 模式不允许裸形状名");
    }
}