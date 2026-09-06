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
  const sendBtn = $('send-btn');
  const thinkingOptions = document.querySelectorAll('.thinking-option');
  const refineHint = $('refine-hint');
  const refineHintCta = $('refine-hint-cta');
  const runStatus = $('run-status');
  const runStatusText = $('run-status-text');
  const errorBox = $('error-box');
  const chatThread = $('chat-thread');
  const promptAttachments = $('prompt-attachments');
  const regenBtn = $('regen-btn');
  const btnExportPng = $('export-png-btn');
  const downloadSvgBtn = $('download-svg-btn');
  const copyXmlUrlBtn = $('copy-xml-url-btn');
  const canvasToolPan = $('canvas-tool-pan');
  const canvasToolSelect = $('canvas-tool-select');

  let ws = null;
  let reconnectTimer = null;
  let reconnectAttempt = 0;
  let currentSessionId = null;
  let currentXml = null;
  let isRunning = false;
  // Live-run state: while a request is in flight, WS trajectory events are
  // traced into the agent bubble's status area (the old standalone Activity
  // panel is gone). `finished` stops late events from mutating a resolved bubble.
  let runCtx = null;
  // Canvas cells attached to the NEXT message as a selection reference
  // (the drawing canvas analogue of @File in a coding agent). When present,
  // Send routes to /patch (scope mode) instead of /generate or /agent-loop.
  let currentSelection = [];
  let currentGraph = null;
  let currentDepth = 'fast';
  // Canvas interaction mode: 'pan' (default) or 'select' (rubber-band marquee).
  let canvasMode = 'pan';
  // In-progress rubber band, in raw client (viewport) coordinates.
  let rubberBand = null;
  // The visible <div> rectangle overlay.
  let rubberBandEl = null;

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
      // Server errors are `{ "error": "..." }`; surface the real message so
      // users see actionable text (e.g. "run /generate first") instead of a
      // bare status code.
      const serverMsg = data && (data.message || data.error);
      const msg = serverMsg || `HTTP ${res.status}`;
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
    regenBtn.hidden = !enabled;
    updateSendButton();
  }

  function canSend() {
    const hasSession = !!currentSessionId;
    const hasPrompt = !!promptEl.value.trim();
    const hasXml = !!(currentXml && currentXml.trim());
    return hasSession && hasPrompt && !isRunning;
  }

  function updateSendButton() {
    sendBtn.disabled = !canSend();
  }

  function setRunLoading(on, text = 'running…') {
    isRunning = on;
    sendBtn.disabled = !canSend();
    runStatus.style.display = on ? 'flex' : 'none';
    runStatusText.textContent = text;
  }

  function setDepth(depth) {
    currentDepth = depth === 'refine' ? 'refine' : 'fast';
    thinkingOptions.forEach(opt => {
      const checked = opt.dataset.depth === currentDepth;
      opt.setAttribute('aria-checked', String(checked));
      opt.classList.toggle('active', checked);
    });
    updateSendButton();
    updateRefineHint();
  }

  function updateRefineHint() {
    // Refine is now self-contained: it auto-generates a baseline if the
    // session has no diagram, and refines if there is one. The hint now
    // warns only when the user is about to *replace* an existing canvas,
    // so they know their current work will be re-rendered.
    const hasSession = !!currentSessionId;
    const hasPrompt = !!promptEl.value.trim();
    const hasXml = !!(currentXml && currentXml.trim());
    const hasRef = currentSelection.length > 0;
    const shouldShow = hasSession && hasPrompt && currentDepth === 'refine' && hasXml && !isRunning && !hasRef;
    if (shouldShow) {
      refineHint.hidden = false;
      requestAnimationFrame(() => refineHint.classList.add('visible'));
    } else {
      refineHint.classList.remove('visible');
      setTimeout(() => { if (!refineHint.classList.contains('visible')) refineHint.hidden = true; }, 200);
    }
  }

  function updateSelectionState(cellIds) {
    currentSelection = cellIds || [];
    renderAttachments();
    updateSendButton();
  }

  function renderAttachments() {
    promptAttachments.innerHTML = '';
    if (!currentSelection.length) {
      promptAttachments.hidden = true;
      return;
    }
    const chip = document.createElement('span');
    chip.className = 'attach-chip';
    chip.innerHTML = `<span>◎ ${currentSelection.length} selected cell${currentSelection.length > 1 ? 's' : ''} — message will patch ONLY these</span>`;
    const x = document.createElement('button');
    x.className = 'attach-x';
    x.type = 'button';
    x.textContent = '✕';
    x.title = 'Remove selection reference';
    x.setAttribute('aria-label', 'Remove selection reference');
    x.addEventListener('click', () => {
      currentSelection = [];
      if (currentGraph) currentGraph.clearSelection();
      renderAttachments();
      updateSendButton();
      promptEl.focus();
    });
    chip.appendChild(x);
    promptAttachments.appendChild(chip);
    promptAttachments.hidden = false;
    updateRefineHint();
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

  // ---------------------------------------------------------------------------
  // Marquee (rubber-band) box selection. The vendored 2018-era mxGraph has no
  // setRubberBandSelection API, so we draw the rectangle ourselves and hit-test
  // cell bounds against it on mouseup.
  // ---------------------------------------------------------------------------

  function setCanvasMode(mode) {
    canvasMode = mode;
    drawioContainer.dataset.mode = mode;
    drawioContainer.style.cursor = mode === 'select' ? 'crosshair' : '';
    canvasToolPan.classList.toggle('active', mode === 'pan');
    canvasToolSelect.classList.toggle('active', mode === 'select');
    canvasToolPan.setAttribute('aria-pressed', String(mode === 'pan'));
    canvasToolSelect.setAttribute('aria-pressed', String(mode === 'select'));
    if (!currentGraph) return;
    if (mode === 'pan') {
      // Pure navigation: panning on, no cell-selection side-effects.
      currentGraph.setPanning(true);
      currentGraph.setCellsSelectable(false);
    } else {
      // Select mode: panning off so a drag draws a marquee; cells selectable.
      currentGraph.setPanning(false);
      currentGraph.setCellsSelectable(true);
    }
    cancelRubberBand();
  }

  /// Start a rubber band at a client point. Returns false if the pointer is on
  /// a cell (single-select handles it) or a band is already active.
  function startRubberBand(clientX, clientY, cell) {
    if (cell) return false;
    if (!currentGraph) return false;
    rubberBand = { startX: clientX, startY: clientY, currentX: clientX, currentY: clientY };
    const p = mxUtils.convertPoint(currentGraph.container, clientX, clientY);
    rubberBandEl = document.createElement('div');
    rubberBandEl.className = 'rubber-band';
    rubberBandEl.style.left = p.x + 'px';
    rubberBandEl.style.top = p.y + 'px';
    rubberBandEl.style.width = '0px';
    rubberBandEl.style.height = '0px';
    drawioContainer.appendChild(rubberBandEl);
    currentGraph.setPanning(false); // don't pan while the box is being drawn
    return true;
  }

  function updateRubberBand(clientX, clientY) {
    if (!rubberBand || !rubberBandEl || !currentGraph) return;
    rubberBand.currentX = clientX;
    rubberBand.currentY = clientY;
    const p1 = mxUtils.convertPoint(currentGraph.container, rubberBand.startX, rubberBand.startY);
    const p2 = mxUtils.convertPoint(currentGraph.container, clientX, clientY);
    rubberBandEl.style.left = Math.min(p1.x, p2.x) + 'px';
    rubberBandEl.style.top = Math.min(p1.y, p2.y) + 'px';
    rubberBandEl.style.width = Math.abs(p2.x - p1.x) + 'px';
    rubberBandEl.style.height = Math.abs(p2.y - p1.y) + 'px';
  }

  function endRubberBand() {
    if (!rubberBand || !currentGraph) {
      cancelRubberBand();
      return;
    }
    const rect = screenRectToGraphRect(
      Math.min(rubberBand.startX, rubberBand.currentX),
      Math.min(rubberBand.startY, rubberBand.currentY),
      Math.abs(rubberBand.currentX - rubberBand.startX),
      Math.abs(rubberBand.currentY - rubberBand.startY)
    );
    const cells = hitTestCells(currentGraph, rect);
    try {
      currentGraph.setSelectionCells(cells);
    } catch (err) {
      // Safety net: if the bundle still throws somewhere, the selection model
      // was already updated — the panel update below covers it.
      console.warn('setSelectionCells threw (selection applied anyway):', err);
    }
    // The SELECTION_CHANGED listener normally updates the panel; this is a
    // belt-and-suspenders call so "N selected" is always correct.
    updateSelectionState(cells.filter(c => c.id && c.id !== '0' && c.id !== '1').map(c => c.id));
    cancelRubberBand();
  }

  function cancelRubberBand() {
    if (rubberBandEl) { rubberBandEl.remove(); rubberBandEl = null; }
    rubberBand = null;
    // Restore panning only when back in pan mode (select mode keeps it off).
    if (currentGraph && canvasMode === 'pan') currentGraph.setPanning(true);
  }

  /// Convert a client-coordinate rectangle into graph/view coordinates.
  function screenRectToGraphRect(x, y, w, h) {
    const p1 = mxUtils.convertPoint(currentGraph.container, x, y);
    const p2 = mxUtils.convertPoint(currentGraph.container, x + w, y + h);
    return { x: p1.x, y: p1.y, width: p2.x - p1.x, height: p2.y - p1.y };
  }

  /// Return all cells whose rendered bounds intersect the given graph rect.
  function hitTestCells(graph, rect) {
    const model = graph.getModel();
    const root = model.getRoot();
    const result = [];
    // Cells live under the default parent (id="1"), which itself is a root
    // child — walk the whole tree. `graph.getCellBounds` returns null in the
    // vendored bundle, so use the view state's x/y/width/height (view space).
    const walk = (cell) => {
      if (!cell) return;
      if (cell.id !== '0' && cell.id !== '1') {
        const st = graph.getView().getState(cell);
        if (st && st.width > 0 && st.height > 0) {
          if (!(
            st.x + st.width < rect.x ||
            rect.x + rect.width < st.x ||
            st.y + st.height < rect.y ||
            rect.y + rect.height < st.y
          )) {
            result.push(cell);
          }
        }
      }
      for (let i = 0; i < model.getChildCount(cell); i++) {
        walk(model.getChildAt(cell, i));
      }
    };
    for (let i = 0; i < model.getChildCount(root); i++) {
      walk(model.getChildAt(root, i));
    }
    return result;
  }

  /// All cells in the model, pre-order (later = drawn on top in mxGraph).
  function collectAllCells(graph) {
    const model = graph.getModel();
    const root = model.getRoot();
    const out = [];
    const walk = (c) => {
      if (!c) return;
      out.push(c);
      for (let i = 0; i < model.getChildCount(c); i++) walk(model.getChildAt(c, i));
    };
    walk(root);
    return out;
  }

  /// Point-in-bounding-box hit test. `graph.getCellAt` misses white-fill cells
  /// whose visible fill is transparent, and nested/group cells; this walks the
  /// whole model and checks the rendered state bounds instead. Topmost first.
  function getCellAtBbox(graph, x, y) {
    const cells = collectAllCells(graph);
    for (let i = cells.length - 1; i >= 0; i--) {
      const cell = cells[i];
      if (!cell || !cell.id || cell.id === '0' || cell.id === '1') continue;
      const state = graph.view.getState(cell);
      if (!state) continue;
      // Skip cells without rendered bounds (e.g. edges with no waypoints).
      if (typeof state.x !== 'number' || typeof state.width !== 'number') continue;
      const inside = x >= state.x && x <= state.x + state.width
                  && y >= state.y && y <= state.y + state.height;
      if (inside) return cell;
    }
    return null;
  }

  /// The vendored bundle is a draw.io fork whose selection/handle handlers call
  /// Graph.prototype methods the base mxGraph lacks (isTableCell, isTableRow,
  /// isTable, getLinksForState). Without them setSelectionCells throws mid-
  /// update and SELECTION_CHANGED never fires, so the selection panel stays
  /// stale. Stub them with correct defaults for non-table diagrams.
  function patchGraphForBundle(graph) {
    if (typeof graph.isTableCell !== 'function') graph.isTableCell = () => false;
    if (typeof graph.isTableRow !== 'function') graph.isTableRow = () => false;
    if (typeof graph.isTable !== 'function') graph.isTable = () => false;
    if (typeof graph.getLinksForState !== 'function') graph.getLinksForState = () => [];
  }

  function loadXmlIntoCanvas(xml) {
    // Destroy the previous graph so its listeners/timers don't linger when a
    // new mxGraph is built on the same container (patch/generate reloads).
    if (currentGraph) {
      try { currentGraph.destroy(); } catch (e) { /* ignore */ }
      currentGraph = null;
    }
    currentXml = xml;
    drawioContainer.innerHTML = '';
    currentSelection = [];
    updateSelectionState([]);
    updateActionButtons();
    updateRefineHint();
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
      patchGraphForBundle(graph);
      graph.setEnabled(true);
      graph.setPanning(true);
      graph.setCellsEditable(false);   // pan/zoom/gestures on; cells stay read-only
      graph.setCellsMovable(false);
      graph.setCellsResizable(false);
      // The vendored mxGraph bundle predates Graph.prototype.setCellsConnectable;
      // guard it so the canvas doesn't fall back to XML on an older bundle.
      if (typeof graph.setCellsConnectable === 'function') graph.setCellsConnectable(false);
      graph.setCellsSelectable(true);  // explicit: keep cell selection enabled
      graph.centerZoom = true;
      graph.refresh();

      // Safari/macOS trackpad: mxClient.IS_TOUCH is true on Safari (ontouchstart
      // exists), so the old mxGraph bundle routes interaction through its touch
      // path, which is buggy here — two-finger drag pans horizontally only and
      // taps don't select. touch-action:none stops the browser from hijacking
      // two-finger drag into page scroll; the single-touch bridge routes taps
      // through the working mouse path so cells become selectable.
      graph.container.style.touchAction = 'none';
      if ('ontouchstart' in window) {
        graph.container.addEventListener('touchstart', (e) => {
          if (e.touches.length === 1) {
            const t = e.touches[0];
            const me = new MouseEvent('mousedown', {
              clientX: t.clientX, clientY: t.clientY,
              bubbles: true, cancelable: true, view: window, button: 0,
            });
            graph.container.dispatchEvent(me);
          }
        }, { passive: false });
      }

      // Re-apply the current canvas mode (pan/select) to the fresh graph. On the
      // first load canvasMode is 'pan' (the default); on later rebuilds the
      // user's chosen mode is preserved.
      setCanvasMode(canvasMode);

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
    clearChat();
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

  /** Legacy name kept for call sites outside a run (exports, clipboard…):
   *  with the Activity panel merged into agent bubbles there is nothing to
   *  append outside an active run, so this is a no-op there. */
  function addActivity(kind, payload) {
    if (runCtx && !runCtx.finished) pushTrace(kind, payload || {});
  }

  /** Append one live status row to the running agent bubble. Stage words
   *  (Drawing / Viewing / Patching / Refining) are derived from the event
   *  stream plus the route the request took. */
  function pushTrace(kind, payload) {
    if (!runCtx || runCtx.finished) return;
    const traceEl = runCtx.el.querySelector('.bubble-trace');
    if (!traceEl) return;
    const row = document.createElement('div');
    row.className = 'trace-row live';
    const line = describeTraceLine(kind, payload);
    if (!line) return;
    row.innerHTML = `<span class="activity-chip ${ACTIVITY_COLORS[line.cls] || 'state'}">${escapeHtml(line.chip)}</span><span class="trace-text">${escapeHtml(line.text)}</span>`;
    // Only the newest row stays highlighted.
    const liveRows = traceEl.querySelectorAll('.trace-row.live');
    for (let i = 0; i < liveRows.length - 1; i += 1) liveRows[i].classList.remove('live');
    traceEl.appendChild(row);
    traceEl.scrollTop = traceEl.scrollHeight;
    scrollChat();
  }

  /** Map a trajectory/WS event to a human stage line. */
  function describeTraceLine(kind, payload) {
    const k = typeof kind === 'string'
      ? kind.replace(/_([a-z])/g, (_, c) => c.toUpperCase()).replace(/^[a-z]/, c => c.toUpperCase())
      : String(kind);
    switch (k) {
      case 'LlmCallStarted': {
        if (runCtx.sawRender) {
          const scoped = runCtx.scoped;
          return scoped
            ? { cls: 'patch', chip: 'patch', text: `✂ Patching… 第 ${runCtx.round} 轮：查看渲染图并修改选中的 ${runCtx.cellCount} 个 cell` }
            : { cls: 'review', chip: 'refine', text: `🧠 Refining… 第 ${runCtx.round} 轮：查看渲染图并修改` };
        }
        return { cls: 'llm', chip: 'draw', text: '🎨 Drawing… 生成/思考 XML' };
      }
      case 'LlmCallCompleted': {
        const secs = ((payload.duration_ms || 0) / 1000).toFixed(1);
        return { cls: 'llm', chip: 'done', text: `✓ 模型返回 · ${payload.input_tokens ?? '?'}+${payload.output_tokens ?? '?'} tok · ${secs}s${payload.finish_reason ? ` · ${payload.finish_reason}` : ''}` };
      }
      case 'RenderStarted': {
        runCtx.round += 1;
        runCtx.sawRender = true;
        return { cls: 'render', chip: 'view', text: `👁 Viewing… 渲染第 ${runCtx.round} 张图` };
      }
      case 'RenderCompleted': {
        const secs = ((payload.duration_ms || 0) / 1000).toFixed(1);
        return { cls: 'render', chip: 'done', text: `✓ 渲染完成 · ${((payload.bytes || 0) / 1024).toFixed(1)} KB · ${secs}s` };
      }
      case 'Error': {
        const msg = String(payload.message || 'unknown').slice(0, 180);
        return { cls: 'error', chip: 'error', text: `⚠ ${payload.stage || 'server'}: ${msg}` };
      }
      case 'StateTransition': {
        const from = payload.from ? `${payload.from} → ` : '';
        return { cls: 'state', chip: 'v', text: `${from}${payload.to}` };
      }
      default:
        return null;
    }
  }

  // -------------------------------------------------------------------------
  // Conversation bubbles (chat skeleton)
  // -------------------------------------------------------------------------

  const KIND_LABEL = {
    generate: '⚡ generate',
    patch: '✂ patch',
    'agent-loop': '🔄 refine',
    error: '⚠ error',
    system: 'ℹ',
  };

  function clearChat() {
    chatThread.innerHTML = '';
    runCtx = null;
  }

  function scrollChat() {
    chatThread.scrollTop = chatThread.scrollHeight;
  }

  /** Append a user bubble. `chipLabel` is shown as an inline reference
   *  chip when the message carried a canvas selection. */
  function addUserBubble(text, chipLabel) {
    const el = document.createElement('div');
    el.className = 'bubble user';
    const kind = document.createElement('div');
    kind.className = 'bubble-kind';
    kind.textContent = 'you';
    el.appendChild(kind);
    const body = document.createElement('div');
    body.className = 'bubble-text';
    body.textContent = text;
    el.appendChild(body);
    if (chipLabel) {
      const chip = document.createElement('span');
      chip.className = 'inline-chip';
      chip.textContent = chipLabel;
      el.appendChild(chip);
    }
    chatThread.appendChild(el);
    scrollChat();
  }

  /** Append (or resolve) an agent bubble. A bubble is
   *  [kind row][main content][live trace]; resolving a pending bubble keeps
   *  its trace (the run's trajectory) and swaps the main content. */
  let pendingTimer = null;

  function addAgentBubble(kind, text, opts = {}) {
    if (opts.pendingEl && pendingTimer) {
      clearInterval(pendingTimer);
      pendingTimer = null;
    }
    const el = opts.pendingEl || document.createElement('div');
    el.className = `bubble agent${opts.isError ? ' error' : ''}${opts.pending ? ' pending' : ''}`;
    let kindEl = el.querySelector ? el.querySelector('.bubble-kind') : null;
    let mainEl = el.querySelector ? el.querySelector('.bubble-main') : null;
    let traceEl = el.querySelector ? el.querySelector('.bubble-trace') : null;
    if (!kindEl) {
      kindEl = document.createElement('div');
      kindEl.className = 'bubble-kind';
      el.appendChild(kindEl);
      mainEl = document.createElement('div');
      mainEl.className = 'bubble-main';
      el.appendChild(mainEl);
      traceEl = document.createElement('div');
      traceEl.className = 'bubble-trace';
      el.appendChild(traceEl);
    }
    kindEl.textContent = KIND_LABEL[kind] || kind;
    mainEl.innerHTML = '';
    if (opts.pending) {
      const row = document.createElement('div');
      row.className = 'bubble-main-row';
      const spinner = document.createElement('span');
      spinner.className = 'spinner';
      row.appendChild(spinner);
      const txt = document.createElement('span');
      txt.className = 'bubble-text';
      txt.textContent = text;
      row.appendChild(txt);
      mainEl.appendChild(row);
      const wait = document.createElement('span');
      wait.className = 'bubble-meta';
      mainEl.appendChild(wait);
      const t0 = Date.now();
      pendingTimer = setInterval(() => {
        const secs = Math.round((Date.now() - t0) / 1000);
        wait.textContent = `⏱ ${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
      }, 1000);
      chatThread.appendChild(el);
      scrollChat();
      return el;
    }
    const body = document.createElement('div');
    body.className = 'bubble-text';
    body.textContent = text;
    mainEl.appendChild(body);
    if (opts.reasoning) {
      const r = document.createElement('blockquote');
      r.className = 'bubble-reason';
      r.textContent = opts.reasoning;
      mainEl.appendChild(r);
    }
    if (opts.meta) {
      const m = document.createElement('div');
      m.className = 'bubble-meta';
      m.textContent = opts.meta;
      mainEl.appendChild(m);
    }
    if (opts.pendingEl) {
      el.classList.remove('pending');
    } else {
      chatThread.appendChild(el);
    }
    scrollChat();
    return el;
  }

  function runningBubble(text) {
    return addAgentBubble('system', text, { pending: true });
  }

  function handleWsMessage(data) {
    if (!data || typeof data !== 'object') return;
    if (data.type === 'trajectory' && data.event) {
      pushTrace(data.event.kind, data.event);
    } else if (data.type === 'version_created') {
      pushTrace('StateTransition', { from: '', to: `📌 version ${data.version_id}` });
    } else if (data.type === 'error') {
      pushTrace('Error', { stage: 'server', message: data.message || 'unknown' });
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
    ws.addEventListener('close', ev => {
      console.warn(`[drawio] WS closed: code=${ev.code} reason=${ev.reason || '(none)'}`);
      scheduleReconnect(id);
    });
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

      // The mock renderer returns a 1×1 white placeholder (69 bytes). Don't
      // hand the user a blank PNG — tell them how to get a real render.
      if (bytes.length < 500) {
        showError(`The server returned a ${bytes.length}-byte placeholder PNG — it is running with the mock renderer. Set DRAWIO_AGENT_RENDERER=chromium for real renders.`);
        return;
      }
      clearError();

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

  function sendRoute(cellIds) {
    // Intent routing: what does the user mean, not which toggle is on.
    //   with selection      -> agent-loop + patch_cell_ids (visual scoped fix)
    //   has diagram         -> agent-loop (modify the current diagram)
    //   empty session, deep -> agent-loop without initial (generate + self-review)
    //   empty session, fast -> /generate (single shot)
    //   forceRegen          -> /generate (from scratch, replaces canvas)
    if (cellIds.length) return { endpoint: '/agent-loop', scope: cellIds };
    if (currentXml && currentXml.trim()) return { endpoint: '/agent-loop' };
    if (currentDepth === 'refine') return { endpoint: '/agent-loop' };
    return { endpoint: '/generate' };
  }

  async function runSend() {
    if (!canSend()) return;
    await sendMessage({ forceRegen: false });
  }

  /** The single composer path. `forceRegen` = explicit "from scratch"
   *  (used by the ↺ button); everything else follows intent routing. */
  async function sendMessage({ forceRegen }) {
    const prompt = promptEl.value.trim();
    const cellIds = currentSelection.slice();
    clearError();

    const route = forceRegen ? { endpoint: '/generate' } : sendRoute(cellIds);
    const isScoped = !!route.scope;
    const isModify = route.endpoint === '/agent-loop' && currentXml && currentXml.trim() && !isScoped;
    const isRegen = forceRegen || (route.endpoint === '/generate' && !isScoped);
    const maxIter = currentDepth === 'refine' ? undefined : 1; // deep=default(3), fast=1 round
    const loadingText = isScoped ? 'patching selected cells…' : (isRegen ? 'generating…' : (currentXml && currentXml.trim() ? 'refining current diagram…' : 'generating draft + self-review…'));

    setRunLoading(true, loadingText);
    setLoading(true, loadingText);
    const chipLabel = cellIds.length ? `◎ ${cellIds.length} selected` : null;
    addUserBubble(prompt, chipLabel);
    const pendingBubble = runningBubble(loadingText);
    // Trace WS trajectory events into this bubble until the run resolves.
    runCtx = { el: pendingBubble, scoped: isScoped, cellCount: cellIds.length, sawRender: false, round: 0, finished: false };
    promptEl.value = '';
    if (cellIds.length && currentGraph) currentGraph.clearSelection(); // clears currentSelection via listener
    currentSelection = [];
    renderAttachments();
    updateSendButton();
    updateRefineHint();

    try {
      const body = { prompt };
      if (route.endpoint === '/agent-loop') {
        body.max_iterations = maxIter;
        if (currentXml && currentXml.trim()) body.initial_xml = currentXml;
        if (isScoped) body.patch_cell_ids = cellIds;
      }
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}${route.endpoint}`, body);

      if (result && result.xml) {
        const noVisualChange = result.xml === currentXml;
        loadXmlIntoCanvas(result.xml);
        if (noVisualChange) {
          addActivity('StateTransition', { from: route.endpoint, to: 'no visual change (LLM returned the same XML)' });
        }
      }

      // Agent bubble summary.
      const depthNote = currentDepth === 'refine' ? '深度' : '快速';
      if (isScoped) {
        const converged = !!result.converged;
        addAgentBubble('patch', converged
          ? `已修改选中的 ${cellIds.length} 个 cell（未选中内容保持原样）· ${depthNote}`
          : `${result.iterations || 1} 轮后未完全收敛，已保留最佳结果（未选中内容保持原样）`, {
          pendingEl: pendingBubble,
          reasoning: (result.last_reasoning ? truncateForBubble(result.last_reasoning, 320) : undefined),
          meta: `v:${(result.version_id || '').slice(0, 8)} · iterations:${result.iterations}`,
        });
      } else if (isRegen) {
        addAgentBubble('generate', '已从头生成新图并载入画布', {
          pendingEl: pendingBubble,
          meta: `v:${(result.version_id || '').slice(0, 8)}`,
        });
      } else {
        const converged = !!result.converged;
        const note = result.last_reasoning ? truncateForBubble(result.last_reasoning, 320) : '';
        addAgentBubble('agent-loop', converged
          ? `✓ 完成 · ${result.iterations} 轮${depthNote}自省收敛`
          : `⚠ ${result.iterations} 轮后未收敛（已保留当前最优结果）`, {
          pendingEl: pendingBubble,
          reasoning: note || undefined,
          meta: result.last_verdict ? `verdict: ${result.last_verdict}` : undefined,
        });
      }
      await loadSessionList();
    } catch (err) {
      const labelName = isScoped ? '局部修改' : (isRegen ? '生成' : '深度精修');
      showError(`${labelName} 失败：${err.message}`);
      addActivity('Error', { stage: labelName, message: err.message });
      addAgentBubble('error', `${labelName} 失败：${err.message}`, {
        pendingEl: pendingBubble,
        isError: true,
      });
    } finally {
      if (runCtx) runCtx.finished = true;
      runCtx = null;
      setRunLoading(false);
      setLoading(false);
    }
  }

  function truncateForBubble(text, maxChars) {
    if (!text) return '';
    const t = String(text);
    return t.length > maxChars ? `${t.slice(0, maxChars)}…` : t;
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
    sendBtn.addEventListener('click', runSend);
    thinkingOptions.forEach(opt => {
      opt.addEventListener('click', () => setDepth(opt.dataset.depth));
    });
    refineHintCta.addEventListener('click', () => {
      setDepth('fast');
      promptEl.focus();
    });
    btnExportPng.addEventListener('click', exportPng);
    downloadSvgBtn.addEventListener('click', downloadSvg);
    copyXmlUrlBtn.addEventListener('click', copyXmlUrl);
    regenBtn.addEventListener('click', async () => {
      if (!currentSessionId || !promptEl.value.trim()) {
        showError('先输入描述，再点「从头画」会忽略当前画布直接生成新图。');
        promptEl.focus();
        return;
      }
      if (!confirm('从头画会忽略当前画布内容生成一张全新图（历史版本仍保留）。继续？')) return;
      await sendMessage({ forceRegen: true });
    });

    promptEl.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        runSend();
      }
    });
    promptEl.addEventListener('input', () => { updateSendButton(); updateRefineHint(); });

    canvasToolPan.addEventListener('click', () => setCanvasMode('pan'));
    canvasToolSelect.addEventListener('click', () => setCanvasMode('select'));

    // Mode-aware canvas interaction. Pan mode = pure navigation (no selection);
    // select mode = click a cell to select, drag on empty canvas to marquee.
    // The container element survives graph rebuilds, so one listener is enough.
    drawioContainer.addEventListener('pointerdown', (e) => {
      if (e.button !== 0 || !currentGraph) return;
      const graphPt = mxUtils.convertPoint(currentGraph.container, e.clientX, e.clientY);
      // Try the bundle's hit-test first; fall back to a bbox walk for
      // white-fill cells and nested groups that getCellAt misses.
      let cell = currentGraph.getCellAt(graphPt.x, graphPt.y);
      if (!cell) cell = getCellAtBbox(currentGraph, graphPt.x, graphPt.y);

      if (canvasMode === 'pan') {
        // Pure navigation — setCellsSelectable(false) already prevents any
        // selection side-effect; just let mxGraph pan.
        return;
      }

      // canvasMode === 'select' from here.
      if (cell) {
        // Click landed on a cell → single-select it directly (not through
        // mxGraph's click handler, which the fork's stubs only partially fix).
        currentGraph.setSelectionCell(cell);
        e.preventDefault();
        return;
      }
      // Empty area → start marquee.
      if (startRubberBand(e.clientX, e.clientY, null)) {
        e.preventDefault();
        const onMove = (ev) => updateRubberBand(ev.clientX, ev.clientY);
        const onUp = () => {
          window.removeEventListener('pointermove', onMove);
          window.removeEventListener('pointerup', onUp);
          endRubberBand();
        };
        window.addEventListener('pointermove', onMove);
        window.addEventListener('pointerup', onUp);
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
    updateRefineHint();
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
