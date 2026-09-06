/* drawio harness web shell — mxGraph canvas + selection + one chat.
   Canvas patterns (embed / codec / marquee / bundle patches) are lifted
   from the old server UI and trimmed to this single-file workflow. */
'use strict';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const canvasEl = document.getElementById('canvas');
const placeholderEl = document.getElementById('placeholder');
let currentGraph = null;   // mxGraph instance (rebuilt on every xml reload)
let canvasMode = 'select'; // 'select' (marquee) | 'pan'
let selectedIds = [];      // cell ids of the current canvas selection
let rubberBand = null;
let rubberBandEl = null;
let busy = false;
let handleLayer = null; // 自绘 resize/rotate 手柄层（每次 loadXml 重建）
let handleCell = null;  // 当前显示手柄的 cell（仅单选中叶子 cell）

/// 任务运行中时，其它需要大锁的按钮直接提示，避免请求挂起等待。
function guardBusy() {
  if (busy) {
    log('error', '任务运行中——先点「停止」再操作');
    return true;
  }
  return false;
}

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------------------
// Canvas: load xml -> mxGraph
// ---------------------------------------------------------------------------

function patchGraphForBundle(graph) {
  // The vendored draw.io viewer bundle calls Graph.prototype methods that
  // base mxGraph lacks; without stubs setSelectionCells throws mid-update.
  if (typeof graph.isTableCell !== 'function') graph.isTableCell = () => false;
  if (typeof graph.isTableRow !== 'function') graph.isTableRow = () => false;
  if (typeof graph.isTable !== 'function') graph.isTable = () => false;
  if (typeof graph.getLinksForState !== 'function') graph.getLinksForState = () => [];
  // drawio 的 Graph 子类才有 getStartEditingCell；基础 mxGraph 没有，
  // 而 bundle 的 mxCellEditor.startEditing 会调用它 → 双击编辑报错。
  if (typeof graph.getStartEditingCell !== 'function') {
    graph.getStartEditingCell = (cell) => cell;
  }
}

function loadXmlIntoCanvas(xml) {
  if (currentGraph) { try { currentGraph.destroy(); } catch (e) {} currentGraph = null; }
  canvasEl.innerHTML = '';
  if (!xml || !xml.trim()) { showPlaceholder('文件为空'); return; }
  if (typeof window.mxGraph === 'undefined') {
    showPlaceholder('viewer bundle 未加载（/vendor/viewer-static.min.js 404？）');
    return;
  }
  try {
    const xmlDoc = window.mxUtils.parseXml(xml);
    const models = xmlDoc.getElementsByTagName('mxGraphModel');
    if (!models.length) {
      // Diagnostic instead of a bare "no <mxGraphModel>": corrupted decls or
      // non-drawio files fail here, and the head of the xml tells the story.
      const parserErr = xmlDoc.getElementsByTagName('parsererror');
      const head = xml.slice(0, 200).replace(/\s+/g, ' ');
      const root = xmlDoc.documentElement ? xmlDoc.documentElement.nodeName : '(parse failed)';
      throw new Error(
        `no <mxGraphModel> found (root=${root}${parserErr.length ? ', xml parse error' : ''}). ` +
        `文件开头: ${head}。若 decl 损坏请重新加载文件（harness 会自动修复），或确认这是 drawio 文件。`
      );
    }
    const model = new window.mxGraphModel();
    const codec = new window.mxCodec(xmlDoc);
    codec.decode(models[0], model);

    const graph = new window.mxGraph(canvasEl, model);
    graph.setEnabled(true);
    graph.setCellsEditable(false);
    graph.setCellsMovable(false);
    graph.setCellsResizable(false);
    graph.setCellsSelectable(true);
    if (typeof graph.setCellsConnectable === 'function') graph.setCellsConnectable(false);
    graph.centerZoom = true;
    graph.container.style.touchAction = 'none';
    patchGraphForBundle(graph);
    currentGraph = graph;
    handleLayer = document.createElement('div');
    handleLayer.id = 'cell-handles';
    canvasEl.appendChild(handleLayer);
    handleLayer.addEventListener('pointerdown', (e) => {
      if (busy || !currentGraph || canvasMode !== 'select' || !handleCell) return;
      const dir = e.target && e.target.dataset ? e.target.dataset.dir : null;
      if (!dir) return;
      e.stopPropagation();
      e.preventDefault();
      window.__pdHandled = true; // 阻断 fork 的 mousedown 兼容事件
      if (dir === 'rotate') beginRotateDrag(e, handleCell);
      else beginResizeDrag(e, handleCell, dir);
    });
    // 缩放标签跟随 scale（常驻）
    const origSat = graph.view.scaleAndTranslate.bind(graph.view);
    graph.view.scaleAndTranslate = function (a, b, c) {
      origSat(a, b, c);
      const el = document.getElementById('mode-zoom-100');
      if (el) el.textContent = Math.round(a * 100) + '%';
      if (window.__overlayPaint) window.__overlayPaint();
      applyRotations();
      refreshHandles();
    };
    applyMode();

    graph.getSelectionModel().addListener(window.mxEvent.SELECTION_CHANGED, () => {
      const ids = graph.getSelectionCells()
        .filter((c) => c.id && c.id !== '0' && c.id !== '1')
        .map((c) => c.id);
      setSelection(ids);
      refreshHandles();
    });

    // mini editor：拖动/缩放/旋转全部自实现。fork 自带的手柄可见但拖动
    // 无效（官方拖动路径已死），关掉以免误导；我们自绘手柄接管。
    graph.setCellsMovable(false);
    graph.setCellsResizable(false);
    graph.setCellsEditable(false); // fork 编辑器焦点/提交不可靠，自实现
    graph.setConnectable(false);
    // 初始 fit：模型几何计算，与 scale 无关
    {
      const model = graph.getModel();
      let x0 = Infinity, y0 = Infinity;
      const walk = (c) => {
        if (c.geometry && !model.isEdge(c)) {
          x0 = Math.min(x0, c.geometry.x);
          y0 = Math.min(y0, c.geometry.y);
        }
        for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
      };
      walk(model.getRoot());
      if (x0 === Infinity) { x0 = 0; y0 = 0; }
      graph.view.scaleAndTranslate(1, 24 - x0, 24 - y0);
    }
    hidePlaceholder();
  } catch (err) {
    showPlaceholder('mxGraph 渲染失败: ' + (err && err.message ? err.message : err));
  }
}

function showPlaceholder(msg) {
  $('placeholder-msg').textContent = msg;
  placeholderEl.hidden = false;
}
function hidePlaceholder() { placeholderEl.hidden = true; }

// ---------------------------------------------------------------------------
// Pan / select modes + marquee (rubber-band) selection
// ---------------------------------------------------------------------------

