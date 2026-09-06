(() => {
  const STATUS_MAP = {
    disconnected: { cls: 'disconnected', text: 'disconnected' },
    connecting: { cls: 'connecting', text: 'connecting' },
    live: { cls: 'live', text: 'live' },
    closed: { cls: 'closed', text: 'closed' },
  };

  const ACTIVITY_COLORS = {
    llm: 'llm', render: 'render', review: 'review', patch: 'patch', state: 'state', error: 'error',
  };

  const $ = id => document.getElementById(id);
  const sessionSelect = $('session-select');
  const newSessionBtn = $('new-session-btn');
  const statusEl = $('status');
  const drawioContainer = $('drawio-container');
  const canvasPlaceholder = $('canvas-placeholder');
  const canvasOverlay = $('canvas-overlay');
  const canvasOverlayText = $('canvas-overlay-text');
  const promptEl = $('prompt');
  const generateBtn = $('generate-btn');
  const loopBtn = $('loop-btn');
  const runStatus = $('run-status');
  const runStatusText = $('run-status-text');
  const errorBox = $('error-box');
  const activityLog = $('activity-log');
  const btnExportPng = $('export-png-btn');
  const downloadSvgBtn = $('download-svg-btn');
  const copyXmlUrlBtn = $('copy-xml-url-btn');
  const selectionPatch = $('selection-patch');
  const selectionChip = $('selection-chip');
  const selectionClear = $('selection-clear');
  const selectionInstruction = $('selection-instruction');
  const selectionModifyBtn = $('selection-modify-btn');

  let ws = null;
  let reconnectTimer = null;
  let reconnectAttempt = 0;
  let currentSessionId = null;
  let currentXml = null;
  let isRunning = false;
  let activityEntries = [];
  let currentSelection = [];
  let currentGraph = null;

  async function api(method, path, body) {
    const opts = { method, headers: {} };
    // Always set Content-Type for non-GET so the server's Json extractor
    // accepts the request (empty body is OK as long as the header is set).
    if (method !== 'GET' && method !== 'HEAD') {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = body !== undefined ? JSON.stringify(body) : '{}';
    }
    const res = await fetch(path, opts);
    const text = await res.text();
    let data;
    try { data = JSON.parse(text); } catch { data = text; }
    if (!res.ok) {
      const msg = data && data.message ? data.message : `HTTP ${res.status}`;
      throw new Error(msg);
    }
    return data;
  }

  function setStatus(state) {
    const s = STATUS_MAP[state] || STATUS_MAP.disconnected;
    statusEl.className = `status ${s.cls}`;
    statusEl.querySelector('.status-text').textContent = s.text;
    statusEl.setAttribute('aria-label', s.text);
  }

  function hashSession() {
    const m = location.hash.match(/^#\/session\/(.+)$/);
    return m ? decodeURIComponent(m[1]) : '';
  }

  function setHash(id) {
    if (!id) { history.replaceState(null, '', location.pathname + location.search); return; }
    const hash = `#/session/${encodeURIComponent(id)}`;
    if (location.hash !== hash) location.hash = hash;
  }

  function showError(msg) {
    errorBox.textContent = msg;
    errorBox.style.display = 'block';
  }

  function clearError() {
    errorBox.textContent = '';
    errorBox.style.display = 'none';
  }

  function setLoading(on, text = 'working…') {
    canvasOverlay.style.display = on ? 'flex' : 'none';
    canvasOverlayText.textContent = text;
  }

  function updateActionButtons() {
    const hasSession = !!currentSessionId;
    const hasXml = !!(currentXml && currentXml.trim());
    const enabled = hasSession && hasXml;
    btnExportPng.disabled = !enabled;
    downloadSvgBtn.disabled = !enabled;
    copyXmlUrlBtn.disabled = !enabled;
  }

  function setRunLoading(on, text = 'running…') {
    isRunning = on;
    generateBtn.disabled = on;
    loopBtn.disabled = on;
    runStatus.style.display = on ? 'flex' : 'none';
    runStatusText.textContent = text;
  }

  function updateSelectionState(cellIds) {
    currentSelection = cellIds || [];
    const count = currentSelection.length;
    selectionChip.textContent = `${count} selected`;
    selectionModifyBtn.disabled = count === 0 || !selectionInstruction.value.trim();
    if (count > 0) {
      selectionPatch.hidden = false;
    } else {
      selectionPatch.hidden = true;
      selectionInstruction.value = '';
    }
  }

  async function patchSelected() {
    if (!currentSessionId) return;
    const cellIds = currentSelection;
    const instruction = selectionInstruction.value.trim();
    if (cellIds.length === 0 || !instruction) return;

    setLoading(true, 'patching selected cells…');
    selectionModifyBtn.disabled = true;
    selectionInstruction.disabled = true;
    const previousLabel = selectionModifyBtn.textContent;
    selectionModifyBtn.textContent = 'Modifying…';
    try {
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}/patch`, {
        cell_ids: cellIds,
        instruction,
        json_mode: false,
      });
      if (result && result.xml) {
        loadXmlIntoCanvas(result.xml);
      }
      currentSelection = [];
      updateSelectionState([]);
      selectionInstruction.value = '';
      addActivity('StateTransition', { from: 'selected cells', to: `patched · v:${(result.version_id || '').slice(0, 8)}` });
    } catch (err) {
      showError(`Patch failed: ${err.message}`);
    } finally {
      setLoading(false);
      selectionModifyBtn.disabled = false;
      selectionInstruction.disabled = false;
      selectionModifyBtn.textContent = previousLabel;
    }
  }

  function escapeHtml(str) {
    return String(str).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#039;');
  }

  function syntaxHighlightXml(xml) {
    const escaped = escapeHtml(xml);
    return escaped
      .replace(/(&lt;\/?)([\w:]+)(.*?)(&gt;)/g, (m, open, tag, attrs, close) => {
        const coloredAttrs = attrs.replace(/(\s+[\w:-]+)(=)(".*?")/g, '$1<span class="attr">$2</span><span class="val">$3</span>');
        return `${open}<span class="tag">${tag}</span>${coloredAttrs}${close}`;
      })
      .replace(/(&gt;)([^&]+)(&lt;)/g, '$1<span class="text">$2</span>$3');
  }

  function loadXmlIntoCanvas(xml) {
    currentXml = xml;
    drawioContainer.innerHTML = '';
    currentGraph = null;
    currentSelection = [];
    updateSelectionState([]);
    updateActionButtons();
    if (!xml || !xml.trim()) {
      drawioContainer.style.display = 'none';
      canvasPlaceholder.style.display = 'flex';
      return;
    }
    canvasPlaceholder.style.display = 'none';
    drawioContainer.style.display = 'block';

    // TODO: replace with mxGraph embed once CDN load is reliable.
    if (typeof window.mxGraph === 'undefined' || typeof window.mxUtils === 'undefined' || typeof window.mxCodec === 'undefined') {
      drawioContainer.innerHTML = `<pre class="xml-fallback"><code>${syntaxHighlightXml(xml)}</code></pre>`;
      return;
    }

    try {
      const xmlDoc = window.mxUtils.parseXml(xml);
      if (!xmlDoc) throw new Error('parseXml returned null');
      const models = xmlDoc.getElementsByTagName('mxGraphModel');
      if (models.length === 0) throw new Error('no <mxGraphModel> found');

      const model = new window.mxGraphModel();
      const codec = new window.mxCodec(xmlDoc);
      codec.decode(models[0], model);

      const graph = new window.mxGraph(drawioContainer, model);
      currentGraph = graph;
      graph.setEnabled(true);
      graph.setPanning(true);
      graph.setCellsEditable(false);   // pan/zoom/gestures on; cells stay read-only
      graph.setCellsMovable(false);
      graph.setCellsResizable(false);
      // The vendored mxGraph bundle predates Graph.prototype.setCellsConnectable;
      // guard it so the canvas doesn't fall back to XML on an older bundle.
      if (typeof graph.setCellsConnectable === 'function') graph.setCellsConnectable(false);
      graph.centerZoom = true;
      graph.refresh();

      graph.getSelectionModel().addListener(window.mxEvent.SELECTION_CHANGED, () => {
        const selected = graph.getSelectionCells();
        const realCells = selected.filter(c => c.id && c.id !== '0' && c.id !== '1');
        updateSelectionState(realCells.map(c => c.id));
      });

      const bounds = graph.getGraphBounds();
      const border = 20;
      graph.view.translate.x = border - bounds.x;
      graph.view.translate.y = border - bounds.y;
      graph.refresh();
    } catch (err) {
      console.warn('mxGraph render failed, falling back to XML:', err);
      drawioContainer.innerHTML = `<pre class="xml-fallback"><code>${syntaxHighlightXml(xml)}</code></pre>`;
    }
  }

  async function loadSessionList() {
    try {
      const sessions = await api('GET', '/api/sessions');
      const currentVal = sessionSelect.value;
      sessionSelect.innerHTML = '<option value="">— select a session —</option>';
      (sessions || []).forEach(s => {
        const opt = document.createElement('option');
        opt.value = s.id;
        const shortId = s.id.slice(0, 8);
        const title = s.title ? `${s.title} · ${shortId}` : shortId;
        const versions = s.version_count > 0 ? ` · ${s.version_count}v` : '';
        opt.textContent = `${title}${versions}`;
        sessionSelect.appendChild(opt);
      });
      if (currentVal) sessionSelect.value = currentVal;
    } catch (err) {
      console.warn('failed to list sessions:', err);
    }
  }

  async function selectSession(id) {
    if (!id) return;
    currentSessionId = id;
    sessionSelect.value = id;
    setHash(id);
    clearError();
    setLoading(true, 'loading session…');
    closeWs();

    try {
      const session = await api('GET', `/api/sessions/${encodeURIComponent(id)}`);
      loadXmlIntoCanvas(session.current_xml || '');
      updateActionButtons();
      connectWs(id);
    } catch (err) {
      showError(`Failed to load session: ${err.message}`);
      setLoading(false);
    } finally {
      setLoading(false);
    }
  }

  async function createSession() {
    try {
      const s = await api('POST', '/api/sessions');
      await loadSessionList();
      await selectSession(s.session_id);
    } catch (err) {
      showError(`Failed to create session: ${err.message}`);
    }
  }

  function summarizeEvent(kind, payload) {
    switch (kind) {
      case 'LlmCallStarted': return { stage: 'llm', text: `call started · ${payload.prompt_chars ?? '?'} chars` };
      case 'LlmCallCompleted': return { stage: 'llm', text: `${payload.input_tokens ?? 0}+${payload.output_tokens ?? 0} tok · ${payload.finish_reason ?? 'done'}` };
      case 'RenderStarted': return { stage: 'render', text: 'render started' };
      case 'RenderCompleted': return { stage: 'render', text: `ok · ${((payload.bytes || 0) / 1024).toFixed(1)} KB · ${(payload.duration_ms / 1000).toFixed(1)}s` };
      case 'Error': return { stage: 'error', text: `${payload.stage}: ${payload.message}` };
      case 'StateTransition': return { stage: 'state', text: `${payload.from ?? '∅'} → ${payload.to}` };
      default: return { stage: 'state', text: kind };
    }
  }

  function addActivity(kind, payload, newest = true) {
    const summary = summarizeEvent(kind, payload || {});
    const li = document.createElement('li');
    if (newest) li.className = 'newest';
    li.innerHTML = `<span class="activity-chip ${ACTIVITY_COLORS[summary.stage] || 'state'}">${summary.stage}</span><span>${escapeHtml(summary.text)}</span>`;
    activityLog.appendChild(li);
    activityEntries.push(li);
    if (activityEntries.length > 20) {
      const old = activityEntries.shift();
      if (old) old.remove();
    }
    activityLog.scrollTop = activityLog.scrollHeight;
    setTimeout(() => li.classList.remove('newest'), 800);
  }

  function clearActivity() {
    activityLog.innerHTML = '';
    activityEntries = [];
  }

  function handleWsMessage(data) {
    if (!data || typeof data !== 'object') return;
    if (data.type === 'trajectory' && data.event) {
      addActivity(data.event.kind, data.event);
    } else if (data.type === 'version_created') {
      addActivity('StateTransition', { from: null, to: `version ${data.version_id}` });
    } else if (data.type === 'error') {
      addActivity('Error', { stage: 'server', message: data.message || 'unknown' });
    }
  }

  function connectWs(id) {
    closeWs();
    setStatus('connecting');
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host || '127.0.0.1:8080'}/api/sessions/${encodeURIComponent(id)}/events`;
    ws = new WebSocket(url);
    ws.addEventListener('open', () => { reconnectAttempt = 0; setStatus('live'); });
    ws.addEventListener('message', ev => {
      try { handleWsMessage(JSON.parse(ev.data)); }
      catch (err) { console.warn('malformed WS message:', err); }
    });
    ws.addEventListener('error', () => setStatus('closed'));
    ws.addEventListener('close', () => scheduleReconnect(id));
  }

  function closeWs() {
    if (ws) { ws.close(); ws = null; }
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
    reconnectAttempt = 0;
  }

  function scheduleReconnect(id) {
    if (currentSessionId !== id) return;
    const delay = Math.min(1000 * 2 ** reconnectAttempt, 30000);
    reconnectAttempt += 1;
    setStatus('connecting');
    reconnectTimer = setTimeout(() => connectWs(id), delay);
  }

  async function exportPng() {
    if (!currentSessionId) return;
    setLoading(true, 'exporting PNG…');
    try {
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}/render`);
      const base64 = result.png_base64 || '';
      if (!base64) throw new Error('server returned empty PNG');

      const binary = atob(base64);
      const bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i += 1) {
        bytes[i] = binary.charCodeAt(i);
      }
      const blob = new Blob([bytes], { type: 'image/png' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `drawio-agent-${currentSessionId.slice(0, 8)}.png`;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);

      btnExportPng.disabled = false;
      downloadSvgBtn.disabled = false;
      copyXmlUrlBtn.disabled = false;
      addActivity('StateTransition', { from: 'export', to: 'PNG downloaded' });
    } catch (err) {
      showError(`PNG export failed: ${err.message}`);
    } finally {
      setLoading(false);
    }
  }

  async function runGenerate() {
    if (!currentSessionId) { showError('Select or create a session first.'); return; }
    const prompt = promptEl.value.trim();
    if (!prompt) { showError('Enter a prompt first.'); return; }
    clearError();
    setRunLoading(true, 'generating…');
    setLoading(true, 'generating…');
    try {
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}/generate`, { prompt });
      loadXmlIntoCanvas(result.xml || '');
      await loadSessionList();
    } catch (err) {
      showError(`Generate failed: ${err.message}`);
    } finally {
      setRunLoading(false);
      setLoading(false);
    }
  }

  async function runLoop() {
    if (!currentSessionId) { showError('Select or create a session first.'); return; }
    const prompt = promptEl.value.trim();
    if (!prompt) { showError('Enter a prompt first.'); return; }
    clearError();
    clearActivity();
    setRunLoading(true, 'agent loop running…');
    setLoading(true, 'agent loop running…');
    try {
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}/agent-loop`, { prompt });
      loadXmlIntoCanvas(result.xml || '');
      await loadSessionList();
      if (result.converged) {
        addActivity('StateTransition', { from: 'loop', to: `converged · ${result.iterations} iterations` });
      } else {
        addActivity('StateTransition', { from: 'loop', to: `finished · ${result.iterations} iterations · not converged` });
      }
    } catch (err) {
      showError(`Agent loop failed: ${err.message}`);
    } finally {
      setRunLoading(false);
      setLoading(false);
    }
  }

  async function downloadSvg() {
    if (!currentSessionId) { showError('No session selected'); return; }
    setLoading(true, 'exporting SVG…');
    try {
      const session = await api('GET', `/api/sessions/${encodeURIComponent(currentSessionId)}`);
      const xml = session.current_xml || '';
      if (!xml) { showError('Session has no XML yet'); return; }

      if (typeof window.mxUtils === 'undefined' || typeof window.mxSvgCanvas2D === 'undefined' ||
          typeof window.mxImageExport === 'undefined' || typeof window.mxConstants === 'undefined') {
        showError('SVG export requires the mxGraph bundle (viewer-static.min.js)');
        return;
      }

      // Load XML into a fresh mxGraph (same path as the canvas render).
      const xmlDoc = window.mxUtils.parseXml(xml);
      const modelEl = xmlDoc.getElementsByTagName('mxGraphModel')[0];
      if (!modelEl) throw new Error('no <mxGraphModel> in session XML');
      const model = new window.mxGraphModel();
      new window.mxCodec(xmlDoc).decode(modelEl, model);

      // Off-screen container so mxGraph can lay out and we can read bounds.
      const off = document.createElement('div');
      off.style.cssText = 'position:absolute;left:-99999px;top:-99999px;width:1px;height:1px;';
      document.body.appendChild(off);
      const graph = new window.mxGraph(off, model);
      graph.setEnabled(false);
      graph.refresh();
      const bounds = graph.getGraphBounds();

      const border = 20;
      const scale = 1;
      const background = '#ffffff';

      // Render the cells into a real SVG document via mxSvgCanvas2D.
      const svgDoc = window.mxUtils.createXmlDocument();
      const root = svgDoc.createElementNS(window.mxConstants.NS_SVG, 'svg');
      root.setAttribute('xmlns', window.mxConstants.NS_SVG);
      root.setAttribute('width', Math.max(1, Math.round((bounds.width + 2 * border) * scale)) + 'px');
      root.setAttribute('height', Math.max(1, Math.round((bounds.height + 2 * border) * scale)) + 'px');
      root.setAttribute('version', '1.1');
      root.setAttribute('style', `background:${background};`);

      const canvas = new window.mxSvgCanvas2D(root, background);
      canvas.scale(scale / graph.view.scale);
      canvas.translate(-bounds.x + border, -bounds.y + border);
      const imgExport = new window.mxImageExport();
      imgExport.drawState(graph.getView().getState(graph.getModel().getRoot()), canvas);

      document.body.removeChild(off);

      const svg = new XMLSerializer().serializeToString(root);
      const blob = new Blob([svg], { type: 'image/svg+xml;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `drawio-agent-${currentSessionId.slice(0, 8)}.svg`;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
    } catch (err) {
      showError(`SVG export failed: ${err.message}`);
    } finally {
      setLoading(false);
    }
  }

  async function copyToClipboard(text) {
    // Try the modern async API first (requires a secure context + gesture).
    if (navigator.clipboard && window.isSecureContext) {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch (err) {
        console.warn('clipboard.writeText failed:', err);
      }
    }
    // Fallback: hidden textarea + execCommand('copy').
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.cssText = 'position:fixed;top:-9999px;left:-9999px;opacity:0;';
    document.body.appendChild(ta);
    ta.focus();
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand('copy');
    } catch (err) {
      console.warn('execCommand copy failed:', err);
    }
    document.body.removeChild(ta);
    return ok;
  }

  async function copyXmlUrl() {
    if (!currentSessionId) return;
    try {
      const versions = await api('GET', `/api/sessions/${encodeURIComponent(currentSessionId)}/versions`);
      const latest = versions && versions.length ? versions[versions.length - 1].version_id : null;
      const url = latest
        ? `${location.origin}/api/sessions/${encodeURIComponent(currentSessionId)}/versions/${encodeURIComponent(latest)}`
        : `${location.origin}/api/sessions/${encodeURIComponent(currentSessionId)}`;
      const ok = await copyToClipboard(url);
      if (ok) {
        addActivity('state', { from: 'clipboard', to: 'XML URL copied' });
      } else {
        showError(`Copy failed — URL: ${url}`);
      }
    } catch (err) {
      showError(`Copy failed: ${err.message}`);
    }
  }

  async function init() {
    await loadSessionList();

    sessionSelect.addEventListener('change', () => {
      const id = sessionSelect.value;
      if (id) selectSession(id);
      else { currentSessionId = null; setHash(''); }
    });

    newSessionBtn.addEventListener('click', createSession);
    generateBtn.addEventListener('click', runGenerate);
    loopBtn.addEventListener('click', runLoop);
    btnExportPng.addEventListener('click', exportPng);
    downloadSvgBtn.addEventListener('click', downloadSvg);
    copyXmlUrlBtn.addEventListener('click', copyXmlUrl);

    selectionClear.addEventListener('click', () => {
      if (currentGraph) currentGraph.clearSelection();
    });
    selectionModifyBtn.addEventListener('click', patchSelected);
    selectionInstruction.addEventListener('input', () => {
      selectionModifyBtn.disabled = currentSelection.length === 0 || !selectionInstruction.value.trim();
    });
    selectionInstruction.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault();
        patchSelected();
      }
    });

    window.addEventListener('hashchange', async () => {
      const id = hashSession();
      if (id) await selectSession(id);
      else if (location.hash === '#/new') await createSession();
    });

    promptEl.focus();

    if (location.hash === '#/new') {
      await createSession();
    } else {
      const id = hashSession();
      if (id) await selectSession(id);
    }
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
