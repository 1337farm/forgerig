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

  function connect() {
    statusEl.textContent = 'Connecting…';
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/');
    ws.onopen = function () {
      statusEl.textContent = 'Connected';
      call('status', {}, function (err, r) {
        if (err || !r || !r.provider) return;
        providerEl.textContent = r.provider;
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
      statusEl.textContent = 'Disconnected — retrying…';
      // Fail every in-flight call so sends are recoverable (composer
      // restore) instead of stuck forever with a dead pending bubble.
      // This covers both bare RPCs (pending map) and accepted streams
      // (whose reply already consumed their pending entry).
      var stale = pending;
      pending = {};
      Object.keys(stale).forEach(function (id) {
        try { stale[id](new Error('disconnected')); } catch (_) {}
      });
      Object.keys(inflight).forEach(function (sid) {
        try { failFlight(sid, 'disconnected — message restored to the composer; edit and resend.'); } catch (_) {}
      });
      setTimeout(connect, 1000);
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
    else if (d.method === 'chat_tool') onStreamTool(sid, p);
    else if (d.method === 'chat_done') onStreamDone(sid);
    else if (d.method === 'chat_error') {
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

  function onStreamChunk(sid, delta) {
    streamBuf[sid] = (streamBuf[sid] || '') + delta;
    if (activeId !== sid) {
      if (!tabFlag[sid]) tabFlag[sid] = 'unread';
      renderTabs();
      return;
    }
    var refs = streamBubble(sid);
    if (refs.body) refs.body.innerHTML = Markdown.render(streamBuf[sid]);
    scrollToBottom();
  }

  function onStreamTool(sid, p) {
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

  function onStreamDone(sid) {
    if (flightTimer[sid]) { clearTimeout(flightTimer[sid]); delete flightTimer[sid]; }
    delete inflight[sid];
    delete streamBuf[sid];
    delete streamEls[sid];
    delete pendEl[sid];
    if (!activeId || activeId === sid) openSession(sid);
    else { tabFlag[sid] = 'unread'; refreshSessionsAfterChat(); }
  }

  // ---------- state ----------
  var sessions = [];      // [{id, title, message_count, created_ms}]
  var activeId = null;    // current session id
  var activeThread = [];  // raw OpenAI messages of the active session
  var inflight = {};      // sessionId -> user text still awaiting a reply
  var tabFlag = {};       // sessionId -> 'unread' | 'error' status icon

  function refreshLean() {
    call('lean_status', {}, function (err, r) { if (!err) renderLean(r); });
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
    } else {
      if (leanTimer) { clearInterval(leanTimer); leanTimer = null; }
      leanStatusEl.textContent = 'Lean: ' + (r.message || 'not installed');
      leanBtnEl.style.display = '';
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
      // Open the most recent session if none is active.
      if (!activeId && sessions.length) openSession(sessions[0].id);
    });
  }

  function renderTabs() {
    sessionsEl.innerHTML = '';
    sessions.forEach(function (s) {
      var el = document.createElement('div');
      el.className = 'tab' + (s.id === activeId ? ' active' : '');
      var label = document.createElement('button');
      label.className = 'tab-label';
      label.textContent = tabGlyph(s.id) + (s.title || '(new)');
      label.onclick = function () { openSession(s.id); };
      var edit = document.createElement('button');
      edit.className = 'tab-rename';
      edit.textContent = '✎';
      edit.onclick = function (e) { e.stopPropagation(); startRename(s.id, label); };
      var x = document.createElement('button');
      x.className = 'tab-close';
      x.textContent = '×';
      // NOTE: window.confirm() is dead in this WebView (no WebChromeClient),
      // so deletion uses a two-tap arm/disarm on the button itself.
      x.onclick = function (e) {
        e.stopPropagation();
        if (x.getAttribute('data-armed') === '1') {
          removeSession(s.id);
        } else {
          x.setAttribute('data-armed', '1');
          x.textContent = 'Sure?';
          setTimeout(function () {
            x.removeAttribute('data-armed');
            x.textContent = '×';
          }, 3000);
        }
      };
      el.appendChild(label);
      el.appendChild(edit);
      el.appendChild(x);
      sessionsEl.appendChild(el);
    });
  }

  // Inline tab rename: swap the label for an input; Enter commits via
  // session_rename, Escape/blur cancels. (prompt() is dead here too.)
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
    labelEl.parentNode.replaceChild(input, labelEl);
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

  function openSession(id) {
    activeId = id;
    delete tabFlag[id];
    renderTabs();
    call('session_history', { session_id: id }, function (err, s) {
      if (err || !s) return;
      if (activeId !== id) return; // stale: user already moved on
      activeThread = s.messages || [];
      renderTabs();
      render();
      if (streamBuf[id]) {
        var rb = streamBubble(id);
        if (rb.body) rb.body.innerHTML = Markdown.render(streamBuf[id]);
      } else if (id && inflight[id]) {
        renderPendingUser(inflight[id]);
        appendElement(makePendingEl());
      }
    });
  }

  function newSession() {
    call('session_create', {}, function (err, s) {
      if (err || !s) return;
      activeId = s.id;
      activeThread = [];
      sessions.unshift({ id: s.id, title: s.title || '(new)', message_count: 0 });
      renderTabs();
      render();
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
    });
  }

  // Live stream state, keyed by session id.
  var streamBuf = {};   // sid -> accumulated streamed markdown
  var streamEls = {};   // sid -> { wrap, body, tools } bubble refs (viewing tab)
  var pendEl = {};      // sid -> Thinking bubble ref, morphed on first chunk
  var flightTimer = {}; // sid -> watchdog timeout id
  var failedFlight = {}; // sid -> failure already reported (disconnect can race accept)

  function sendMessage() {
    var text = composerEl.value;
    if (!text.trim()) return;
    var sid = activeId || '__fresh__';
    if (inflight[sid]) return; // one flight per tab (fresh tab included)
    composerEl.value = '';

    if (text.charAt(0) === '!') {
      renderPendingUser(text);
      call('exec', { command: text.slice(1).trim() }, function (err, r) {
        appendMessage({ kind: 'assistant', content: err ? ('Error: ' + err) : formatExec(r) }, true);
      });
      return;
    }

    renderPendingUser(text);
    pendEl[sid] = appendElement(makePendingEl());
    delete failedFlight[sid];
    inflight[sid] = text;
    renderTabs();
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

  // Shared failure path: flag, refresh titles, restore the text for retry.
  function failFlight(sid, message) {
    if (failedFlight[sid]) return;
    failedFlight[sid] = true;
    var text = inflight[sid];
    if (flightTimer[sid]) { clearTimeout(flightTimer[sid]); delete flightTimer[sid]; }
    delete inflight[sid];
    delete streamBuf[sid];
    delete streamEls[sid];
    delete pendEl[sid];
    var replySid = (sid === '__fresh__') ? null : sid;
    if (replySid) tabFlag[replySid] = 'error';
    refreshSessionsAfterChat();
    if (!activeId || activeId === replySid) {
      // Undo: failed turns are NOT persisted server-side, so the text goes
      // back in the composer for edit+retry with no duplication.
      if (text) composerEl.value = text;
      appendMessage({ kind: 'assistant', content: '**Error:** ' + message }, true);
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

    if (turn.kind === 'assistant' && visibleIndex >= 0) {
      var fork = document.createElement('button');
      fork.className = 'mini';
      fork.textContent = '⤴ Fork';
      fork.onclick = function () { forkFrom(visibleIndex); };
      meta.appendChild(fork);
    }

    wrap.appendChild(head);
    wrap.appendChild(body);
    wrap.appendChild(meta);
    return wrap;
  }

  // noFork skips the Fork button for ephemeral bubbles (errors, exec
  // output) whose position does not map onto the server thread.
  function appendMessage(turn, noFork) {
    var turns = ChatState.visibleTurns(activeThread);
    appendElement(makeTurnEl(turn, noFork ? -1 : turns.length));
  }

  function renderPendingUser(text) {
    var wrap = document.createElement('div');
    wrap.className = 'msg user';
    var head = document.createElement('div');
    head.className = 'author';
    head.textContent = 'You';
    var body = document.createElement('div');
    body.className = 'body';
    body.textContent = text;
    wrap.appendChild(head);
    wrap.appendChild(body);
    appendElement(wrap);
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
      renderTabs();
      render();
    });
  }

  // ---------- wire events ----------
  newBtn.onclick = newSession;
  historyBtn.onclick = function () {
    if (closedPanel.style.display === 'block') { closedPanel.style.display = 'none'; return; }
    call('session_closed', {}, function (err, list) {
      if (err || !list) return;
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
        t.textContent = (s.title || '(new)') + ' (' + (s.message_count || 0) + ')';
        var rb = document.createElement('button');
        rb.textContent = 'Restore';
        rb.onclick = function () {
          call('session_restore', { session_id: s.id }, function (e2, f) {
            if (e2 || !f) return;
            closedPanel.style.display = 'none';
            sessions.unshift({ id: f.id, title: f.title, message_count: (f.messages || []).length });
            openSession(f.id);
          });
        };
        row.appendChild(t);
        row.appendChild(rb);
        closedList.appendChild(row);
      });
      closedPanel.style.display = 'block';
    });
  };
  sendBtn.onclick = sendMessage;
  leanBtnEl.onclick = function () {
    var was = leanBtnEl.style.display;
    leanBtnEl.style.display = 'none';
    call('lean_provision', {}, function () {
      if (!leanTimer) leanTimer = setInterval(pollTick, POLL_MS);
    });
    void was;
  };
  composerEl.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  });

  // Re-render when the daemon connection drops/returns so a reconnected page
  // re-syncs the active session.
  function onVisibility() { if (activeId) openSession(activeId); }
  window.addEventListener('online', onVisibility);

  connect();
})();