function applyMode() {
  $('mode-select').classList.toggle('active', canvasMode === 'select');
  $('mode-pan').classList.toggle('active', canvasMode === 'pan');
  if (!currentGraph) return;
  if (canvasMode === 'pan') {
    currentGraph.setPanning(false); // bundle 的 panningHandler 不可靠，自实现
    currentGraph.setCellsSelectable(false); // 纯导航：拖动平移，不选 cell
  } else {
    currentGraph.setPanning(false);
    currentGraph.setCellsSelectable(true);  // 点选/框选
  }
  cancelRubberBand();
  refreshHandles();
}
// ---- 导出（PNG 走服务端 chromium 渲染；SVG 用 fork 的 getSvg；XML 直读磁盘） ----
function exportStem() {
  const p = picker.value || 'diagram';
  return p.replace(/\.drawio$/, '');
}
function downloadBlob(blob, name) {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = name;
  document.body.appendChild(a);
  a.click();
  setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
}
async function downloadUrl(url, name) {
  const r = await fetch(url);
  if (!r.ok) { log('error', '导出失败: ' + (await r.text())); return; }
  downloadBlob(await r.blob(), name);
}
$('export-png').onclick = () => downloadUrl('/api/export/png', exportStem() + '.png');
/// 导出 SVG：fork 把 translate/scale 全烤进 state 且 DOM 直接按 state
/// 渲染（容器像素），所以克隆画布 SVG 就是最终坐标的矢量图。去掉网格
/// 背景、按图元边界（含旋转外接盒）+ 边距裁剪、加白底。
function buildExportSvg() {
  const svg0 = currentGraph.container.querySelector('svg');
  if (!svg0) throw new Error('画布 SVG 不存在');
  const model = currentGraph.getModel();
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  const walk = (c) => {
    if (c && c.id && c.id !== '0' && c.id !== '1') {
      const st = currentGraph.view.getState(c);
      if (st && st.width > 0 && st.height > 0) {
        const bb = model.isEdge(c)
          ? { x: st.x, y: st.y, w: st.width, h: st.height }
          : stateVisualBbox(st, styleRotation(model.getStyle(c)));
        minX = Math.min(minX, bb.x); maxX = Math.max(maxX, bb.x + bb.w);
        minY = Math.min(minY, bb.y); maxY = Math.max(maxY, bb.y + bb.h);
      }
    }
    if (c) for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
  };
  walk(model.getRoot());
  if (minX === Infinity) throw new Error('图中没有可导出的元素');
  const border = 8;
  const W = Math.ceil(maxX - minX + 2 * border);
  const H = Math.ceil(maxY - minY + 2 * border);
  const clone = svg0.cloneNode(true);
  clone.removeAttribute('style'); // 去网格背景与尺寸样式
  clone.setAttribute('xmlns', 'http://www.w3.org/2000/svg');
  clone.setAttribute('width', W);
  clone.setAttribute('height', H);
  clone.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
  const wrap = document.createElementNS('http://www.w3.org/2000/svg', 'g');
  wrap.setAttribute('transform', 'translate(' + (border - minX) + ' ' + (border - minY) + ')');
  while (clone.firstChild) wrap.appendChild(clone.firstChild);
  clone.appendChild(wrap);
  const bg = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
  bg.setAttribute('width', '100%');
  bg.setAttribute('height', '100%');
  bg.setAttribute('fill', '#ffffff');
  clone.insertBefore(bg, wrap);
  return new XMLSerializer().serializeToString(clone);
}
$('export-svg').onclick = () => {
  if (!currentGraph) return;
  let svg;
  try {
    svg = buildExportSvg();
  } catch (err) {
    log('error', 'SVG 导出失败: ' + (err && err.message ? err.message : err));
    return;
  }
  downloadBlob(new Blob([svg], { type: 'image/svg+xml' }), exportStem() + '.svg');
};
$('export-xml').onclick = async () => {
  const r = await fetch('/api/file');
  if (!r.ok) { log('error', '导出失败: 无当前会话文件'); return; }
  downloadBlob(new Blob([await r.text()], { type: 'application/xml' }), exportStem() + '.drawio');
};

$('mode-select').onclick = () => { canvasMode = 'select'; applyMode(); };
$('mode-pan').onclick = () => { canvasMode = 'pan'; applyMode(); };
$('mode-zoom-in').onclick = () => {
  if (!currentGraph) return;
  const rect = currentGraph.container.getBoundingClientRect();
  zoomAt(rect.left + rect.width / 2, rect.top + rect.height / 2, 1.2);
};
$('mode-zoom-out').onclick = () => {
  if (!currentGraph) return;
  const rect = currentGraph.container.getBoundingClientRect();
  zoomAt(rect.left + rect.width / 2, rect.top + rect.height / 2, 1 / 1.2);
};
/// 复位视图：基于模型几何计算 fit（fork 的 getGraphBounds 与 scale 相关，
/// 从非 1 倍率复位会得到错误 translate，图被摆到画布外）
function fitView() {
  const model = currentGraph.getModel();
  let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
  const walk = (c) => {
    if (c.geometry && !model.isEdge(c)) {
      x0 = Math.min(x0, c.geometry.x);
      y0 = Math.min(y0, c.geometry.y);
      x1 = Math.max(x1, c.geometry.x + (c.geometry.width || 0));
      y1 = Math.max(y1, c.geometry.y + (c.geometry.height || 0));
    }
    for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
  };
  walk(model.getRoot());
  if (x0 === Infinity) { x0 = 0; y0 = 0; x1 = 0; y1 = 0; }
  currentGraph.view.scaleAndTranslate(1, 24 - x0, 24 - y0);
}
$('mode-zoom-100').onclick = () => {
  if (!currentGraph) return;
  fitView();
};

/// 点内 bbox 命中检测：bundle 的 getCellAt 会漏掉白填充 cell 与嵌套组。
/// 自顶向下遍历（后绘制者优先），按渲染态边界判定。
function getCellAtBbox(graph, x, y) {
  const model = graph.getModel();
  const out = [];
  const walk = (c) => {
    if (!c) return;
    out.push(c);
    for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
  };
  for (let i = 0; i < model.getChildCount(model.getRoot()); i++) walk(model.getChildAt(model.getRoot(), i));
  for (let i = out.length - 1; i >= 0; i--) {
    const c = out[i];
    if (!c || !c.id || c.id === '0' || c.id === '1') continue;
    const st = graph.view.getState(c);
    if (!st || typeof st.x !== 'number' || typeof st.width !== 'number') continue;
    if (x >= st.x && x <= st.x + st.width && y >= st.y && y <= st.y + st.height) return c;
  }
  return null;
}

// 模式感知的交互：pan = 纯导航；select = 点选 cell / 空白处拉框。
// 容器级 pointerdown（旧实现的 e.target!==canvas 检查永远命中 SVG，框选
// 因此从不启动）。
/// client 坐标 → 命中检测坐标。
/// 重要：这个 fork 把 view.translate 直接烤进 state（state.x = 模型坐标
/// + translate = 容器坐标），getCellAt/getCellAtBbox/hitTestCells 全部
/// 用容器坐标对 state 比较——因此这里**不能**减 translate（那是经典
/// mxGraph 的公式；照搬会造成点击偏移 translate 的量，花朵这类
/// x≈340 的图在加载时 translate≈-316，直接偏出 300+px）。
/// fork 的 scaleAndTranslate 在 scale/translate 同值时**跳过** viewStateChanged
/// （不 revalidate）——这与经典 mxGraph 不同。我们直接改 geometry/style 后
/// 必须显式 revalidate，否则 state/DOM 保持陈旧（拖动无视觉、手柄错位）。
function revalidateView() {
  if (!currentGraph) return;
  currentGraph.view.revalidate();
  applyRotations();
  refreshHandles();
}

/// 从 style 字符串读取 rotation（无则 0）
function styleRotation(style) {
  const m = String(style || '').match(/(?:^|;)rotation=(-?\d+(?:\.\d+)?)/);
  return m ? parseFloat(m[1]) : 0;
}

/// state 的视觉外接盒（含旋转；未旋转时即 state 矩形本身）
function stateVisualBbox(st, deg) {
  if (!deg) return { x: st.x, y: st.y, w: st.width, h: st.height };
  const cx = st.x + st.width / 2, cy = st.y + st.height / 2;
  const rad = deg * Math.PI / 180, cos = Math.cos(rad), sin = Math.sin(rad);
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const [px, py] of [[st.x, st.y], [st.x + st.width, st.y],
                          [st.x, st.y + st.height], [st.x + st.width, st.y + st.height]]) {
    const dx = px - cx, dy = py - cy;
    const rx = cx + dx * cos - dy * sin, ry = cy + dx * sin + dy * cos;
    minX = Math.min(minX, rx); maxX = Math.max(maxX, rx);
    minY = Math.min(minY, ry); maxY = Math.max(maxY, ry);
  }
  return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
}

