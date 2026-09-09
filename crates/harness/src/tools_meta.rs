// Tool metadata - single source of truth for TOOL_NAMES and tool_specs.
// This file is included by both tools.rs and build.rs.

/// Tool metadata: (name, description for specs)
pub const TOOL_META: &[(&str, &str)] = &[
    (
        "read",
        r#"{"range": "120-156" | "cell:svc-a" | "120"}
          或 {"cells": ["svc-a", "e3", "40-80"]}（批量：一次读多个 cell/区间）
          或 {"query": "order"}（按文本搜 cell，返回命中 cell 与 @行区间，最多 12 条）
          或 {"outline": true}（全图概览：每实体一行「行区间 | id | 类型 | 标签」；实体多时加 "offset" 分页续读）
    返回文件中指定区间的原文（带行号——这些行号就是 edit 的行号）。
    改之前先读；范围尽量小（单次超 150 行会截断并给出续读 range）。
    行号会随编辑漂移，优先用 cell:id。"#,
    ),
    (
        "edit",
        r#"{"range": "120-156" | "cell:svc-a", "text": "<完整 XML 片段>"}
          或批量 {"ranges": [{"range": "...", "text": "..."}, ...]}
    把 range 覆盖的行整体替换为 text。text 必须是**完整自洽的 XML**：
    开闭标签齐全、属性完整（如 mxGeometry 要带 as="geometry"、mxCell 要带 parent/vertex），新增 cell 用新的唯一 id，连线要有 source/target。只改目标 cell，其余必须字节不变——系统校验后回报 added/changed/removed 清单，出现越界改动会被警告。
    **删除**：text 传空即删除该区间。删节点/容器时必须**连同引用它的边/子元素一起删**（先 read {"query": "<id>"} 找到所有引用方，再批量 ranges 一批删净）——只删节点会因断引用被拒绝，错误里会点名悬空的边。
    **批量（ranges 数组）**：一次提交多个不重叠的区间（行号都按当前文件），全部通过才落盘、任一失败整体不动（全或无）。
    规则：需要改动 2 个及以上 cell 时**必须**用批量一次提交，禁止逐个 cell 单独 edit（那会浪费大量轮次）。"#,
    ),
    (
        "draw",
        r#"{"xml": "<mxGraphModel>…</mxGraphModel>"}
    整图重建（新画一张图或大改布局时用）。推荐直接给 mxGraphModel
    （系统自动补 mxfile/diagram 外壳）；给完整 mxfile 也行。
    mxGraphModel 内必须有 <root>（含锚点 0/1）。开闭标签必须逐层配对——
    漏闭标签（如少写 </mxGraphModel>）会被解析拒绝，错误里带正确骨架。
    边建议 style 带 edgeStyle=orthogonalEdgeStyle（正交走线）——裸边是
    直线，在多层图里会斜穿泳道和其它边造成遮挡。节点较多（≥6）时：
    画完先排好节点位置（层次/间距），再用 layout route 做一次避障布线，
    不要逐边手工调锚点。"#,
    ),
    (
        "check",
        r#"{}
    确定性校验：结构（XML 合法、id 唯一、parent/source/target 引用完整）+ 布局 lint 摘要（重叠/连线交叉/标签溢出/越界/分支未平行）。
    edit/draw 之后建议调用。结构错误（引用断裂等）必须修；布局警告修最明显的 1-2 处即可，**不要逐条清零**（烧轮次收益极低）——视觉与语义层面的把关用 view 看图自己判断。"#,
    ),
    (
        "layout",
        r#"{"move": {"ids": [...], "dx": n, "dy": n}}（统一偏移）
          或 {"move": [{"id": "a", "x": 400, "y": 200}, {"id": "b", "dx": 0, "dy": -40}]}（绝对/相对可混用）
          或 {"align": {"ids": [...], "axis": "x"|"y", "mode": "left"|"right"|"center"|"top"|"bottom"|"middle"|"gap"}}
          或 {"route": {}}（全图避障布线；也可 {"route": {"ids": [...]}} 先校验指定边）
    几何级工具：移动/对齐/等距分布多个 cell，或避障布线。只动 mxGeometry/style，不碰文本与结构（那些用 edit）；整批一次落盘，失败整体回滚；结果报告直接带每个 cell 的新坐标，不用再 read 确认。
    align 组合约束：axis=x 配 left/right/center，axis=y 配 top/bottom/middle，gap 两轴均可（沿轴等距分布）。
    route 的边界：只管走线、不管布局——它保证不穿节点，但节点本身位置不合理（层次不清/间距拥挤）时布线仍会乱。**先用 move/align 把节点排顺，再 route 一次**；不要靠逐边手调锚点去救乱的布局。
    何时用：**纯位置调整（改坐标/对齐/排布）一律优先 layout**，而不是 edit 重写整个 cell——放到哪直接给绝对 x/y，等距对齐交给 align 算，不用自己做算术，也绝不会写坏 cell 结构。出现「线穿节点/线乱」：先排布局再 route，一次搞定。"#,
    ),
    (
        "view",
        r#"{} 或 {"annotate": true} 或 {"focus": ["svc-a", "db"]}
    渲染当前文件为截图并作为图像消息发给你——你会真正看到这张图。
    - annotate=true：图上叠加红色小徽章标注 cell id（密集处自动避让），用于把视觉元素与 cell id 对应起来
    - focus=[id...]：只渲染这些 cell 的局部放大图（密集区域看细节用）
    检查：节点重叠、文字溢出框体、连线错位/穿框、箭头方向、布局失衡。
    看完再决定改哪里；不要连续重复调用（上一张图已经在你的上下文里，文件未变时重复 view 不会产生新图）。
    画布坐标与 xml 行区间没有 1:1 对应：定位用 read query，几何值用 read。"#,
    ),
];