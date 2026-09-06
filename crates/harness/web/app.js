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
    applyMode();

    graph.getSelectionModel().addListener(window.mxEvent.SELECTION_CHANGED, () => {
      const ids = graph.getSelectionCells()
        .filter((c) => c.id && c.id !== '0' && c.id !== '1')
        .map((c) => c.id);
      setSelection(ids);
    });

    const b = graph.getGraphBounds();
    graph.view.translate.x = 24 - b.x;
    graph.view.translate.y = 24 - b.y;
    graph.refresh();
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
}
$('mode-select').onclick = () => { canvasMode = 'select'; applyMode(); };
$('mode-pan').onclick = () => { canvasMode = 'pan'; applyMode(); };

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
/// client 坐标 → 图坐标：view.scale / view.translate 必须参与换算。
/// mxUtils.convertPoint 只减去容器偏移，不含 translate——用它检测会全空。
function clientToGraph(clientX, clientY) {
  const rect = currentGraph.container.getBoundingClientRect();
  const v = currentGraph.view;
  return {
    x: (clientX - rect.left) / v.scale - v.translate.x,
    y: (clientY - rect.top) / v.scale - v.translate.y,
  };
}

// 关键：capture 阶段阻断 mousedown。Safari/WebKit 不会因 pointerdown 的
// preventDefault 而抑制兼容 mousedown，mxGraph 自己的 mousedown 处理器
// （shift 时它会再做一次 toggle）会造成双重选择/抵消。本页交互全部走
// pointerdown，mxGraph 的鼠标路径不需要；此监听注册于页面加载，早于
// mxGraph 的 bubble 监听，stopPropagation 可确定性阻止它。
canvasEl.addEventListener('mousedown', (e) => {
  e.stopPropagation();
  e.preventDefault();
}, true);

canvasEl.addEventListener('pointerdown', (e) => {
  if (e.button !== 0 || !currentGraph || busy) return;

  if (canvasMode === 'pan') {
    // 自实现拖拽平移：用 fork 的正确原语 scaleAndTranslate（更新
    // translate + revalidate 状态），保持"渲染/状态/命中检测"三者同步；
    // 不用 refresh()（它清空全部 state 且触发 SIZE 事件，是副作用来源）。
    const v = currentGraph.view;
    const startX = e.clientX, startY = e.clientY;
    const t0 = { x: v.translate.x, y: v.translate.y };
    // 每个 pointermove 同步更新：scaleAndTranslate 内部同步 revalidate
    // （状态）并重绘 SVG（视觉）。Safari 下 SVG 重绘有自己的节奏，
    // 任何节流都会制造"视觉滞后于状态"的窗口——松开后立刻 shift 点选
    // 就会命中旧视觉位置的 cell。同步更新则两套坐标永远锁步。
    const apply = (ev) => {
      v.scaleAndTranslate(
        v.scale,
        t0.x + (ev.clientX - startX) / v.scale,
        t0.y + (ev.clientY - startY) / v.scale
      );
    };
    const onMove = (ev) => apply(ev);
    const onUp = (ev) => {
      apply(ev); // 松开时用最终坐标同步落位
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    e.preventDefault(); // 同时抑制 mxGraph 的 mouse 兼容事件
    return;
  }

  const p = clientToGraph(e.clientX, e.clientY);
  let cell = currentGraph.getCellAt(p.x, p.y);
  if (!cell) cell = getCellAtBbox(currentGraph, p.x, p.y);
  // select 模式
  if (cell) {
    if (e.shiftKey) {
      // Shift 点选：切换该 cell 的选中状态
      const already = currentGraph.getSelectionCells().some((c) => c === cell);
      if (already) currentGraph.removeSelectionCell(cell);
      else currentGraph.addSelectionCell(cell);
    } else {
      currentGraph.setSelectionCell(cell);
    }
    e.preventDefault();
    return;
  }
  // 空白处 → 开始框选（Shift 拉框 = 追加到现有选择）
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
  const origScaleAndTranslate = currentGraph ? currentGraph.view.scaleAndTranslate.bind(currentGraph.view) : null;
  if (origScaleAndTranslate) {
    currentGraph.view.scaleAndTranslate = function (a, b, c) {
      origScaleAndTranslate(a, b, c);
      paintStateOverlay();
    };
  }
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

$('cfg-preset-btn').onclick = () => {
  const model = cfgModel.value.trim().toLowerCase();
  const presets = {
    'glm-4.6': [5, 15], 'glm-4.5': [5, 15], 'glm-4-flash': [0, 0],
    'glm-4v': [0.1, 0.1], 'deepseek-chat': [2, 8], 'deepseek-reasoner': [4, 16],
    'gpt-4o': [17, 68], 'gpt-4o-mini': [1.1, 4.4],
  };
  let picked = null;
  for (const [k, v] of Object.entries(presets)) {
    if (model.includes(k)) { picked = v; break; }
  }
  if (picked) {
    cfgPriceIn.value = picked[0];
    cfgPriceOut.value = picked[1];
    setTestResult('ok', `已按 ${cfgModel.value.trim()} 填入价格（¥${picked[0]}/${picked[1]} 每百万 tokens，2025 官方公开价，以账单为准可改）`);
  } else {
    setTestResult('err', `没有 ${cfgModel.value.trim() || '(空)'} 的预设价格，请手动填写（可参考: glm-4.6 5/15、deepseek-chat 2/8、gpt-4o-mini 1.1/4.4）`);
  }
};

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

$('ctx-import-btn').onclick = () => { if (guardBusy()) return; $('ctx-file').click(); };
$('ctx-file').onchange = async () => {
  const file = $('ctx-file').files[0];
  if (!file) return;
  log('tool-note', `导入会话 ${file.name} …`);
  try {
    const resp = await fetch('/api/context/load', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: await file.text(),
    });
    const r = await resp.json();
    if (r.ok) {
      log('ok', `✓ 会话已加载：${r.cells} 个元素，${r.memory_messages} 条记忆消息`);
      await refreshCanvas();
      const st = await (await fetch('/api/state')).json();
      $('cells').textContent = `${st.cells} 个元素 / ${st.lines} 行`;
      if (st.session) renderUsage(st.session);
    } else log('error', '导入失败: ' + (r.error || ''));
  } catch (e) { log('error', '导入失败: ' + e); }
  $('ctx-file').value = '';
};

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}