/// fork 的渲染器不画旋转（canvas.rotate 存在但无调用方）——自己在 cell 的
/// shape 节点上附加 rotate transform。节点可能被 revalidate 重建/重置，
/// 故在每次 revalidate 之后调用。rotate 中心 = state 中心（容器坐标，
/// 节点 transform 仅 translate(0.5,0.5)，同一坐标系）。
function applyRotations() {
  if (!currentGraph) return;
  const model = currentGraph.getModel();
  const walk = (c) => {
    if (c && model.isVertex(c)) {
      const st = currentGraph.view.getState(c);
      if (st && st.shape && st.shape.node) {
        const deg = styleRotation(model.getStyle(c));
        let t = st.shape.node.getAttribute('transform') || '';
        t = t.replace(/\s*rotate\([^)]*\)\s*$/, '');
        if (deg) {
          const cx = st.x + st.width / 2, cy = st.y + st.height / 2;
          t = (t ? t + ' ' : '') + 'rotate(' + deg + ' ' + cx + ' ' + cy + ')';
        }
        st.shape.node.setAttribute('transform', t || 'translate(0.5,0.5)');
      }
    }
    if (c) for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
  };
  walk(model.getRoot());
}

function clientToGraph(clientX, clientY) {
  const rect = currentGraph.container.getBoundingClientRect();
  // 本 fork 的 state = (模型 + translate) × scale = 容器像素，
  // getCellAt/hitTest 都在这个空间比较——client 减容器原点即可，不除 scale。
  return {
    x: clientX - rect.left,
    y: clientY - rect.top,
  };
}

// 条件式 mousedown 抑制：mxGraph 的输入路径是 mousedown（bundle 注册
// 的是 mouse 事件），而 WebKit 不会因 pointerdown preventDefault 抑制
// 兼容 mousedown。我们消费的事件（shift 切换/拖动/框选/平移）必须阻断
// mxGraph 的 mousedown，否则它会再处理一次（替换选择/抵消 toggle）。
// 注册于页面加载、早于 mxGraph 的监听，目标阶段先执行。
canvasEl.addEventListener('mousedown', (e) => {
  if (!window.__pdHandled) return;
  e.stopPropagation();
  e.preventDefault();
}, true);

canvasEl.addEventListener('pointerdown', (e) => {
  if (e.button !== 0 || !currentGraph || busy) return;
  window.__pdHandled = false;
  setTimeout(() => { window.__pdHandled = false; }, 0);
  // 文字编辑器等 HTML 输入：完全放行（不启动平移/框选/拖动）
  const t = e.target;
  if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;

  if (canvasMode === 'pan') {
    // 拖拽设计（v2）：拖拽期间完全不碰 mxGraph —— 把容器子节点包进
    // wrapper，只用 CSS transform 移动整层（GPU 合成、瞬时、跨浏览器
    // 一致）。松开时一次性提交 translate（单次 revalidate），状态与
    // SVG 同步落位。不存在"状态先走、视觉后追"的错位窗口。
    const container = currentGraph.container;
    const wrapper = document.createElement('div');
    wrapper.style.cssText = 'position:absolute;left:0;top:0;width:100%;height:100%;will-change:transform;';
    while (container.firstChild) wrapper.appendChild(container.firstChild);
    container.appendChild(wrapper);
    const v = currentGraph.view;
    const startX = e.clientX, startY = e.clientY;
    let dx = 0, dy = 0;
    const onMove = (ev) => {
      dx = (ev.clientX - startX) / v.scale;
      dy = (ev.clientY - startY) / v.scale;
      wrapper.style.transform = 'translate(' + dx + 'px,' + dy + 'px)';
    };
    const onUp = () => {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      // 提交：unwrap + 单次 scaleAndTranslate
      wrapper.style.transform = '';
      while (wrapper.firstChild) container.appendChild(wrapper.firstChild);
      wrapper.remove();
      if (dx !== 0 || dy !== 0) {
        v.scaleAndTranslate(v.scale, v.translate.x + dx, v.translate.y + dy);
      }
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    e.preventDefault(); // 同时抑制 mxGraph 的 mouse 兼容事件
    return;
  }

  const p = clientToGraph(e.clientX, e.clientY);
  let cell = currentGraph.getCellAt(p.x, p.y);
  if (!cell) cell = getCellAtBbox(currentGraph, p.x, p.y);
  // select 模式：cell 交互
  if (cell) {
    if (e.shiftKey) {
      // fork 的 viewer bundle 没有 shift-toggle：自行实现切换，
      // 并阻断 mxGraph 的 mousedown 防其替换选择
      const sel = currentGraph.getSelectionCells();
      if (sel.some((c) => c === cell)) currentGraph.removeSelectionCell(cell);
      else currentGraph.addSelectionCell(cell);
      window.__pdHandled = true;
      e.preventDefault();
      return;
    }
    // 普通按下：选中（未选时）并开始我们自己的拖动
    const sel = currentGraph.getSelectionCells();
    if (!sel.some((c) => c === cell)) currentGraph.setSelectionCell(cell);
    beginCellDrag(e);
    window.__pdHandled = true;
    e.preventDefault();
    return;
  }
  // 空白处 → 开始框选（Shift 拉框 = 追加到现有选择）
  window.__pdHandled = true;
  rubberBand = { x1: e.clientX, y1: e.clientY, x2: e.clientX, y2: e.clientY, additive: !!e.shiftKey };
  rubberBandEl = document.createElement('div');
  rubberBandEl.className = 'rubber-band';
  canvasEl.appendChild(rubberBandEl);
  paintRubberBand();
  e.preventDefault();
  const onMove = (ev) => {
    if (!rubberBand) return;
    rubberBand.x2 = ev.clientX; rubberBand.y2 = ev.clientY;
    paintRubberBand();
  };
  const onUp = () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    if (!rubberBand || !currentGraph) { cancelRubberBand(); return; }
    const rect = rubberBand;
    const p1 = clientToGraph(rect.x1, rect.y1);
    const p2 = clientToGraph(rect.x2, rect.y2);
    const g = {
      x: Math.min(p1.x, p2.x), y: Math.min(p1.y, p2.y),
      width: Math.abs(p2.x - p1.x), height: Math.abs(p2.y - p1.y),
    };
    const cells = hitTestCells(g);
    const additive = rect.additive;
    cancelRubberBand();
    try {
      if (additive) currentGraph.addSelectionCells(cells);
      else currentGraph.setSelectionCells(cells);
    } catch (err) { console.warn(err); }
    if (cells.length) setSelection(currentGraph.getSelectionCells().filter((c) => c.id && c.id !== '0' && c.id !== '1').map((c) => c.id));
  };
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
});

function paintRubberBand() {
  if (!rubberBandEl || !rubberBand) return;
  const r = rubberBand;
  // overlay 定位用容器坐标（不带 translate），client 减容器原点即可
  const rect = currentGraph.container.getBoundingClientRect();
  const left = Math.min(r.x1, r.x2) - rect.left;
  const top = Math.min(r.y1, r.y2) - rect.top;
  const w = Math.abs(r.x2 - r.x1), h = Math.abs(r.y2 - r.y1);
  rubberBandEl.style.left = left + 'px';
  rubberBandEl.style.top = top + 'px';
  rubberBandEl.style.width = w + 'px';
  rubberBandEl.style.height = h + 'px';
}
function cancelRubberBand() {
  if (rubberBandEl) { rubberBandEl.remove(); rubberBandEl = null; }
  rubberBand = null;
}

