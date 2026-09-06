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
    currentGraph.setPanning(true);
  } else {
    currentGraph.setPanning(false);
  }
  cancelRubberBand();
}
$('mode-select').onclick = () => { canvasMode = 'select'; applyMode(); };
$('mode-pan').onclick = () => { canvasMode = 'pan'; applyMode(); };

canvasEl.addEventListener('mousedown', (e) => {
  if (canvasMode !== 'select' || busy || e.button !== 0) return;
  if (e.target !== canvasEl) return;   // clicked on a cell -> graph handles it
  const rect = canvasEl.getBoundingClientRect();
  rubberBand = { x1: e.clientX, y1: e.clientY, x2: e.clientX, y2: e.clientY };
  rubberBandEl = document.createElement('div');
  rubberBandEl.className = 'rubber-band';
  canvasEl.appendChild(rubberBandEl);
  paintRubberBand();
  e.preventDefault();
});
window.addEventListener('mousemove', (e) => {
  if (!rubberBand) return;
  rubberBand.x2 = e.clientX; rubberBand.y2 = e.clientY;
  paintRubberBand();
});
window.addEventListener('mouseup', () => {
  if (!rubberBand || !currentGraph) { cancelRubberBand(); return; }
  const rect = rubberBand;
  const p1 = mxUtils.convertPoint(currentGraph.container, rect.x1, rect.y1);
  const p2 = mxUtils.convertPoint(currentGraph.container, rect.x2, rect.y2);
  const g = {
    x: Math.min(p1.x, p2.x), y: Math.min(p1.y, p2.y),
    width: Math.abs(p2.x - p1.x), height: Math.abs(p2.y - p1.y),
  };
  const cells = hitTestCells(g);
  cancelRubberBand();
  try { currentGraph.setSelectionCells(cells); } catch (err) { console.warn(err); }
  if (cells.length) setSelection(cells.filter((c) => c.id).map((c) => c.id));
});

function paintRubberBand() {
  if (!rubberBandEl || !rubberBand) return;
  const r = rubberBand;
  const left = Math.min(r.x1, r.x2), top = Math.min(r.y1, r.y2);
  const p = mxUtils.convertPoint(currentGraph.container, left, top);
  const w = Math.abs(r.x2 - r.x1), h = Math.abs(r.y2 - r.y1);
  rubberBandEl.style.left = p.x + 'px';
  rubberBandEl.style.top = p.y + 'px';
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
  // 选中会话即看到它的历史记录（只读时间线）
  await openHistory(true);
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
      // 进入页面即展示当前会话的历史记录（只读时间线）
      await openHistory(true);
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
// R5: per-file history panel (trajectory inspect / restore / export/import)
// ---------------------------------------------------------------------------

const histLog = $('histlog');
let histOpen = false;

async function openHistory(force) {
  if (busy && typeof force !== 'boolean') { log('error', '任务运行中——先点「停止」再查看历史'); return; }
  if (typeof force === 'boolean') histOpen = force;
  else histOpen = !histOpen;
  histLog.hidden = !histOpen;
  $('hist-btn').classList.toggle('active', histOpen);
  if (!histOpen) return;
  let data;
  try { data = await (await fetch('/api/history')).json(); }
  catch (e) { log('error', '历史读取失败: ' + e); return 0; }
  const count = (data.records || []).length;
  $('hist-btn').textContent = count ? `历史(${count})` : '历史';
  histLog.innerHTML = '';
  // 作用域标注：历史属于「当前会话（文件）」这条时间线
  const scope = document.createElement('div');
  scope.className = 'hist-meta';
  scope.style.margin = '4px 2px 6px';
  const cur = picker.value ? picker.value : '(未选择会话)';
  scope.innerHTML = `时间线 · 当前会话：<b>${escapeHtml(cur)}</b><br>
    <span style="font-size:11px">会话 = 文件（横向切换工作区）；这里 = 本文件内的任务轨迹与版本回滚。</span>`;
  histLog.appendChild(scope);
  if (!data.records || !data.records.length) {
    const d = document.createElement('div');
    d.className = 'dim';
    d.textContent = '暂无历史记录（完成一次对话后自动记录）';
    histLog.appendChild(d);
    return count;
  }
  for (const r of data.records) {
    const item = document.createElement('div');
    item.className = 'hist-item';
    const d = new Date(r.ts * 1000);
    const ts = d.toLocaleString('zh-CN', { hour12: false });
    item.innerHTML = `<div class="hist-user">${escapeHtml(r.user)}</div>
      <div class="hist-meta">${ts} · ${r.tool_calls} 次工具 · ${r.usage_in + r.usage_out} tokens${r.cost_yuan > 0 ? ' · ' + fmtCost(r.cost_yuan) : ''}${r.error ? ' · ⚠ 出错' : ''}</div>`;
    const detail = document.createElement('div');
    detail.className = 'hist-detail';
    detail.hidden = true;
    item.appendChild(detail);
    item.onclick = async () => {
      detail.hidden = !detail.hidden;
      if (detail.hidden) return;
      detail.textContent = '加载中…';
      const full = await (await fetch('/api/history/' + r.idx)).json();
      if (!full.ok) { detail.textContent = full.error || '读取失败'; return; }
      const rec = full.record;
      let txt = '';
      for (const ev of rec.events || []) {
        const t = ev.type;
        if (t === 'tool') txt += `→ ${ev.name} ${ev.args || ''}\n`;
        else if (t === 'tool_result') txt += `↳ ${ev.name}: ${ev.preview || ''}${ev.has_image ? ' 📷' : ''}\n`;
        else if (t === 'usage') txt += `· tokens +${ev.in}/+${ev.out}\n`;
        else if (t === 'reply') txt += `回复: ${ev.reply}\n`;
        else if (t === 'error') txt += `错误: ${ev.error}\n`;
      }
      if (rec.error) txt += `错误: ${rec.error}\n`;
      txt += `\n最终回复: ${rec.reply}\n（xml ${rec.xml.length} 字符）`;
      detail.textContent = txt;
      // 历史是只读记录：仅提供导出，不做回溯
      const acts = document.createElement('div');
      acts.className = 'hist-actions';
      const dl = document.createElement('button');
      dl.textContent = '导出会话 JSON';
      dl.onclick = (e) => {
        e.stopPropagation();
        const blob = new Blob([JSON.stringify(rec, null, 2)], { type: 'application/json' });
        const a = document.createElement('a');
        a.href = URL.createObjectURL(blob);
        a.download = `drawio-ctx-${r.idx}.json`;
        a.click();
      };
      acts.append(dl);
      detail.appendChild(acts);
    };
    histLog.appendChild(item);
  }
  return count;
}
$('hist-btn').onclick = () => openHistory();

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
