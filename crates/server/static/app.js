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
  const activityLog = $('activity-log');
  const chatThread = $('chat-thread');
  const promptAttachments = $('prompt-attachments');
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
  let activityEntries = [];
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

  function summarizeEvent(kind, payload) {
    // TrajectoryEvent kinds arrive over WS snake_cased ("llm_call_started");
    // normalize to the camel-case labels used by the cases below.
    const k = typeof kind === 'string'
      ? kind.replace(/_([a-z])/g, (_, c) => c.toUpperCase()).replace(/^[a-z]/, c => c.toUpperCase())
      : kind;
    switch (k) {
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

  /** Append (or resolve) an agent bubble. Pass an existing element as
   *  `pendingEl` to turn a running placeholder into the final message. */
  function addAgentBubble(kind, text, opts = {}) {
    const el = opts.pendingEl || document.createElement('div');
    el.className = `bubble agent${opts.isError ? ' error' : ''}${opts.pending ? ' pending' : ''}`;
    el.innerHTML = '';
    const kindEl = document.createElement('div');
    kindEl.className = 'bubble-kind';
    kindEl.textContent = KIND_LABEL[kind] || kind;
    el.appendChild(kindEl);
    if (opts.pending) {
      const spinner = document.createElement('span');
      spinner.className = 'spinner';
      el.appendChild(spinner);
      const txt = document.createElement('span');
      txt.className = 'bubble-text';
      txt.textContent = text;
      el.appendChild(txt);
      chatThread.appendChild(el);
      scrollChat();
      return el;
    }
    const body = document.createElement('div');
    body.className = 'bubble-text';
    body.textContent = text;
    el.appendChild(body);
    if (opts.reasoning) {
      const r = document.createElement('blockquote');
      r.className = 'bubble-reason';
      r.textContent = opts.reasoning;
      el.appendChild(r);
    }
    if (opts.meta) {
      const m = document.createElement('div');
      m.className = 'bubble-meta';
      m.textContent = opts.meta;
      el.appendChild(m);
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

  async function runSend() {
    if (!canSend()) return;
    const prompt = promptEl.value.trim();
    const cellIds = currentSelection.slice();
    clearError();
    if (currentDepth === 'refine') clearActivity();

    // One composer, three backends (until they converge on a chat turn API):
    //   with a selection reference  -> /patch      (scope: ONLY these cells)
    //   no reference + refine depth -> /agent-loop (visual self-review loop)
    //   no reference + fast depth   -> /generate   (single shot from scratch)
    const isPatch = cellIds.length > 0;
    const endpoint = isPatch ? '/patch' : (currentDepth === 'refine' ? '/agent-loop' : '/generate');
    const statusText = isPatch ? '✂ Patching selected…' : (currentDepth === 'refine' ? '🔄 Refining…' : '⚡ Generating…');
    const loadingText = isPatch ? 'patching selected cells…' : (currentDepth === 'refine' ? 'refining…' : 'generating…');

    setRunLoading(true, statusText);
    setLoading(true, loadingText);
    const chipLabel = cellIds.length ? `◎ ${cellIds.length} selected` : null;
    addUserBubble(prompt, chipLabel);
    const pendingBubble = runningBubble(loadingText);
    promptEl.value = '';
    if (cellIds.length && currentGraph) currentGraph.clearSelection(); // clears currentSelection via listener
    currentSelection = [];
    renderAttachments();
    updateSendButton();
    updateRefineHint();

    try {
      const body = { prompt };
      if (isPatch) {
        body.cell_ids = cellIds;
        body.instruction = prompt;
        delete body.prompt;
      } else if (currentDepth === 'refine' && currentXml) {
        body.initial_xml = currentXml;
      }
      const result = await api('POST', `/api/sessions/${encodeURIComponent(currentSessionId)}${endpoint}`, body);

      if (result && result.xml) {
        const noVisualChange = result.xml === currentXml;
        loadXmlIntoCanvas(result.xml);
        if (noVisualChange) {
          addActivity('StateTransition', { from: endpoint, to: 'no visual change (LLM returned the same XML)' });
        }
      }

      // Agent bubble summary.
      if (isPatch) {
        addAgentBubble('patch', `已修改选中的 ${cellIds.length} 个 cell（未选中的内容保持原样）`, {
          pendingEl: pendingBubble,
          meta: `v:${(result.version_id || '').slice(0, 8)}`,
        });
      } else if (currentDepth === 'refine') {
        const converged = !!result.converged;
        const note = result.last_reasoning ? truncateForBubble(result.last_reasoning, 320) : '';
        addAgentBubble('agent-loop', converged
          ? `✓ 完成 · ${result.iterations} 轮收敛`
          : `⚠ ${result.iterations} 轮后未收敛（已保留当前最优结果）`, {
          pendingEl: pendingBubble,
          reasoning: note || undefined,
          meta: result.last_verdict ? `verdict: ${result.last_verdict}` : undefined,
        });
      } else {
        addAgentBubble('generate', '已生成新图并载入画布', {
          pendingEl: pendingBubble,
          meta: `v:${(result.version_id || '').slice(0, 8)}`,
        });
      }
      await loadSessionList();
    } catch (err) {
      const label = isPatch ? 'Patch' : (currentDepth === 'refine' ? 'Refine' : 'Generate');
      showError(`${label} failed: ${err.message}`);
      addActivity('Error', { stage: currentDepth, message: err.message });
      addAgentBubble('error', `${label} 失败：${err.message}`, {
        pendingEl: pendingBubble,
        isError: true,
      });
    } finally {
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