/// Cells whose rendered bounds intersect the graph rect (topmost last =
/// keep model order; selection order does not matter to the backend).
function hitTestCells(rect) {
  const model = currentGraph.getModel();
  const out = [];
  const walk = (cell) => {
    if (cell && cell.id !== '0' && cell.id !== '1') {
      const st = currentGraph.getView().getState(cell);
      if (st && st.width > 0 && st.height > 0) {
        if (!(st.x + st.width < rect.x || rect.x + rect.width < st.x ||
              st.y + st.height < rect.y || rect.y + rect.height < st.y)) {
          out.push(cell);
        }
      }
    }
    if (cell) for (let i = 0; i < model.getChildCount(cell); i++) walk(model.getChildAt(cell, i));
  };
  for (let i = 0; i < model.getChildCount(model.getRoot()); i++) {
    walk(model.getChildAt(model.getRoot(), i));
  }
  return out;
}

function setSelection(ids) {
  selectedIds = ids;
  const chip = $('sel-chip');
  if (!ids.length) { chip.hidden = true; $('sel-note').textContent = ''; return; }
  chip.hidden = false;
  chip.textContent = `已选中 ${ids.length} 个 cell：${ids.join(', ')} —— 将随下一条消息附带`;
  $('sel-note').textContent = `（附带 ${ids.length} 个 cell 引用）`;
}

// ---------------------------------------------------------------------------
// mini editor：拖动 cell / 缩放 / 手动改动同步
// ---------------------------------------------------------------------------

/// 拖动选中 cell（含其子元素整体同位移；edge 跟随端点，不动其几何）
function beginCellDrag(e) {
  const v = currentGraph.view;
  const startX = e.clientX, startY = e.clientY;
  let moved = 0;
  const model = currentGraph.getModel();
  const collect = () => {
    const sel = currentGraph.getSelectionCells().filter((c) => c && !model.isEdge(c));
    const set = [];
    const seen = new Set();
    const walk = (c) => {
      if (!c || seen.has(c)) return;
      seen.add(c);
      set.push(c);
      for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
    };
    for (const c of sel) walk(c);
    return set.filter((c) => c.geometry);
  };
  let lastX = startX, lastY = startY;
  const onMove = (ev) => {
    const ddx = (ev.clientX - lastX) / v.scale;
    const ddy = (ev.clientY - lastY) / v.scale;
    lastX = ev.clientX; lastY = ev.clientY;
    if (ddx === 0 && ddy === 0) return;
    moved += Math.abs(ddx) + Math.abs(ddy);
    for (const c of collect()) {
      c.geometry.x += ddx;
      c.geometry.y += ddy;
    }
    revalidateView();
  };
  const onUp = () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    if (moved > 2) markDirty();
  };
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
}

// ---------------------------------------------------------------------------
// 自绘 resize / rotate 手柄（fork 自带手柄不可用，视觉可见但无拖动逻辑）
// ---------------------------------------------------------------------------

/// 重绘手柄层：仅当 select 模式、非 busy、恰好选中 1 个叶子 cell。
/// 手柄位置用 state（=容器像素）直接定位；所有视图变化（缩放/平移/
/// 拖动/revalidate）都经 scaleAndTranslate 钩子触发本函数跟随。
function refreshHandles() {
  if (handleLayer) handleLayer.innerHTML = '';
  handleCell = null;
  if (!currentGraph || busy || canvasMode !== 'select') return;
  const model = currentGraph.getModel();
  const sel = currentGraph.getSelectionCells().filter(
    (c) => c && c.id && c.id !== '0' && c.id !== '1' && !model.isEdge(c) &&
      model.getChildCount(c) === 0 && c.geometry
  );
  if (sel.length !== 1) return;
  const cell = sel[0];
  const st = currentGraph.view.getState(cell);
  if (!st) return;
  handleCell = cell;
  // 旋转过的 cell：手柄跟随视觉 bbox（fork 的 state 不计算旋转外接盒）
  const bb = stateVisualBbox(st, styleRotation(model.getStyle(cell)));
  const bx = bb.x, by = bb.y, bw = bb.w, bh = bb.h;
  const CURSORS = { nw:'nwse-resize', se:'nwse-resize', ne:'nesw-resize', sw:'nesw-resize',
                    n:'ns-resize', s:'ns-resize', e:'ew-resize', w:'ew-resize' };
  const mk = (cls, cx, cy, dir) => {
    const el = document.createElement('div');
    el.className = 'mx-handle ' + cls;
    el.dataset.dir = dir;
    el.style.left = cx + 'px';
    el.style.top = cy + 'px';
    if (CURSORS[dir]) el.style.cursor = CURSORS[dir];
    handleLayer.appendChild(el);
    return el;
  };
  const x = bx, y = by, w = bw, h = bh;
  mk('mx-handle-resize', x - 4, y - 4, 'nw');
  mk('mx-handle-resize', x + w / 2 - 4, y - 4, 'n');
  mk('mx-handle-resize', x + w - 4, y - 4, 'ne');
  mk('mx-handle-resize', x + w - 4, y + h / 2 - 4, 'e');
  mk('mx-handle-resize', x + w - 4, y + h - 4, 'se');
  mk('mx-handle-resize', x + w / 2 - 4, y + h - 4, 's');
  mk('mx-handle-resize', x - 4, y + h - 4, 'sw');
  mk('mx-handle-resize', x - 4, y + h / 2 - 4, 'w');
  // 旋转手柄：顶边中点上方 26px（手柄中心定位）；顶边贴近画布上缘时
  // 翻到底边中点下方，连线方向同步翻转
  let ry = y - 26 - 7;
  let flip = false;
  if (ry < 4) { ry = y + h + 26 - 7; flip = true; }
  mk('mx-handle-rotate' + (flip ? ' mx-handle-rotate-flip' : ''), x + w / 2 - 7, ry, 'rotate');
}

/// 拖 resize 手柄：方向编码 n/e/s/w。数学全程在 state（容器像素）空间
/// 做约束，最后一次性换算回模型几何（÷scale − translate）。
function beginResizeDrag(e, cell, dir) {
  const v = currentGraph.view;
  const st0 = v.getState(cell);
  const x0 = st0.x, y0 = st0.y, w0 = st0.width, h0 = st0.height;
  const startX = e.clientX, startY = e.clientY;
  const MIN = 20; // 状态空间最小 20px
  let moved = false;
  const onMove = (ev) => {
    const dx = ev.clientX - startX, dy = ev.clientY - startY;
    let x = x0, y = y0, w = w0, h = h0;
    if (dir.includes('e')) w = w0 + dx;
    if (dir.includes('s')) h = h0 + dy;
    if (dir.includes('w')) { x = x0 + dx; w = w0 - dx; }
    if (dir.includes('n')) { y = y0 + dy; h = h0 - dy; }
    if (w < MIN) { if (dir.includes('w')) x = x0 + w0 - MIN; w = MIN; }
    if (h < MIN) { if (dir.includes('n')) y = y0 + h0 - MIN; h = MIN; }
    if (x !== x0 || y !== y0 || w !== w0 || h !== h0) moved = true;
    cell.geometry.x = x / v.scale - v.translate.x;
    cell.geometry.y = y / v.scale - v.translate.y;
    cell.geometry.width = w / v.scale;
    cell.geometry.height = h / v.scale;
    revalidateView();
  };
  const onUp = () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    if (moved) markDirty();
  };
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
}

