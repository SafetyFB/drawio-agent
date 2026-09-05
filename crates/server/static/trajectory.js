(() => {
  const EVENT_COLORS = {
    LlmCallStarted: '#0a7cb9', LlmCallCompleted: '#06b6d4',
    RenderStarted: '#7c3aed', RenderCompleted: '#c026d3',
    Error: '#ef4444', StateTransition: '#f59e0b',
  };
  const inputs = [document.getElementById('session-input'), document.getElementById('empty-session-input')];
  const connectBtns = [document.getElementById('connect-btn'), document.getElementById('empty-connect-btn')];
  const statusEl = document.getElementById('status');
  const emptyState = document.getElementById('empty-state');
  const timeline = document.getElementById('timeline');
  const bindAddr = document.getElementById('bind-addr');

  let ws = null, reconnectTimer = null, reconnectAttempt = 0, currentSession = null;
  let events = [], seenSignatures = new Set(), timeUpdater = null;

  function setStatus(state, detail = '') {
    const map = {
      disconnected: { cls: 'disconnected', text: 'disconnected' },
      connecting: { cls: 'connecting', text: `connecting${detail}` },
      live: { cls: 'live', text: 'live' },
      closed: { cls: 'closed', text: `closed${detail}` },
    };
    const s = map[state] || map.disconnected;
    statusEl.className = `status ${s.cls}`;
    statusEl.querySelector('.status-text').textContent = s.text;
    statusEl.setAttribute('aria-label', s.text);
  }

  function hashSession() {
    const m = location.hash.match(/^#\/session\/(.+)$/);
    return m ? decodeURIComponent(m[1]) : '';
  }
  function setHashSession(id) {
    if (!id) { history.replaceState(null, '', location.pathname + location.search); return; }
    const hash = `#/session/${encodeURIComponent(id)}`;
    if (location.hash !== hash) location.hash = hash;
  }
  function updateInputs(id) { inputs.forEach(i => { if (i) i.value = id; }); }

  function signature(event) {
    // `seq` is monotonic per-session (set by the trajectory store on the
    // server). Two events with the same seq are the same event — never
    // re-add. Fall back to kind+payload if seq is missing (synthetic
    // version_created / error events synthesised by this client).
    if (event.seq != null) return `seq:${event.seq}`;
    try { return `synthetic:${event.kind}|${JSON.stringify(event)}`; }
    catch { return `synthetic:${event.kind}|${String(event)}`; }
  }

  function summarize(kind, payload) {
    switch (kind) {
      case 'LlmCallStarted': return `prompt ${payload.prompt_chars ?? '?'} chars · ${payload.json_mode ? 'JSON mode' : 'text mode'}`;
      case 'LlmCallCompleted': return `${payload.input_tokens ?? 0}+${payload.output_tokens ?? 0} tok · ${(payload.duration_ms / 1000).toFixed(1)}s · ${payload.finish_reason ?? 'no reason'}`;
      case 'RenderStarted': return `scale ${payload.scale ?? '?'}`;
      case 'RenderCompleted': return `${payload.bytes ?? 0} bytes · ${(payload.duration_ms / 1000).toFixed(1)}s`;
      case 'Error': return `${payload.stage ?? 'unknown'} · ${payload.message ?? ''}`;
      case 'StateTransition': return `${payload.from ?? '∅'} → ${payload.to ?? '?'}`;
      default: return '';
    }
  }

  function formatRelative(iso) {
    const d = new Date(iso);
    const secs = Math.round((Date.now() - d.getTime()) / 1000);
    if (secs < 1) return 'now';
    const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
    if (secs < 60) return rtf.format(-secs, 'second');
    const mins = Math.round(secs / 60);
    if (mins < 60) return rtf.format(-mins, 'minute');
    const hours = Math.round(mins / 60);
    if (hours < 24) return rtf.format(-hours, 'hour');
    return rtf.format(-Math.round(hours / 24), 'day');
  }

  function escapeHtml(str) {
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#039;');
  }

  function createEventNode(event) {
    const kind = event.kind || 'Unknown';
    const payload = { ...event };
    // Strip envelope metadata before pretty-printing the payload.
    ['kind','id','seq','at','at_ms','session_id'].forEach(k => delete payload[k]);

    const node = document.createElement('article');
    node.className = 'event';
    node.style.setProperty('--event-color', EVENT_COLORS[kind] || '#9aa0a6');

    // Prefer at_ms (number) from the WS payload; fall back to REST's `at`
    // (whatever shape SystemTime serialises to); else now.
    let atIso;
    if (event.at_ms != null) atIso = new Date(event.at_ms).toISOString();
    else if (event.at) atIso = new Date(event.at).toISOString();
    else atIso = new Date().toISOString();
    const row = document.createElement('div');
    row.className = 'event-row';
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    row.setAttribute('aria-expanded', 'false');
    row.innerHTML = `<time class="event-time" datetime="${atIso}" title="${new Date(atIso).toLocaleString()}">${formatRelative(atIso)}</time><span class="event-chip">${kind}</span><span class="event-summary">${summarize(kind, event)}</span>`;

    const detail = document.createElement('div');
    detail.className = 'event-detail';
    detail.innerHTML = `<pre><code>${escapeHtml(JSON.stringify(event, null, 2))}</code></pre>`;

    node.appendChild(row);
    node.appendChild(detail);

    const toggle = () => { const expanded = node.classList.toggle('expanded'); row.setAttribute('aria-expanded', String(expanded)); };
    row.addEventListener('click', toggle);
    row.addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(); } });
    return node;
  }

  function render() {
    timeline.innerHTML = '';
    // Sort by `seq` descending — deterministic regardless of arrival order
    // (REST history + WS live stream can interleave during reconnect).
    [...events].sort((a, b) => (b.seq ?? -1) - (a.seq ?? -1)).forEach(e => timeline.appendChild(createEventNode(e)));
  }

  function addEvent(event) {
    const sig = signature(event);
    if (seenSignatures.has(sig)) return;
    seenSignatures.add(sig);
    events.push(event);
    timeline.insertBefore(createEventNode(event), timeline.firstChild);
  }

  function updateTimes() {
    document.querySelectorAll('.event-time').forEach(time => {
      time.textContent = formatRelative(time.getAttribute('datetime'));
    });
  }

  function clearSession() {
    if (ws) { ws.close(); ws = null; }
    clearTimeout(reconnectTimer); reconnectTimer = null; reconnectAttempt = 0;
    events = []; seenSignatures.clear();
    timeline.innerHTML = ''; timeline.classList.remove('active');
    emptyState.style.display = 'flex';
    setStatus('disconnected');
    currentSession = null;
  }

  async function loadHistory(sessionId) {
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/trajectory`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      if (!Array.isArray(data)) throw new Error('expected array');
      data.forEach(e => { if (e && e.kind) addEvent(e); });
      render();
    } catch (err) { console.warn('failed to load history:', err); }
  }

  function connect(sessionId) {
    if (!sessionId) return;
    if (currentSession === sessionId && ws?.readyState === WebSocket.OPEN) return;
    clearSession();
    currentSession = sessionId;
    updateInputs(sessionId);
    setHashSession(sessionId);
    emptyState.style.display = 'none';
    timeline.classList.add('active');
    setStatus('connecting', '...');
    loadHistory(sessionId);
    openSocket(sessionId);
  }

  function openSocket(sessionId) {
    if (ws) ws.close();
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const host = location.host || '127.0.0.1:8080';
    const url = `${proto}//${host}/api/sessions/${encodeURIComponent(sessionId)}/events`;
    ws = new WebSocket(url);

    ws.addEventListener('open', () => { reconnectAttempt = 0; setStatus('live'); });
    ws.addEventListener('message', event => {
      try {
        const msg = JSON.parse(event.data);
        if (!msg || typeof msg !== 'object') return;
        if (msg.type === 'trajectory' && msg.event && typeof msg.event === 'object') {
          // Flatten seq/at_ms from the envelope into the event record so the
          // dedup signature and timeline ordering have them in one place.
          addEvent({ ...msg.event, seq: msg.seq, at_ms: msg.at_ms });
        } else if (msg.type === 'version_created') {
          addEvent({ kind: 'StateTransition', from: null, to: `version ${msg.version_id}`, seq: null, at_ms: Date.now() });
        } else if (msg.type === 'error') {
          addEvent({ kind: 'Error', stage: 'server', message: msg.message || 'unknown error', seq: null, at_ms: Date.now() });
        }
      } catch (err) { console.warn('malformed WS message:', err); }
    });
    ws.addEventListener('close', () => scheduleReconnect(sessionId));
    ws.addEventListener('error', err => { console.warn('ws error:', err); setStatus('closed', ' (error)'); });
  }

  function scheduleReconnect(sessionId) {
    if (!currentSession || currentSession !== sessionId) return;
    const delay = Math.min(1000 * 2 ** reconnectAttempt, 30000);
    reconnectAttempt += 1;
    setStatus('connecting', ` · ${reconnectAttempt} (${delay / 1000}s)`);
    reconnectTimer = setTimeout(() => openSocket(sessionId), delay);
  }

  function init() {
    bindAddr.textContent = location.host || '127.0.0.1:8080';
    connectBtns.forEach((btn, idx) => {
      if (!btn) return;
      btn.addEventListener('click', () => { const id = inputs[idx]?.value.trim(); if (id) connect(id); });
    });
    inputs.forEach(input => {
      if (!input) return;
      input.addEventListener('keydown', e => { if (e.key === 'Enter') { const id = input.value.trim(); if (id) connect(id); } });
    });
    window.addEventListener('hashchange', () => {
      const id = hashSession();
      updateInputs(id);
      if (id) connect(id); else clearSession();
    });
    timeUpdater = setInterval(updateTimes, 5000);
    const id = hashSession();
    if (id) connect(id);
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
