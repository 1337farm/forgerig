// app.js — browser UI for the ForgeRig daemon (tabs, message list, bottom
// composer, markdown rendering, forking). Assumes `Markdown` and `ChatState`
// globals are already loaded.

(function () {
  'use strict';

  // ---------- DOM refs ----------
  var $ = function (id) { return document.getElementById(id); };
  var sessionsEl = $('tab-list');
  var messagesEl = $('messages');
  var composerEl = $('composer');
  var sendBtn = $('send-btn');
  var newBtn = $('new-session');
  var historyBtn = $('history-btn');
  var closedPanel = $('closed-panel');
  var closedList = $('closed-list');
  var statusEl = $('status');
  var providerEl = $('provider');
  var leanStatusEl = $('lean-status');
  var leanBtnEl = $('lean-btn');
  var leanBarEl = $('lean-bar');
  var leanFillEl = $('lean-fill');
  var leanPctEl = $('lean-pct');
var leanOverlayEl = $('lean-overlay');
  var leanOverlayDetailEl = $('lean-overlay-detail');
  var leanOverlayFillEl = $('lean-overlay-fill');
  var leanOverlayPctEl = $('lean-overlay-pct');
  var tabContainerEl = $('tabs');
  var branchPagerEl = $('branch-pager');
  var messageContainerEl = $('messages');
  var footerContainerEl = typeof document.querySelector === 'function' ? document.querySelector('footer') : null;

  function setChatAreaVisible(visible) {
    if (tabContainerEl) tabContainerEl.style.display = visible ? 'block' : 'none';
    if (branchPagerEl) branchPagerEl.style.display = visible ? 'block' : 'none';
    if (messageContainerEl) messageContainerEl.style.display = visible ? 'block' : 'none';
    if (footerContainerEl) footerContainerEl.style.display = visible ? 'block' : 'none';
  }

  // ---------- RPC client (id-keyed, supports concurrency) ----------
  var ws = null;
  var reqId = 0;
  var pending = {};
  var POLL_MS = 500;
  var leanTimer = null;

  function call(method, params, cb) {
    if (!ws || ws.readyState !== 1) {
      if (cb) cb(new Error('not connected'));
      return null;
    }
    var id = ++reqId;
    pending[id] = cb || function () {};
    ws.send(JSON.stringify({ jsonrpc: '2.0', method: method, params: params || {}, id: id }));
    return id;
  }

  function paintHeader(state) {
    // state: connecting | live | working | error | offline
    var pill = $('status-pill');
    var dot = $('conn-dot');
    var label = state === 'working' ? 'Agent working…' :
      state === 'live' ? 'Ready' :
      state === 'error' ? 'Error' :
      state === 'offline' ? 'Offline' : 'Connecting…';
    if (pill) pill.textContent = label;
    if (dot) dot.className = 'dot' + (state === 'working' ? ' busy' : state === 'error' || state === 'offline' ? ' error' : '');
    statusEl.textContent = state === 'live' || state === 'working' ? 'Connected' : label;
  }

  function paintProvider(raw) {
    providerEl.textContent = raw || '';
    var key = $('key-pill');
    var lean = $('lean-pill');
    if (key) {
      var missing = /key=missing/.test(raw || '');
      key.style.display = '';
      key.textContent = missing ? 'key missing' : 'key set';
      key.className = 'pill ' + (missing ? 'key-missing' : 'key-ok');
    }
    if (lean) {
      var lt = (leanStatusEl && leanStatusEl.textContent) || '';
      var ready = /ready/i.test(lt);
      lean.style.display = '';
      lean.textContent = ready ? lt.replace(/^Lean:\s*/, '') : 'lean?';
      lean.className = 'pill ' + (ready ? 'lean-ok' : 'lean-warn');
    }
  }

  function connect() {
    paintHeader('connecting');
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/');
    ws.onopen = function () {
      reconnectCount = 0;
      paintHeader('live');
      call('status', {}, function (err, r) {
        if (err || !r || !r.provider) return;
        paintProvider(r.provider);
        if (/key=missing/.test(r.provider)) {
          var notice = $('setup-notice');
          var text = $('setup-notice-text');
          if (notice && text) {
            text.textContent = 'No API key configured — open Settings to choose a provider, model, and token budget before sending.';
            notice.style.display = 'block';
          }
        }
      });
      refreshLean();
      loadSessions();
    };
    ws.onclose = function () {
      paintHeader('offline');
      // GateKeeper-style stream resume: keep in-flight thinking bubbles and
      // queued turns, back off, then reopen the single channel and re-sync
      // the active session. No composer clobber, no queue loss.
      statusEl.textContent = 'Disconnected — retrying…';
      var n = (reconnectCount || 0) + 1;
      reconnectCount = n;
      var wait = Math.min(15000, 500 * Math.pow(2, n - 1));
      setTimeout(connect, wait);
    };
    ws.onmessage = function (e) {
      var d;
      try { d = JSON.parse(e.data); } catch (_) { return; }
      if (d.id == null && d.method) { handlePush(d); return; }
      if (d.id != null && pending[d.id]) {
        var cb = pending[d.id];
        delete pending[d.id];
        cb(d.error, d.result);
      }
    };
  }

  // Server-initiated streaming notifications (no request id).
  function handlePush(d) {
    var p = d.params || {};
    var sid = p.session_id;
    if (!sid) return;
    if (d.method === 'chat_chunk') onStreamChunk(sid, p.delta || '');
    else if (d.method === 'chat_phase') onStreamPhase(sid, p.phase || 'thinking');
    else if (d.method === 'chat_tool') onStreamTool(sid, p);
    else if (d.method === 'chat_done') onStreamDone(sid, p.reply || '');
    else if (d.method === 'chat_stopped') onStreamStopped(sid);
    else if (d.method === 'chat_error') {
      paintHeader('error');
      failFlight(sid, (p.error && (p.error.message || p.error)) || p.error || 'unknown error');
    }
  }

  function childByClass(el, cls) {
    var kids = el.children || [];
    for (var i = 0; i < kids.length; i++) {
      if (kids[i].className === cls) return kids[i];
    }
    return null;
  }

  // Bubble for live tokens: morphs the Thinking bubble when attached,
  // otherwise builds (or rebuilds, after a re-render) a fresh one.
  function streamBubble(sid) {
    var refs = streamEls[sid];
    if (refs && refs.wrap.parentNode) return refs;
    var pw = pendEl[sid];
    if (pw && pw.parentNode) {
      delete pendEl[sid];
      var b = childByClass(pw, 'body');
      var t = childByClass(pw, 'tools');
      if (!t) {
        t = document.createElement('div');
        t.className = 'tools';
        pw.appendChild(t);
      }
      refs = { wrap: pw, body: b, tools: t };
    } else {
      delete pendEl[sid];
      var wrap = document.createElement('div');
      wrap.className = 'msg assistant';
      var head = document.createElement('div');
      head.className = 'author';
      head.textContent = 'Assistant';
      var body = document.createElement('div');
      body.className = 'body';
      var tools = document.createElement('div');
      tools.className = 'tools';
      wrap.appendChild(head);
      wrap.appendChild(body);
      wrap.appendChild(tools);
      appendElement(wrap);
      refs = { wrap: wrap, body: body, tools: tools };
    }
    streamEls[sid] = refs;
    return refs;
  }

  // Last phase label per session, so streamed chunks can re-render the
  // pinned header (label + triple-dot animation) above the tokens without
  // needing DOM queries the test stub does not implement.
  var phaseLabel = {}; // sid -> 'Thinking' | 'Reviewing' | 'Researching' | 'Ready'

  function phaseHeaderHtml(sid) {
    var label = phaseLabel[sid] || 'Thinking';
    return '<span class="phase">' + label +
      '<span class="typing"><span></span><span></span><span></span></span></span>';
  }

  function onStreamPhase(sid, phase) {
    if (stoppedSid[sid]) return;
    phaseLabel[sid] = phase === 'reviewing' ? 'Reviewing' :
      phase === 'researching' ? 'Researching' :
      phase === 'ready' ? 'Ready' : 'Thinking';
    if (activeId !== sid) {
      if (!tabFlag[sid]) tabFlag[sid] = 'unread';
      renderTabs();
      return;
    }
    var refs = streamBubble(sid);
    if (!refs || !refs.body) return;
    // Keep the phase label pinned above the streamed tokens with the
    // triple-dot pending animation the whole time tokens flow.
    var text = streamBuf[sid] ? Markdown.render(streamBuf[sid]) : '';
    refs.body.innerHTML = phaseHeaderHtml(sid) +
      (text ? '<div class="stream-text">' + text + '</div>' : '');
    scrollToBottom();
  }

  function onStreamChunk(sid, delta) {
    if (stoppedSid[sid]) return;
    streamBuf[sid] = (streamBuf[sid] || '') + delta;
    if (activeId !== sid) {
      if (!tabFlag[sid]) tabFlag[sid] = 'unread';
      renderTabs();
      return;
    }
    var refs = streamBubble(sid);
    // Preserve the pinned phase header while tokens stream underneath it.
    if (refs.body) {
      refs.body.innerHTML = phaseHeaderHtml(sid) +
        '<div class="stream-text">' + Markdown.render(streamBuf[sid]) + '</div>';
    }
    scrollToBottom();
  }

  function onStreamTool(sid, p) {
    if (stoppedSid[sid]) return;
    if (activeId !== sid) {
      if (!tabFlag[sid]) tabFlag[sid] = 'unread';
      renderTabs();
      return;
    }
    var refs = streamBubble(sid);
    var line = document.createElement('div');
    line.className = 'tool-line';
    if (p.phase === 'start') line.textContent = '\u2699 ' + (p.name || 'tool') + ' …';
    else line.textContent = '\u2192 ' + String(p.preview || '').slice(0, 240);
    refs.tools.appendChild(line);
    scrollToBottom();
  }

  // Stopped flights tear the live bubble down immediately and never
  // accept late traffic: any chunk/tool/done arriving afterwards is a ghost
  // from the cancelled socket and must be dropped.
  function removeNode(el) {
    if (!el) return;
    if (el.parentNode && el.parentNode.removeChild) el.parentNode.removeChild(el);
    else if (messagesEl && messagesEl.children) {
      var i = messagesEl.children.indexOf(el);
      if (i >= 0) messagesEl.children.splice(i, 1);
    }
  }

  function teardownLive(sid) {
    if (flightTimer[sid]) { clearTimeout(flightTimer[sid]); delete flightTimer[sid]; }
    delete streamBuf[sid];
    delete phaseLabel[sid];
    var refs = streamEls[sid];
    if (refs && refs.wrap) removeNode(refs.wrap);
    delete streamEls[sid];
    removeNode(pendEl[sid]);
    delete pendEl[sid];
  }

  function onStreamStopped(sid) {
    if (stoppedSid[sid]) return;
    stoppedSid[sid] = true;
    teardownLive(sid);
    delete inflight[sid];
    if (currentFlight === sid) currentFlight = null;
    updateSendButton();
    paintHeader('live');
    reportAgentStatus('Container running');
    if (!activeId || activeId === sid) openSession(sid);
    else refreshSessionsAfterChat();
    drainQueue();
  }

  function onStreamDone(sid, reply) {
    if (stoppedSid[sid]) return; // ghost from a cancelled socket
    if (flightTimer[sid]) { clearTimeout(flightTimer[sid]); delete flightTimer[sid]; }
    delete inflight[sid];
    delete streamBuf[sid];
    delete phaseLabel[sid];
    delete streamEls[sid];
    delete pendEl[sid];
    if (currentFlight === sid) currentFlight = null;
    updateSendButton();
    reportAgentStatus('Container running');
    // Ping the Android notification drawer when the app is backgrounded so
    // the user knows the agent is done and ready for more messages.
    try {
      if (typeof document !== 'undefined' && document.hidden &&
          window.NativeHost && window.NativeHost.notifyAgentDone) {
        window.NativeHost.notifyAgentDone(String(reply || '').slice(0, 240));
      }
    } catch (_) {}
    if (!activeId || activeId === sid) openSession(sid);
    else { tabFlag[sid] = 'unread'; refreshSessionsAfterChat(); }
    drainQueue();
  }

  function reportAgentStatus(text) {
    try {
      if (window.NativeHost && window.NativeHost.reportAgentStatus) {
        window.NativeHost.reportAgentStatus(text);
      }
    } catch (_) {}
  }

  // ---------- state ----------
  var sessions = [];      // [{id, title, message_count, created_ms}]
  var activeId = null;    // current session id
  var activeThread = [];  // raw OpenAI messages of the active session
  var navState = null;    // branch pager snapshot for the active session
  var inflight = {};      // sessionId -> user text still awaiting a reply
  var tabFlag = {};       // sessionId -> 'unread' | 'error' status icon
  var messageQueue = [];  // queued user messages
  var currentFlight = null;

  function refreshLean() {
    call('lean_status', {}, function (err, r) {
      if (err || !r) return;
      renderLean(r);
      // Auto-provision if Lean is not installed and not already in progress.
      if (!r.ready && !r.provisioning && !r.downloading && !r.warming && !r.provision_error) {
        call('lean_provision', {}, function () {});
      }
    });
  }

  function pollTick() {
    call('lean_progress', {}, function (err, r) {
      if (err) return;
      renderLean(r);
      if (!r.downloading && !r.provisioning && !r.warming) {
        // Settled: progress() never reports ready and never kicks the
        // warm-up (both need the full status()), so take one full reading
        // to surface ready/version and start warming, then stop polling.
        if (leanTimer) { clearInterval(leanTimer); leanTimer = null; }
        refreshLean();
      }
    });
  }

  function renderLean(r) {
    if (!r) return;
    updateLeanBar(r);
    var isInstalling = r.provisioning || r.downloading || r.warming;
    // Show/hide install overlay.
    if (leanOverlayEl) {
      leanOverlayEl.style.display = isInstalling ? 'flex' : 'none';
    }
    // Update overlay detail and progress.
    if (isInstalling && leanOverlayDetailEl && leanOverlayFillEl && leanOverlayPctEl) {
      if (r.downloading && r.total > 0) {
        var dpct = Math.floor(r.downloaded * 100 / r.total);
        leanOverlayDetailEl.textContent = 'Downloading Lean… ' + dpct + '%';
        leanOverlayFillEl.style.width = dpct + '%';
        leanOverlayPctEl.textContent = dpct + '%';
      } else if (r.provisioning) {
        leanOverlayDetailEl.textContent = 'Extracting Lean into container…';
        leanOverlayFillEl.style.width = '100%';
        leanOverlayPctEl.textContent = 'working…';
      } else if (r.warming) {
        var pct = (r.warm_timeout > 0) ? Math.min(100, Math.floor((r.warm_elapsed || 0) * 100 / r.warm_timeout)) : 0;
        leanOverlayDetailEl.textContent = 'Warming up Lean runtime… ' + (r.warm_elapsed || 0) + 's';
        leanOverlayFillEl.style.width = pct + '%';
        leanOverlayPctEl.textContent = pct + '%';
      }
    }
    // Gate chat area on Lean readiness.
    setChatAreaVisible(r.ready);
    // Existing header status updates.
    if (r.provisioning || r.downloading) {
      leanBtnEl.style.display = 'none';
      if (!leanTimer) leanTimer = setInterval(pollTick, POLL_MS);
    } else if (r.warming) {
      leanStatusEl.textContent = 'Lean: warming up runtime… ' + (r.warm_elapsed || 0) + 's';
      leanBtnEl.style.display = 'none';
      if (!leanTimer) leanTimer = setInterval(pollTick, POLL_MS);
    } else if (r.ready) {
      if (leanTimer) { clearInterval(leanTimer); leanTimer = null; }
      leanStatusEl.textContent = r.version ? ('Lean: ready (' + r.version + ')') : 'Lean: installed';
      leanBtnEl.style.display = 'none';
    } else if (r.provision_error) {
      if (leanTimer) { clearInterval(leanTimer); leanTimer = null; }
      leanStatusEl.textContent = 'Lean: install failed — ' + r.provision_error;
      leanBtnEl.style.display = '';
      leanBtnEl.textContent = 'Retry Lean install';
    } else {
      if (leanTimer) { clearInterval(leanTimer); leanTimer = null; }
      leanStatusEl.textContent = 'Lean: ' + (r.message || 'not installed');
      leanBtnEl.style.display = '';
      leanBtnEl.textContent = 'Download & install Lean (~550 MB)';
    }
  }

  function updateLeanBar(r) {
    if (r.warming) {
      var pct = (r.warm_timeout > 0) ? Math.min(100, Math.floor((r.warm_elapsed || 0) * 100 / r.warm_timeout)) : 0;
      leanBarEl.style.display = 'block';
      leanFillEl.style.width = pct + '%';
      leanPctEl.textContent = 'Warming up Lean runtime… ' + (r.warm_elapsed || 0) + 's';
    } else if (r.downloading && r.total > 0) {
      var dpct = Math.floor(r.downloaded * 100 / r.total);
      leanBarEl.style.display = 'block';
      leanFillEl.style.width = dpct + '%';
      leanPctEl.textContent = dpct + '%';
    } else if (r.downloading || r.provisioning) {
      leanBarEl.style.display = 'block';
      leanFillEl.style.width = '100%';
      leanPctEl.textContent = 'working…';
    } else {
      leanBarEl.style.display = 'none';
    }
  }

  function loadSessions() {
    call('session_list', {}, function (err, list) {
      if (err || !list) return;
      sessions = list;
      renderTabs();
      updateHistoryBadge();
      // Open the most recent session if none is active.
      if (!activeId && sessions.length) openSession(sessions[0].id);
    });
  }

  function stopEvent(e) {
    if (e) {
      if (e.stopPropagation) e.stopPropagation();
      if (e.preventDefault) e.preventDefault();
    }
  }

  // Tab clicks must work even when the tab strip re-renders mid-tap: bind
  // one delegated listener on the container instead of per-button handlers
  // that a concurrent renderTabs() can orphan.
  function tabAction(el) {
    if (!el) return null;
    var node = el;
    while (node && node !== sessionsEl) {
      if (node.getAttribute) {
        var kind = node.getAttribute('data-tab-action');
        var sid = node.getAttribute('data-session-id');
        if (kind && sid) return { kind: kind, sid: sid, node: node };
      }
      node = node.parentNode;
    }
    return null;
  }

  function renderTabs() {
    sessionsEl.innerHTML = '';
    sessions.forEach(function (s) {
      var el = document.createElement('div');
      el.className = 'tab' + (s.id === activeId ? ' active' : '');
      var label = document.createElement('button');
      label.className = 'tab-label';
      label.textContent = tabGlyph(s.id) + (s.title || '(new)');
      label.setAttribute('data-tab-action', 'open');
      label.setAttribute('data-session-id', s.id);
      var edit = document.createElement('button');
      edit.className = 'tab-rename';
      edit.textContent = '\u270E';
      edit.setAttribute('data-tab-action', 'rename');
      edit.setAttribute('data-session-id', s.id);
      var x = document.createElement('button');
      x.className = 'tab-close';
      x.textContent = '\u00D7';
      x.setAttribute('data-tab-action', 'close');
      x.setAttribute('data-session-id', s.id);
      // NOTE: window.confirm() is dead in this WebView (no WebChromeClient),
      // so deletion uses a two-tap arm/disarm on the button itself.
      el.appendChild(label);
      el.appendChild(edit);
      el.appendChild(x);
      sessionsEl.appendChild(el);
    });
  }

  function armCloseButton(btn) {
    btn.setAttribute('data-armed', '1');
    btn.textContent = 'Sure?';
    setTimeout(function () {
      // The strip may have re-rendered since arming; only reset this node
      // when it is still armed to avoid clobbering a fresh button.
      if (btn.getAttribute && btn.getAttribute('data-armed') === '1') {
        btn.removeAttribute('data-armed');
        btn.textContent = '\u00D7';
      }
    }, 3000);
  }

  sessionsEl.addEventListener('click', function (e) {
    var hit = tabAction(e && e.target);
    if (!hit) return;
    stopEvent(e);
    if (hit.kind === 'open') openSession(hit.sid);
    else if (hit.kind === 'rename') startRename(hit.sid, hit.node);
    else if (hit.kind === 'close') {
      if (hit.node.getAttribute('data-armed') === '1') removeSession(hit.sid);
      else armCloseButton(hit.node);
    }
  });

  // Inline tab rename: swap the tab button for an input; Enter commits
  // via session_rename, Escape/blur cancels. (prompt() is dead here too.)
  // Tolerates being passed the tab strip button (post-delegation) or the
  // legacy label element: the input always replaces the action button.
  function startRename(id, labelEl) {
    var cur = '';
    sessions.forEach(function (x) { if (x.id === id) cur = x.title || ''; });
    var input = document.createElement('input');
    input.className = 'tab-edit';
    input.value = cur;
    input.placeholder = 'Name this chat…';
    var done = false;
    function finish(commit) {
      if (done) return;
      done = true;
      if (!commit) { renderTabs(); return; }
      call('session_rename', { session_id: id, title: input.value }, function (err, r) {
        if (!err && r && r.title) {
          sessions.forEach(function (x) { if (x.id === id) x.title = r.title; });
        }
        renderTabs();
      });
    }
    input.addEventListener('keydown', function (e) {
      e.stopPropagation();
      if (e.key === 'Enter') finish(true);
      else if (e.key === 'Escape') finish(false);
    });
    input.addEventListener('blur', function () { finish(true); });
    if (labelEl && labelEl.className === 'tab-rename' && labelEl.parentNode) {
      // Delegated rename button: swap the whole tab row for the editor so
      // the tap target cannot vanish mid-edit on narrow strips.
      var row = labelEl.parentNode;
      row.innerHTML = '';
      row.appendChild(input);
    } else if (labelEl && labelEl.parentNode) {
      labelEl.parentNode.replaceChild(input, labelEl);
    } else {
      return;
    }
    input.focus();
    try { input.select(); } catch (_) {}
  }

  // Per-tab status glyph: … while a reply is in flight, ● when a reply
  // arrived while the tab was in the background, ! when the last send
  // failed. Cleared when the tab is opened.
  function tabGlyph(id) {
    if (id && inflight[id]) return '\u2026 ';
    var f = tabFlag[id];
    if (f === 'unread') return '\u25cf ';
    if (f === 'error') return '! ';
    return '';
  }

  function openSession(id, after) {
    activeId = id;
    delete tabFlag[id];
    renderTabs();
    call('session_history', { session_id: id }, function (err, s) {
      if (err || !s) return;
      if (activeId !== id) return; // stale: user already moved on
      activeThread = s.messages || [];
      navState = s.nav || null;
      renderTabs();
      render();
      if (streamBuf[id]) {
        var rb = streamBubble(id);
        if (rb.body) rb.body.innerHTML = Markdown.render(streamBuf[id]);
      } else if (id && inflight[id]) {
        appendElement(makePendingEl());
      }
      if (after) after();
    });
  }

  function newSession() {
    call('session_create', {}, function (err, s) {
      if (err || !s) return;
      activeId = s.id;
      activeThread = [];
      navState = null;
      sessions.unshift({ id: s.id, title: s.title || '(new)', message_count: 0 });
      renderTabs();
      render();
      renderBranchPager();
      composerEl.focus();
    });
  }

  function removeSession(id) {
    delete inflight[id];
    delete tabFlag[id];
    call('session_delete', { session_id: id }, function () {
      sessions = sessions.filter(function (s) { return s.id !== id; });
      if (activeId === id) { activeId = null; activeThread = []; }
      renderTabs();
      render();
      // Live history: if the closed-tabs panel is open, refresh it so the
      // just-closed tab appears without reopening the clock.
      if (closedPanel.style.display === 'block') renderClosedList();
      else updateHistoryBadge();
    });
  }

  var reconnectCount = 0;
  // Live stream state, keyed by session id.
  var streamBuf = {};   // sid -> accumulated streamed markdown
  var streamEls = {};   // sid -> { wrap, body, tools } bubble refs (viewing tab)
  var pendEl = {};      // sid -> Thinking bubble ref, morphed on first chunk
  var flightTimer = {}; // sid -> watchdog timeout id
  var failedFlight = {}; // sid -> failure already reported (disconnect can race accept)
  var stoppedSid = {}; // sid -> stop acknowledged; late traffic is dropped

  function sendMessage(text) {
    text = (typeof text === 'string') ? text : composerEl.value;
    if (!text.trim()) return;
    // Send-time commit: resolve the tab synchronously (create if needed) so
    // a mid-flight tab switch always has somewhere to come back to. The
    // user turn renders from the local commit, never from the accept.
    var sid = activeId;
    if (!sid) {
      call('session_create', {}, function (err, ns) {
        if (err || !ns) return;
        activeId = ns.id;
        activeThread = ns.messages || [];
        sessions.unshift({ id: ns.id, title: '(new)', message_count: 1 });
        renderTabs();
        sendMessage(text);
      });
      return;
    }
    if (inflight[sid]) { queueMessage(text); return; }
    composerEl.value = '';
    activeThread.push({ role: 'user', content: text });
    delete stoppedSid[sid];
    render();

    if (text.charAt(0) === '!') {
      call('exec', { command: text.slice(1).trim() }, function (err, r) {
        appendMessage({ kind: 'assistant', content: err ? ('Error: ' + err) : formatExec(r) }, true);
      });
      return;
    }

    pendEl[sid] = appendElement(makePendingEl());
    delete failedFlight[sid];
    inflight[sid] = text;
    currentFlight = sid;
    renderTabs();
    updateSendButton();
    paintHeader('working');
    reportAgentStatus('Agent working…');
    // Client-side bound matching the server's 300s cap: a hung reply
    // becomes a recoverable error instead of a stuck pending bubble.
    flightTimer[sid] = setTimeout(function () {
      failFlight(sid, 'timed out after 310s — message restored to the composer; edit and resend.');
    }, 310000);
    call('chat', { session_id: activeId, prompt: text }, function (err, r) {
      if (err || !(r && r.accepted)) {
        // Transport-level failure (the streamed outcome arrives as
        // chat_done/chat_error notifications instead).
        failFlight(sid, (err && (err.message || err)) || 'send failed');
      }
      // Accepted: chunks arrive as notifications; the watchdog bounds them.
    });
  }

  function queueMessage(text) {
    if (!text.trim()) return;
    messageQueue.push(text.trim());
    composerEl.value = '';
    if (!currentFlight) drainQueue();
  }

  function drainQueue() {
    if (currentFlight || !messageQueue.length) return;
    var next = messageQueue.shift();
    sendMessage(next);
  }

  function interruptFlight() {
    if (!currentFlight) return;
    var sid = currentFlight;
    // True stop: tell the daemon to drop the provider socket and tool loop
    // first (cuts token spend), then tear the live bubble down locally.
    call('chat_stop', { session_id: sid === '__fresh__' ? null : sid, partial: streamBuf[sid] || '' }, function () {});
    onStreamStopped(sid);
  }

  function updateSendButton() {
    if (currentFlight && inflight[currentFlight]) {
      sendBtn.textContent = 'Stop';
      sendBtn.disabled = false;
    } else {
      sendBtn.textContent = 'Send';
      sendBtn.disabled = false;
    }
  }

  // Shared failure path: the thinking bubble is torn down, the sent
  // user bubble stays (committed at send time — never clobbers the
  // composer or queued text), and an agent error bubble carries a Retry.
  function failFlight(sid, message) {
    if (failedFlight[sid]) return;
    failedFlight[sid] = true;
    teardownLive(sid);
    delete inflight[sid];
    if (currentFlight === sid) currentFlight = null;
    updateSendButton();
    paintHeader('live');
    reportAgentStatus('Container running');
    var replySid = (sid === '__fresh__') ? null : sid;
    if (replySid) tabFlag[replySid] = 'error';
    refreshSessionsAfterChat();
    if (!activeId || activeId === replySid) {
      // Re-sync (server never persisted the failed turn) WITHOUT wiping the
      // local send-time commit: openSession's reload would drop the sent
      // bubble the user must see, so only refresh titles/flags here.
      refreshSessionsAfterChat();
      appendErrorBubble(message, sid === '__fresh__' ? null : sid);
    }
    drainQueue();
  }

  function appendErrorBubble(message, sid) {
    var wrap = document.createElement('div');
    wrap.className = 'msg assistant';
    var head = document.createElement('div');
    head.className = 'author';
    head.textContent = 'Assistant';
    var body = document.createElement('div');
    body.className = 'body';
    body.innerHTML = Markdown.render('**Error:** ' + message);
    var meta = document.createElement('div');
    meta.className = 'meta';
    var retry = document.createElement('button');
    retry.className = 'mini';
    retry.textContent = '↻ Retry';
    retry.onclick = function (e) {
      if (e && e.stopPropagation) e.stopPropagation();
      removeNode(wrap);
      retryLast(sid);
    };
    meta.appendChild(retry);
    wrap.appendChild(head);
    wrap.appendChild(body);
    wrap.appendChild(meta);
    appendElement(wrap);
  }

  // Retry = resend the thread's last user turn without retyping. The
  // canonical source is the LOCAL thread: the failed server never stored
  // the turn, so replaying from history would resend a stale message.
  function retryLast(sid) {
    if (sid && activeId !== sid) { openSession(sid); return; }
    for (var i = activeThread.length - 1; i >= 0; i--) {
      var m = activeThread[i];
      if (m && m.role === 'user' && m.content) {
        sendMessage(m.content);
        return;
      }
    }
  }

  function formatExec(r) {
    var t = '';
    if (r && r.stdout) t += r.stdout;
    if (r && r.stderr) t += '\n```\n' + r.stderr + '\n```';
    if (r && (r.exit_code !== 0 || r.timed_out)) t += '\n`[exit ' + r.exit_code + (r.timed_out ? ' timed out' : '') + ']`';
    return t || '(no output)';
  }

  function refreshSessionsAfterChat() {
    call('session_list', {}, function (err, list) {
      if (err || !list) return;
      sessions = list;
      renderTabs();
    });
  }

  // ---------- rendering ----------
  function render() {
    messagesEl.innerHTML = '';
    renderBranchPager();
    var turns = ChatState.visibleTurns(activeThread);
    turns.forEach(function (t, idx) {
      appendElement(makeTurnEl(t, idx));
    });
    scrollToBottom();
  }

  function makeTurnEl(turn, visibleIndex) {
    var wrap = document.createElement('div');
    wrap.className = 'msg ' + turn.kind;

    var head = document.createElement('div');
    head.className = 'author';
    head.textContent = turn.kind === 'user' ? 'You' : 'Assistant';

    var body = document.createElement('div');
    body.className = 'body';
    if (turn.kind === 'assistant') {
      body.innerHTML = Markdown.render(turn.content);
    } else {
      body.textContent = turn.content;
    }

    var meta = document.createElement('div');
    meta.className = 'meta';
    // Action menu (Copy / Fork / Undo) stays hidden until the message is
    // tapped: single tap toggles it, keeping the thread clean.
    meta.style.display = 'none';

    if (turn.kind === 'assistant' && visibleIndex >= 0) {
      var fork = document.createElement('button');
      fork.className = 'mini';
      fork.textContent = '⤴ Fork';
      fork.onclick = function (e) { if (e && e.stopPropagation) e.stopPropagation(); forkFrom(visibleIndex); };
      meta.appendChild(fork);
    }
    var copy = document.createElement('button');
    copy.className = 'mini';
    copy.textContent = '⧉ Copy';
    copy.onclick = function (e) { if (e && e.stopPropagation) e.stopPropagation(); copyText(turn.content); };
    meta.appendChild(copy);

    if (turn.kind === 'user') {
      var undo = document.createElement('button');
      undo.className = 'mini';
      undo.textContent = '↩ Undo';
      undo.onclick = function (e) { if (e && e.stopPropagation) e.stopPropagation(); undoLastTurn(); };
      meta.appendChild(undo);
      // Resume only makes sense on the latest user turn: continue the
      // stopped/clipped generation from the banked partial without
      // re-prompting (no re-think of "continue").
      var resume = document.createElement('button');
      resume.className = 'mini';
      resume.textContent = '▶ Resume';
      resume.onclick = function (e) { if (e && e.stopPropagation) e.stopPropagation(); retryLast(activeId); };
      meta.appendChild(resume);
    }

    wrap.onclick = function (e) {
      if (e && e.stopPropagation) e.stopPropagation();
      toggleMetaMenu(wrap, meta);
    };
    wrap.title = 'Tap for actions';

    wrap.appendChild(head);
    wrap.appendChild(body);
    wrap.appendChild(meta);
    return wrap;
  }

  // Single-tap menu: reveal this message's actions, collapse all others.
  function toggleMetaMenu(wrap, meta) {
    var show = meta.style.display === 'none';
    var kids = messagesEl.children || [];
    for (var i = 0; i < kids.length; i++) {
      var m = childByClass(kids[i], 'meta');
      if (m) m.style.display = 'none';
    }
    void wrap;
    meta.style.display = show ? 'flex' : 'none';
  }

  function copyText(text) {
    if (!text) return;
    var nav = (typeof navigator !== 'undefined') ? navigator : null;
    if (nav && nav.clipboard && nav.clipboard.writeText) {
      nav.clipboard.writeText(text).catch(function () {});
    } else if (typeof document !== 'undefined' && document.body) {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      if (ta.select) ta.select();
      try { document.execCommand('copy'); } catch (_) {}
      document.body.removeChild(ta);
    }
    showToast('Copied');
  }

  function showToast(text) {
    if (typeof document === 'undefined' || !document.body) return;
    var el = document.createElement('div');
    el.className = 'toast';
    el.textContent = text;
    document.body.appendChild(el);
    setTimeout(function () { removeNode(el); }, 1200);
  }

  function undoLastTurn() {
    if (!activeId) return;
    call('session_undo', { session_id: activeId }, function (err, r) {
      if (err || !r || !r.user_message) {
        showToast('Nothing to undo');
        return;
      }
      composerEl.value = r.user_message;
      if (r.messages) activeThread = r.messages;
      navState = r.nav || null;
      render();
      renderBranchPager();
      composerEl.focus();
    });
  }

  function redoTurn() {
    if (!activeId) return;
    call('session_redo', { session_id: activeId }, function (err, r) {
      if (err || !r || !r.messages) {
        showToast('Nothing to redo');
        return;
      }
      activeThread = r.messages;
      navState = r.nav || null;
      render();
      renderBranchPager();
    });
  }

  // Branch pager: walk the tree head up/down and across limbs. Undo never
  // prunes — it only moves head — so every limb stays reachable.
  function renderBranchPager() {
    var pager = $('branch-pager');
    if (!pager) return;
    pager.innerHTML = '';
    if (!activeId || !navState) { pager.style.display = 'none'; return; }
    var kids = (navState.children || []);
    if (!navState.can_undo && !navState.can_redo && !kids.length) {
      pager.style.display = 'none';
      return;
    }
    pager.style.display = 'flex';
    function btn(label, title, fn, off) {
      var b = document.createElement('button');
      b.className = 'mini';
      b.textContent = label;
      b.title = title;
      if (off) b.disabled = true;
      else b.onclick = fn;
      pager.appendChild(b);
      return b;
    }
    btn('↩ Undo', 'Back up one turn (kept as phantom)', undoLastTurn, !navState.can_undo);
    btn('↪ Redo', 'Back down the undone limb', redoTurn, !navState.can_redo);
    kids.slice(0, 3).forEach(function (k, i) {
      btn('⑂ ' + (i + 1), k.role + ': ' + k.preview, function () { gotoNode(k.id); });
    });
    var info = document.createElement('span');
    info.className = 'branch-info';
    info.textContent = kids.length ? (kids.length + ' branch' + (kids.length > 1 ? 'es' : '')) : 'linear';
    pager.appendChild(info);
  }

  function gotoNode(node) {
    if (!activeId) return;
    call('session_goto', { session_id: activeId, node: node }, function (err, r) {
      if (err || !r || !r.messages) return;
      activeThread = r.messages;
      navState = r.nav || null;
      render();
      renderBranchPager();
    });
  }

  // noFork skips the action menu for ephemeral bubbles (errors, exec
  // output) whose position does not map onto the server thread.
  function appendMessage(turn, noFork) {
    var turns = ChatState.visibleTurns(activeThread);
    appendElement(makeTurnEl(turn, noFork ? -1 : turns.length));
  }

  function makePendingEl() {
    var wrap = document.createElement('div');
    wrap.className = 'msg assistant pending';
    var head = document.createElement('div');
    head.className = 'author';
    head.textContent = 'Assistant';
    var body = document.createElement('div');
    body.className = 'body';
    body.innerHTML = '<span class="typing"><span></span><span></span><span></span></span>';
    wrap.appendChild(head);
    wrap.appendChild(body);
    return wrap;
  }

  function appendElement(el) {
    messagesEl.appendChild(el);
    scrollToBottom();
    return el;
  }

  function scrollToBottom() {
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function forkFrom(visibleIndex) {
    var rawIndex = ChatState.forkRawIndex(activeThread, visibleIndex);
    if (rawIndex == null) return;
    call('session_fork', { session_id: activeId, message_index: rawIndex }, function (err, f) {
      if (err || !f) return;
      activeId = f.id;
      activeThread = f.messages || [];
      sessions.unshift({ id: f.id, title: f.title, message_count: f.messages.length });
      call('session_nav', { session_id: f.id }, function (e2, n) {
        if (!e2 && n) navState = n.nav || null;
        renderBranchPager();
      });
      renderTabs();
      render();
    });
  }

  // History clock: badge shows the closed-tab count; the panel refreshes
  // live on close/restore instead of only when opened.
  function updateHistoryBadge() {
    call('session_closed', {}, function (err, list) {
      if (err || !list) return;
      historyBtn.textContent = list.length ? ('🕘 ' + list.length) : '🕘';
    });
  }

  function renderClosedList() {
    call('session_closed', {}, function (err, list) {
      if (err || !list) return;
      historyBtn.textContent = list.length ? ('🕘 ' + list.length) : '🕘';
      closedList.innerHTML = '';
      if (!list.length) {
        var em = document.createElement('div');
        em.className = 'closed-empty';
        em.textContent = 'No closed tabs — deleted tabs stay here for restore.';
        closedList.appendChild(em);
      }
      list.forEach(function (s) {
        var row = document.createElement('div');
        row.className = 'closed-item';
        var t = document.createElement('span');
        t.textContent = (s.archived ? '📦 ' : '') + (s.title || '(new)') + ' (' + (s.message_count || 0) + ')';
        var rb = document.createElement('button');
        rb.textContent = 'Restore';
        rb.onclick = function () {
          call('session_restore', { session_id: s.id }, function (e2, f) {
            if (e2 || !f) return;
            closedPanel.style.display = 'none';
            sessions.unshift({ id: f.id, title: f.title, message_count: (f.messages || []).length });
            openSession(f.id);
            updateHistoryBadge();
          });
        };
        row.appendChild(t);
        row.appendChild(rb);
        var arc = document.createElement('button');
        arc.textContent = s.archived ? 'Unarchive' : 'Archive';
        arc.onclick = function () {
          call('session_archive', { session_id: s.id, archived: !s.archived }, function () {
            renderClosedList();
          });
        };
        row.appendChild(arc);
        closedList.appendChild(row);
      });
      closedPanel.style.display = 'block';
    });
  }
  // ---------- wire events ----------
  newBtn.onclick = newSession;
  historyBtn.onclick = function () {
    if (closedPanel.style.display === 'block') { closedPanel.style.display = 'none'; return; }
    renderClosedList();
  };
  sendBtn.onclick = function () {
    if (currentFlight && inflight[currentFlight]) {
      interruptFlight();
    } else {
      var text = composerEl.value;
      if (text.trim()) {
        queueMessage(text);
      }
    }
  };
  leanBtnEl.onclick = function () {
    // Kick the provision and poll regardless of the reply: the daemon
    // answers fire-and-forget ("started" / "already running"), and a
    // dropped reply must not leave the button dead with no progress.
    leanBtnEl.style.display = 'none';
    leanStatusEl.textContent = 'Lean: starting install…';
    call('lean_provision', {}, function () {
      if (!leanTimer) leanTimer = setInterval(pollTick, POLL_MS);
    });
    // Belt-and-suspenders: even if the RPC never returns (dead socket),
    // start polling so the status line reflects reality.
    setTimeout(function () {
      if (!leanTimer) leanTimer = setInterval(pollTick, POLL_MS);
    }, 1500);
  };
  composerEl.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      var text = composerEl.value;
      if (text.trim()) {
        if (currentFlight && inflight[currentFlight]) {
          queueMessage(text);
        } else {
          queueMessage(text);
        }
      }
    }
  });

  // Bridge for Android notification replies: the app injects the reply
  // text here and it flows through the normal queue/send path.
  window.ForgeRigReply = function (text) {
    if (!text || !String(text).trim()) return;
    composerEl.value = String(text);
    queueMessage(composerEl.value);
    try { composerEl.focus(); } catch (_) {}
  };

  // Re-render when the daemon connection drops/returns so a reconnected page
  // re-syncs the active session.
  function onVisibility() { if (activeId) openSession(activeId); }
  window.addEventListener('online', onVisibility);

  connect();
})();