/// style 字符串上更新 rotation= 项（其余项保持，顺序无关）
function styleSetRotation(style, deg) {
  const parts = String(style || '')
    .split(';').map((s) => s.trim())
    .filter((s) => s && !/^rotation=/.test(s));
  parts.push('rotation=' + deg);
  return parts.join(';');
}

/// 拖旋转手柄：绕 cell 中心（state bbox 中心 = 几何中心，旋转下不变）。
/// 角度 = atan2（指针向右=90°，上方=0°，顺时针）。Shift 吸附 15°。
function beginRotateDrag(e, cell) {
  const v = currentGraph.view;
  const model = currentGraph.getModel();
  const st = v.getState(cell);
  const cx = st.x + st.width / 2, cy = st.y + st.height / 2;
  const rect = currentGraph.container.getBoundingClientRect();
  let moved = false;
  const onMove = (ev) => {
    const px = ev.clientX - rect.left, py = ev.clientY - rect.top;
    let deg = Math.atan2(px - cx, cy - py) * 180 / Math.PI;
    deg = ((deg % 360) + 360) % 360;
    deg = ev.shiftKey ? Math.round(deg / 15) * 15 % 360 : Math.round(deg);
    model.setStyle(cell, styleSetRotation(model.getStyle(cell), deg));
    moved = true;
    revalidateView();
  };
  const onUp = () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    if (moved) markDirty();
  };
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
}

/// 自实现文字编辑：双击 cell → 覆盖 input（Enter/失焦提交，Esc 取消）
function startTextEdit(cell) {
  const st = currentGraph.view.getState(cell);
  if (!st) return;
  const input = document.createElement('input');
  input.className = 'mini-editor';
  input.value = cell.value || '';
  const rect = currentGraph.container.getBoundingClientRect();
  input.style.left = (st.x + 2) + 'px';
  input.style.top = (st.y + st.height / 2 - 10) + 'px';
  input.style.width = Math.max(60, st.width - 4) + 'px';
  canvasEl.appendChild(input);
  input.focus();
  input.select();
  if (handleLayer) handleLayer.innerHTML = '';
  let done = false; // Enter 提交后 blur 会再触发一次 commit
  const commit = () => {
    if (done) return;
    done = true;
    const v = input.value;
    input.remove();
    window.removeEventListener('pointerdown', outside);
    if (v !== (cell.value || '')) {
      currentGraph.getModel().setValue(cell, v);
      revalidateView(); // 重绘标签
      markDirty();
    }
    refreshHandles();
  };
  const cancel = () => {
    if (done) return;
    done = true;
    input.remove();
    window.removeEventListener('pointerdown', outside);
    refreshHandles();
  };
  const outside = (ev) => {
    if (ev.target !== input) commit();
  };
  input.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter') { ev.preventDefault(); commit(); }
    else if (ev.key === 'Escape') { ev.preventDefault(); cancel(); }
  });
  input.addEventListener('blur', () => commit());
  setTimeout(() => window.addEventListener('pointerdown', outside), 0);
}
canvasEl.addEventListener('dblclick', (e) => {
  if (canvasMode !== 'select' || !currentGraph || busy) return;
  const p = clientToGraph(e.clientX, e.clientY);
  let cell = currentGraph.getCellAt(p.x, p.y);
  if (!cell) cell = getCellAtBbox(currentGraph, p.x, p.y);
  if (!cell) return;
  e.stopPropagation();
  e.preventDefault();
  startTextEdit(cell);
}, true);

// ctrl/cmd + 滚轮缩放（围绕光标）；画布按钮缩放围绕中心
function zoomAt(clientX, clientY, factor) {
  const v = currentGraph.view;
  const ns = Math.min(4, Math.max(0.25, v.scale * factor));
  if (ns === v.scale) return;
  const rect = currentGraph.container.getBoundingClientRect();
  const cx = clientX - rect.left, cy = clientY - rect.top;
  v.scaleAndTranslate(
    ns,
    v.translate.x + cx / v.scale - cx / ns,
    v.translate.y + cy / v.scale - cy / ns
  );
}
canvasEl.addEventListener('wheel', (e) => {
  if (!currentGraph || busy || !(e.ctrlKey || e.metaKey)) return;
  e.preventDefault();
  zoomAt(e.clientX, e.clientY, Math.pow(1.1, -Math.sign(e.deltaY)));
}, { passive: false });

// ---- 手动改动 → 服务端同步（防抖批量） ----
let currentXml = '';
let dirty = false;
let syncTimer = 0;
function markDirty() {
  dirty = true;
  clearTimeout(syncTimer);
  syncTimer = setTimeout(syncNow, 600);
}
function buildCurrentMxfile() {
  const codec = new mxCodec();
  const node = codec.encode(currentGraph.getModel());
  let modelXml;
  try {
    if (typeof XMLSerializer !== 'undefined') modelXml = new XMLSerializer().serializeToString(node);
    else modelXml = mxUtils.getXml(node);
  } catch (err) {
    modelXml = mxUtils.getXml(node);
  }
  if (!currentXml) return null;
  const i = currentXml.indexOf('<mxGraphModel');
  const j = currentXml.lastIndexOf('</mxGraphModel>');
  if (i < 0 || j < 0) return null;
  return currentXml.slice(0, i) + modelXml + currentXml.slice(j + '></mxGraphModel>'.length);
}
async function syncNow() {
  if (!dirty) return;
  if (busy) {
    // 任务运行中：稍后重试
    syncTimer = setTimeout(syncNow, 2000);
    return;
  }
  const xml = buildCurrentMxfile();
  if (!xml) return;
  dirty = false;
  try {
    const r = await api('/api/manual', { xml });
    if (r.ok) {
      currentXml = r.xml;
      const st = await (await fetch('/api/state')).json();
      $('cells').textContent = `${st.cells} 个元素 / ${st.lines} 行`;
      if (!currentGraph) return;
      // 服务端 canonicalize 可能与本地形态一致；若不一致（理论上不会）
      // 以服务端为准刷新
    } else {
      log('error', '手动改动同步失败: ' + (r.error || ''));
      dirty = true;
      // 回退到服务端版本
      await refreshCanvas();
    }
  } catch (err) {
    dirty = true;
  }
}

// ---------------------------------------------------------------------------
// Sessions (会话 = 一个 .drawio 文件)
// ---------------------------------------------------------------------------

const picker = $('session-picker');

function setPlaceholder(mode) {
  placeholderEl.hidden = false;
  $('placeholder-msg').textContent = mode === 'loading' ? '加载中…' : '';
  $('welcome').hidden = mode !== 'welcome';
  canvasEl.style.display = mode === 'canvas' ? '' : 'none';
}

async function loadSessions() {
  const data = await (await fetch('/api/sessions')).json();
  picker.innerHTML = '';
  for (const s of data.sessions || []) {
    const o = document.createElement('option');
    o.value = s.name;
    o.textContent = s.name + (s.current ? '（当前）' : '');
    o.selected = !!s.current;
    picker.appendChild(o);
  }
  return data;
}

async function switchSession(name, announce) {
  if (busy) { log('error', '任务运行中——先停止再切换会话'); return false; }
  const r = await api('/api/sessions/switch', { name });
  if (!r.ok) { log('error', '切换失败: ' + (r.error || '')); return false; }
  await refreshCanvas();
  const st = await (await fetch('/api/state')).json();
  $('cells').textContent = st.cells != null ? `${st.cells} 个元素 / ${st.lines} 行` : '–';
  if (st.session) renderUsage(st.session);
  $('chatlog').innerHTML = '';
  if (announce) log('tool-note', `已进入会话（文件）: ${name}`);
  await loadSessions();
  await renderHistoryIntoChat();
  return true;
}

