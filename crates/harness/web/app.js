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
    if (!models.length) throw new Error('no <mxGraphModel> found');
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
// Data + chat
// ---------------------------------------------------------------------------

function log(kind, text) {
  const div = document.createElement('div');
  div.className = 'msg ' + kind;
  div.textContent = text;
  $('chatlog').appendChild(div);
  $('chatlog').scrollTop = $('chatlog').scrollHeight;
}

async function refreshCanvas() {
  const resp = await fetch('/api/file');
  const xml = await resp.text();
  loadXmlIntoCanvas(xml);
  setSelection([]);
}

async function api(path, body) {
  const resp = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body || {}),
  });
  return resp.json();
}

async function loadState() {
  const st = await (await fetch('/api/state')).json();
  $('file').textContent = st.file;
  $('cells').textContent = `${st.cells} 个元素 / ${st.lines} 行`;
  $('llm').textContent = st.llm_ready ? 'LLM ✓' : 'LLM ✗';
  $('llm-banner').hidden = st.llm_ready;
}

$('check').onclick = async () => {
  const r = await api('/api/check');
  if (r.ok) log('ok', `✓ 检查通过（cells=${r.cells} edges=${r.edges}）`);
  else {
    log('error', '✗ 检查发现 ' + (r.issues || []).length + ' 个问题：\n' + (r.issues || []).join('\n'));
  }
};
$('undo').onclick = async () => {
  const r = await api('/api/undo');
  if (r.ok) { log('tool-note', '↩ 已撤销，画布已回滚'); await refreshCanvas(); }
  else log('error', '撤销失败：' + (r.error || ''));
};
$('reload').onclick = async () => {
  const r = await api('/api/reload');
  log('tool-note', `已从磁盘重新加载（${r.cells || 0} 个元素）`);
  await refreshCanvas();
};

$('chatform').onsubmit = async (ev) => {
  ev.preventDefault();
  if (busy) return;
  const text = $('input').value.trim();
  if (!text) return;
  busy = true;
  $('send').disabled = true;
  const ids = selectedIds.slice();
  log('user', text);
  $('input').value = '';
  setSelection([]);
  try {
    const r = await api('/api/chat', { text, cell_ids: ids });
    if (r.error) {
      log('error', '对话出错：' + r.error);
    } else {
      log('tool-note', `（工具调用 ${r.tool_calls} 次）`);
      log('assistant', r.reply);
    }
  } catch (e) {
    log('error', '网络错误：' + e);
  }
  busy = false;
  $('send').disabled = false;
  await refreshCanvas();
  const st = await (await fetch('/api/state')).json();
  $('cells').textContent = `${st.cells} 个元素 / ${st.lines} 行`;
};

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

(async () => {
  await loadState();
  await refreshCanvas();
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

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const payload = {
    base_url: cfgBaseUrl.value.trim(),
    model: cfgModel.value.trim(),
    api_key: cfgApiKey.value.trim(),
  };
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