async function createSession() {
  if (guardBusy()) return;
  const name = window.prompt('新会话名称（留空自动命名；会话 = 新建 .drawio 文件）', '');
  if (name === null) return;
  const r = await api('/api/sessions', { name });
  if (!r.ok) { log('error', '创建失败: ' + (r.error || '')); return; }
  await switchSession(r.name, true);
  log('ok', `✓ 新会话 ${r.name} 已创建（${r.note || ''}）`);
}

$('session-new').onclick = createSession;
$('welcome-new').onclick = createSession;
picker.onchange = () => switchSession(picker.value, true);
$('session-del').onclick = async () => {
  if (guardBusy()) return;
  const name = picker.value;
  if (!name) return;
  if (!window.confirm(`删除会话（文件与历史）？\n${name}\n此操作不可撤销。`)) return;
  const r = await api('/api/sessions/' + encodeURIComponent(name), {}, 'DELETE');
  if (!r.ok) { log('error', '删除失败: ' + (r.error || '')); return; }
  log('tool-note', r.note || '已删除');
  await refreshSessionView();
};

// 清空当前会话视图回到列表态
async function refreshSessionView() {
  const data = await loadSessions();
  const list = data.sessions || [];
  if (list.length) {
    await switchSession(picker.value || list[0].name, false);
  } else {
    $('chatlog').innerHTML = '';
    setPlaceholder('welcome');
    $('usage').textContent = '–';
  }
}

// ---------------------------------------------------------------------------
// Data + chat
// ---------------------------------------------------------------------------

function log(kind, text) {
  const div = document.createElement('div');
  div.className = 'msg ' + kind;
  div.textContent = text;
  $('chatlog').appendChild(div);
  $('chatlog').scrollTop = $('chatlog').scrollHeight;
}

let lastView = null;
function rememberView() {
  if (!currentGraph) return;
  const v = currentGraph.view;
  lastView = { x: v.translate.x, y: v.translate.y, s: v.scale };
}
function restoreView() {
  if (lastView && currentGraph) {
    const v = currentGraph.view;
    v.translate.x = lastView.x;
    v.translate.y = lastView.y;
    v.scale = lastView.s;
    currentGraph.refresh();
  }
  lastView = null;
}

async function refreshCanvas(keepView) {
  if (keepView) rememberView();
  let resp;
  try {
    resp = await fetch('/api/file');
  } catch (e) {
    throw e;
  }
  if (resp.status === 404) {
    setPlaceholder('welcome');
    return;
  }
  const xml = await resp.text();
  currentXml = xml;
  hidePlaceholder();
  canvasEl.style.display = '';
  loadXmlIntoCanvas(xml);
  if (keepView) restoreView();
  setSelection([]);
}

async function api(path, body, method) {
  let resp;
  try {
    resp = await fetch(path, {
      method: method || 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body || {}),
    });
  } catch (e) {
    return { ok: false, error: '请求失败: ' + e.message };
  }
  const text = await resp.text();
  let data = null;
  if (text) {
    try { data = JSON.parse(text); }
    catch (e) { data = null; }
  }
  if (!resp.ok) {
    return { ok: false, error: 'HTTP ' + resp.status + ': ' + (data && data.error ? data.error : (text.slice(0, 200) || resp.statusText)) };
  }
  return data || {};
}

async function loadState() {
  const st = await (await fetch('/api/state')).json();
  if (st.build) {
    const b = $('build');
    if (b) b.textContent = st.build;
    console.log('[drawio-harness] build:', st.build);
  }
  const cells = $('cells');
  if (cells) cells.textContent = st.cells != null ? `${st.cells} 个元素 / ${st.lines} 行` : '–';
  $('llm').textContent = st.llm_ready ? 'LLM ✓' : 'LLM ✗';
  $('llm-banner').hidden = st.llm_ready;
  if (st.session) renderUsage(st.session);
  return st;
}

$('check').onclick = async () => { if (guardBusy()) return;
  const r = await api('/api/check');
  if (r.ok) log('ok', `✓ 检查通过（cells=${r.cells} edges=${r.edges}）`);
  else {
    log('error', '✗ 检查发现 ' + (r.issues || []).length + ' 个问题：\n' + (r.issues || []).join('\n'));
  }
};
$('undo').onclick = async () => { if (guardBusy()) return;
  const r = await api('/api/undo');
  if (r.ok) { log('tool-note', '↩ 已撤销，画布已回滚'); await refreshCanvas(); }
  else log('error', '撤销失败：' + (r.error || ''));
};
$('reload').onclick = async () => { if (guardBusy()) return;
  const r = await api('/api/reload');
  log('tool-note', `已从磁盘重新加载（${r.cells || 0} 个元素）`);
  await refreshCanvas();
};

// (busy 已在顶部声明)

async function cancelJob() {
  const r = await api('/api/chat/cancel', {});
  if (r.ok) log('tool-note', '⏹ 已发送停止信号，等待中断…');
  else if (r.error && !String(r.error).includes('没有运行中')) log('error', '停止失败: ' + r.error);
}

async function handleStreamEvent(ev) {
  switch (ev.type) {
    case 'turn':
      log('tool-note', `— 模型轮次 ${ev.index + 1} …`);
      break;
    case 'tool':
      log('tool-note', `→ ${ev.name} ${ev.args || ''}`);
      break;
    case 'tool_result': {
      log('tool-note', `↳ ${ev.name}: ${ev.preview || ''}${ev.has_image ? ' 📷' : ''}`);
      // 动态更新画布：改动类工具落盘后立即重绘，而不是等整轮结束
      if (ev.name === 'edit' || ev.name === 'draw' || ev.name === 'undo') {
        await refreshCanvas(true);
      }
      break;
    }
    case 'usage': {
      const c = ev.cost_yuan > 0 ? ' · ' + fmtCost(ev.cost_yuan) : '';
      log('tool-note', `  本轮用量 ${ev.in} in / ${ev.out} out tokens${c}`);
      break;
    }
    case 'reply':
      log('assistant', ev.reply);
      break;
    case 'done':
      log('tool-note', `（本轮工具调用 ${ev.tool_calls} 次）`);
      if (ev.session) renderUsage(ev.session);
      break;
    case 'error':
      log('error', '对话出错：' + ev.error);
      break;
    default:
      break;
  }
}

$('chatform').onsubmit = async (ev) => {
  ev.preventDefault();
  if (busy) { await cancelJob(); return; }
  const text = $('input').value.trim();
  if (!text) return;
  if (!picker.value) { log('error', '先创建一个会话（＋ 新建会话）'); return; }
  busy = true;
  refreshHandles();
  $('send').disabled = false;
  $('send').textContent = '停止';
  $('send').classList.add('danger');
  const ids = selectedIds.slice();
  log('user', text);
  $('input').value = '';
  setSelection([]);
  try {
    const resp = await fetch('/api/chat/stream', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text, cell_ids: ids }),
    });
    if (!resp.ok || !resp.body) {
      const t = await resp.text();
      log('error', '请求失败 HTTP ' + resp.status + ': ' + t.slice(0, 300));
      return;
    }
    const reader = resp.body.getReader();
    const dec = new TextDecoder();
    let buf = '';
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!line) continue;
        let ev2;
        try { ev2 = JSON.parse(line); } catch (e) { continue; }
        await handleStreamEvent(ev2);
      }
    }
  } catch (e) {
    if (!(e && e.name === 'AbortError') && !(e && e.message === 'Load failed')) {
      log('error', '网络错误: ' + (e && e.message ? e.message : e));
    }
  } finally {
    busy = false;
    refreshHandles();
    $('send').textContent = '发送';
    $('send').classList.remove('danger');
    // 页面刷新/关闭会中断这些 fetch——兜底吞掉，不再产生未处理 rejection
    try {
      await refreshCanvas();
      const st = await (await fetch('/api/state')).json();
      $('cells').textContent = `${st.cells} 个元素 / ${st.lines} 行`;
      if (st.session) renderUsage(st.session);
    } catch (e2) { /* page going away */ }
  }
};

// ---------------------------------------------------------------------------
// 诊断 HUD（?debug=1）：光标处坐标系统 + 命中 cell，用于定位偏移类问题
// ---------------------------------------------------------------------------

function setupDebugHud() {
  if (!location.search.includes('debug=1')) return;
  const hud = document.createElement('div');
  hud.id = 'debug-hud';
  hud.style.cssText = 'position:fixed;top:70px;right:360px;z-index:99;background:rgba(0,0,0,.85);color:#8f8;font:11px/1.5 monospace;padding:8px 10px;border-radius:8px;white-space:pre;';
  document.body.appendChild(hud);
  // 状态矩形叠加层：把每个 cell 的 state 画成彩色框叠在画布上。
  // 彩色框 = 系统认为 cell 所在的位置（命中检测也用它）。
  // 如果框与花瓣错位 → 渲染/状态分叉；如果框套着花瓣但点选还是错 → 坐标问题。
  const overlay = document.createElement('div');
  overlay.id = 'state-overlay';
  overlay.style.cssText = 'position:absolute;inset:0;pointer-events:none;z-index:30;';
  currentGraph ? currentGraph.container.appendChild(overlay) : null;
  const paintStateOverlay = () => {
    if (!currentGraph) return;
    if (!overlay.parentNode) currentGraph.container.appendChild(overlay);
    overlay.innerHTML = '';
    const model = currentGraph.getModel();
    const walk = (c) => {
      if (c.id && c.id !== '0' && c.id !== '1') {
        const st = currentGraph.view.getState(c);
        if (st && st.width > 0) {
          const d = document.createElement('div');
          d.style.cssText = 'position:absolute;left:' + st.x + 'px;top:' + st.y + 'px;width:' + st.width + 'px;height:' + st.height + 'px;border:1px dashed #0f0;color:#0f0;font:10px monospace;';
          d.textContent = c.id;
          overlay.appendChild(d);
        }
      }
      for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
    };
    walk(model.getRoot());
  };
  paintStateOverlay();
  // 平移/重绘后刷新叠加层
  window.__overlayPaint = paintStateOverlay;

  const origLoad = window.loadXmlIntoCanvas;
  window.loadXmlIntoCanvas = function (xml) {
    origLoad(xml);
    setTimeout(paintStateOverlay, 50);
  };
  document.addEventListener('pointerup', () => setTimeout(paintStateOverlay, 0));

  // cell 状态矩形 vs SVG 实际屏幕矩形（视觉/状态是否分叉的诊断核心）
  const cellRects = () => {
    if (!currentGraph) return '';
    const model = currentGraph.getModel();
    const cells = [];
    const walk = (c) => { if (c.id && c.id !== '0' && c.id !== '1' && c.geometry) cells.push(c); for (let i=0;i<model.getChildCount(c);i++) walk(model.getChildAt(c,i)); };
    walk(model.getRoot());
    const rows = [];
    for (const c of cells) {
      const st = currentGraph.view.getState(c);
      if (!st) continue;
      // 找该 cell 的 SVG 节点屏幕矩形
      const el = currentGraph.container.querySelector('[id*="' + c.id + '"]') || null;
      const dr = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
      rows.push(
        c.id + ': state(' + st.x.toFixed(0) + ',' + st.y.toFixed(0) + ' ' + st.width.toFixed(0) + 'x' + st.height.toFixed(0) + ')' +
        (dr ? ' dom(' + dr.left.toFixed(0) + ',' + dr.top.toFixed(0) + ' ' + dr.width.toFixed(0) + 'x' + dr.height.toFixed(0) + ')' : ' dom=none')
      );
    }
    return rows.join('\n');
  };
  const upd = (e) => {
    if (!currentGraph) return;
    const v = currentGraph.view;
    const rect = currentGraph.container.getBoundingClientRect();
    const gx = (e.clientX - rect.left) / v.scale - v.translate.x;
    const gy = (e.clientY - rect.top) / v.scale - v.translate.y;
    let hit = currentGraph.getCellAt(gx, gy);
    const bbox = !hit ? getCellAtBbox(currentGraph, gx, gy) : null;
    const sel = currentGraph.getSelectionCells().map((c) => c.id).join(',');
    hud.textContent = `client  (${e.clientX.toFixed(1)}, ${e.clientY.toFixed(1)})\ncontainer(${(e.clientX - rect.left).toFixed(1)}, ${(e.clientY - rect.top).toFixed(1)})\ngraph   (${gx.toFixed(1)}, ${gy.toFixed(1)})\ntranslate(${v.translate.x.toFixed(1)}, ${v.translate.y.toFixed(1)}) scale=${v.scale}\ngetCellAt=${hit ? hit.id : 'null'} bbox=${bbox ? bbox.id : 'null'}\nselection=[${sel}]\n--- cells ---\n${cellRects()}`;
  };
  // 点击日志：每次 pointerdown 记录命中
  document.addEventListener('pointerdown', (e) => {
    if (!currentGraph) return;
    const v = currentGraph.view;
    const rect = currentGraph.container.getBoundingClientRect();
    const gx = (e.clientX - rect.left) / v.scale - v.translate.x;
    const gy = (e.clientY - rect.top) / v.scale - v.translate.y;
    let hit = currentGraph.getCellAt(gx, gy);
    const bbox = !hit ? getCellAtBbox(currentGraph, gx, gy) : null;
    console.log('[click] graph(' + gx.toFixed(1) + ',' + gy.toFixed(1) + ') shift=' + e.shiftKey + ' getCellAt=' + (hit ? hit.id : 'null') + ' bbox=' + (bbox ? bbox.id : 'null'));
  }, true);
  currentGraph ? currentGraph.container.addEventListener('pointermove', upd) : null;
  document.addEventListener('pointermove', upd);
}
setupDebugHud();

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

(async () => {
  try {
    await loadState();
    const data = await loadSessions();
    const list = data.sessions || [];
    const cur = list.find((x) => x.current);
    if (cur) {
      await refreshCanvas();
      await renderHistoryIntoChat();
    } else if (list.length) {
      await switchSession(list[0].name, false);
    } else {
      setPlaceholder('welcome');
      $('usage').textContent = '–';
    }
  } catch (e) {
    setPlaceholder('welcome');
    $('placeholder-msg').textContent = '连接服务器失败：' + (e && e.message ? e.message : e);
  }
  $('input').focus();
})();

// ---------------------------------------------------------------------------
// Settings modal (LLM provider config + connection test)
// ---------------------------------------------------------------------------

const modal = $('settings-modal');
const form = $('settings-form');
const cfgBaseUrl = $('cfg-base-url');
const cfgApiKey = $('cfg-api-key');
const cfgModel = $('cfg-model');
const cfgFile = $('cfg-file');
const cfgBody = $('cfg-current-body');
const cfgHint = $('cfg-demo-hint');
const testResult = $('test-result');
const cfgCtxLen = $('cfg-context-length');
const cfgThinking = $('cfg-thinking');
const cfgPriceIn = $('cfg-price-in');
const cfgPriceOut = $('cfg-price-out');
const cfgBudget = $('cfg-budget');
const cfgMaxTurns = $('cfg-max-turns');

function fmtCost(v) { return '¥' + (v == null ? '?' : v.toFixed(4)); }

function renderUsage(session) {
  const chip = $('usage');
  if (!session) { chip.textContent = '–'; return; }
  const t = (session.in || 0) + (session.out || 0);
  const b = session.budget_yuan;
  const txt = `本次会话 ${t} tokens (in ${session.in || 0} / out ${session.out || 0}) · 花费 ${fmtCost(session.cost_yuan)}` +
    (b != null ? ` / 预算 ${'¥' + b.toFixed(2)}` : '');
  chip.textContent = txt;
  chip.classList.toggle('warn', b != null && session.cost_yuan >= b);
}

function setTestResult(kind, text) {
  testResult.className = 'test-result ' + kind;
  testResult.textContent = text;
  testResult.hidden = false;
}

function renderCurrentCfg(cfg) {
  const llm = cfg.llm || { kind: 'unconfigured', base_url: '', model: '', api_key_masked: '' };
  cfgHint.hidden = !(llm.kind === 'unconfigured');
  const source = llm.kind === 'unconfigured' ? '未配置' : (cfg.source === 'file' ? '配置文件' : '环境变量');
  cfgFile.textContent = cfg.config_file || '未持久化';
  cfgBody.innerHTML = '';
  const rows = [
    ['来源', source],
    ['Base URL', llm.base_url || '—'],
    ['Model', llm.model || '—'],
    ['API Key', llm.api_key_masked || '—'],
    ['配置文件', cfg.config_file || '无'],
  ];
  for (const [k, v] of rows) {
    const dt = document.createElement('dt'); dt.textContent = k;
    const dd = document.createElement('dd'); dd.textContent = v;
    cfgBody.append(dt, dd);
  }
  cfgBaseUrl.value = llm.base_url || '';
  cfgModel.value = llm.model || '';
  cfgApiKey.value = '';
  cfgApiKey.placeholder = llm.api_key_masked ? `留空 = 保持不变 (${llm.api_key_masked})` : 'sk-…';
  cfgCtxLen.value = llm.context_length != null ? llm.context_length : '';
  cfgThinking.value = llm.thinking === 'no-think' ? 'no-think' : 'default';
  cfgPriceIn.value = (llm.price_input_per_m || 0);
  cfgPriceOut.value = (llm.price_output_per_m || 0);
  cfgBudget.value = llm.budget_yuan != null ? llm.budget_yuan : '';
  cfgMaxTurns.value = llm.max_turns || 24;
}

async function openSettings() {
  try {
    const cfg = await (await fetch('/api/config')).json();
    renderCurrentCfg(cfg);
    modal.hidden = false;
    testResult.hidden = true;
  } catch (e) { setTestResult('err', '读取配置失败: ' + e); }
}
function closeSettings() { modal.hidden = true; }

$('settings-btn').onclick = openSettings;
$('settings-close').onclick = closeSettings;
$('cfg-cancel-btn').onclick = closeSettings;
modal.addEventListener('click', (e) => { if (e.target === modal) closeSettings(); });
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !modal.hidden) closeSettings();
});

$('cfg-test-btn').onclick = async () => {
  const payload = {
    base_url: cfgBaseUrl.value.trim(),
    model: cfgModel.value.trim(),
    api_key: cfgApiKey.value.trim(),
  };
  if (!payload.base_url || !payload.model) {
    setTestResult('err', '先填写 Base URL 与 Model');
    return;
  }
  $('cfg-test-btn').disabled = true;
  setTestResult('ok', '连接中…');
  try {
    const r = await api('/api/config/test', payload);
    if (r.ok) setTestResult('ok', `✓ 连接成功（${r.ms}ms）· ${r.model}\n模型回复: ${r.reply}`);
    else setTestResult('err', '✗ 连接失败:\n' + (r.error || ''));
  } catch (e) { setTestResult('err', '网络错误: ' + e); }
  $('cfg-test-btn').disabled = false;
};

function configPayload() {
  const num = (v) => { const n = parseFloat(v); return Number.isFinite(n) ? n : 0; };
  const opt = (v) => { const n = parseFloat(v); return Number.isFinite(n) && v !== '' && n > 0 ? n : null; };
  return {
    base_url: cfgBaseUrl.value.trim(),
    model: cfgModel.value.trim(),
    api_key: cfgApiKey.value.trim(),
    context_length: opt(cfgCtxLen.value),
    thinking: cfgThinking.value === 'no-think' ? 'no-think' : 'default',
    price_input_per_m: num(cfgPriceIn.value),
    price_output_per_m: num(cfgPriceOut.value),
    budget_yuan: opt(cfgBudget.value),
    max_turns: parseInt(cfgMaxTurns.value, 10) || 24,
  };
}

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const payload = configPayload();
  const btn = $('cfg-save-btn');
  btn.disabled = true;
  try {
    const r = await api('/api/config', payload, 'PUT');
    if (r.ok) {
      const cfg = await (await fetch('/api/config')).json();
      renderCurrentCfg(cfg);
      setTestResult('ok', '✓ 已保存并生效');
      // refresh header chips / banner
      const st = await (await fetch('/api/state')).json();
      $('llm').textContent = st.llm_ready ? 'LLM ✓' : 'LLM ✗';
      $('llm-banner').hidden = st.llm_ready;
      closeSettings();
    } else {
      setTestResult('err', '保存失败: ' + (r.error || ''));
    }
  } catch (err) { setTestResult('err', '网络错误: ' + err); }
  btn.disabled = false;
});

// ---------------------------------------------------------------------------
// R5: 历史 = 聊天记录本身（无感重放，不再有单独面板）
// ---------------------------------------------------------------------------

/// 把会话历史按聊天流形式铺进 chatlog：user 气泡 → 工具轨迹 → 回复气泡。
/// 打开页面/切换会话时调用，看起来就像上次的对话一直在这里。
async function renderHistoryIntoChat() {
  if (busy) return;
  let data;
  try { data = await (await fetch('/api/history')).json(); }
  catch (e) { return; }
  const recs = (data.records || []).slice(0, 50).reverse(); // 旧→新
  if (!recs.length) return;
  const divider = document.createElement('div');
  divider.className = 'msg chat-divider';
  divider.textContent = '── 历史记录 ──';
  $('chatlog').appendChild(divider);
  for (const r of recs) {
    logMsg('user', r.user);
    for (const ev of r.events || []) {
      const t = ev.type;
      if (t === 'tool') logMsg('tool-note', `→ ${ev.name} ${ev.args || ''}`);
      else if (t === 'tool_result') logMsg('tool-note', `↳ ${ev.name}: ${ev.preview || ''}${ev.has_image ? ' 📷' : ''}`);
      else if (t === 'usage') logMsg('tool-note', `  · tokens +${ev.in}/+${ev.out}${ev.cost_yuan > 0 ? ' ≈ ' + fmtCost(ev.cost_yuan) : ''}`);
    }
    if (r.error) logMsg('error', '错误: ' + r.error);
    if (r.reply) logMsg('assistant', r.reply);
    else logMsg('tool-note', '（本轮无回复）');
  }
  $('chatlog').scrollTop = $('chatlog').scrollHeight;
}

/// log() 但总是 append（启动期 log 定义早于本函数也无妨）
function logMsg(kind, text) {
  const div = document.createElement('div');
  div.className = 'msg ' + kind;
  div.textContent = text;
  $('chatlog').appendChild(div);
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}
