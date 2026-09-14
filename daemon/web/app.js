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
  var statusEl = $('status');
  var providerEl = $('provider');
  var leanStatusEl = $('lean-status');
  var leanBtnEl = $('lean-btn');
  var leanBarEl = $('lean-bar');
  var leanFillEl = $('lean-fill');

  // ---------- RPC client (id-keyed, supports concurrency) ----------
  var ws = null;
  var reqId = 0;
  var pending = {};
  var POLL_MS = 500;
  var leanTimer = null;

  function call(method, params, cb) {
    if (!ws || ws.readyState !== 1) {
      if (cb) cb(new Error('not connected'));
      return;
    }
    var id = ++reqId;
    pending[id] = cb || function () {};
    ws.send(JSON.stringify({ jsonrpc: '2.0', method: method, params: params || {}, id: id }));
  }

  function connect() {
    statusEl.textContent = 'Connecting…';
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/');
    ws.onopen = function () {
      statusEl.textContent = 'Connected';
      call('status', {}, function () {});
      refreshLean();
      loadSessions();
    };
    ws.onclose = function () {
      statusEl.textContent = 'Disconnected — retrying…';
      setTimeout(connect, 1000);
    };
    ws.onmessage = function (e) {
      var d;
      try { d = JSON.parse(e.data); } catch (_) { return; }
      if (d.id != null && pending[d.id]) {
        var cb = pending[d.id];
        delete pending[d.id];
        cb(d.error, d.result);
      }
    };
  }

  // ---------- state ----------
  var sessions = [];      // [{id, title, message_count, created_ms}]
  var activeId = null;    // current session id
  var activeThread = [];  // raw OpenAI messages of the active session
  var busy = false;       // a chat is in flight for the active session

  function refreshLean() {
    call('lean_status', {}, function (err, r) { if (!err) renderLean(r); });
  }

  function pollTick() {
    call('lean_progress', {}, function (err, r) { if (!err) renderLean(r); });
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
      leanFillEl.textContent = 'Warming up Lean runtime… ' + (r.warm_elapsed || 0) + 's';
    } else if (r.downloading && r.total > 0) {
      var dpct = Math.floor(r.downloaded * 100 / r.total);
      leanBarEl.style.display = 'block';
      leanFillEl.style.width = dpct + '%';
      leanFillEl.textContent = dpct + '%';
    } else if (r.downloading || r.provisioning) {
      leanBarEl.style.display = 'block';
      leanFillEl.style.width = '100%';
      leanFillEl.textContent = 'working…';
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
      label.textContent = s.title || '(new)';
      label.onclick = function () { openSession(s.id); };
      var x = document.createElement('button');
      x.className = 'tab-close';
      x.textContent = '×';
      x.onclick = function (e) { e.stopPropagation(); removeSession(s.id); };
      el.appendChild(label);
      el.appendChild(x);
      sessionsEl.appendChild(el);
    });
  }

  function openSession(id) {
    activeId = id;
    call('session_history', { session_id: id }, function (err, s) {
      if (err || !s) return;
      activeThread = s.messages || [];
      busy = false;
      renderTabs();
      render();
    });
  }

  function newSession() {
    call('session_create', {}, function (err, s) {
      if (err || !s) return;
      activeId = s.id;
      activeThread = [];
      busy = false;
      sessions.unshift({ id: s.id, title: s.title || '(new)', message_count: 0 });
      renderTabs();
      render();
      composerEl.focus();
    });
  }

  function removeSession(id) {
    if (!confirm('Delete this conversation?')) return;
    call('session_delete', { session_id: id }, function () {
      sessions = sessions.filter(function (s) { return s.id !== id; });
      if (activeId === id) { activeId = null; activeThread = []; }
      renderTabs();
      render();
    });
  }

  function sendMessage() {
    var text = composerEl.value;
    if (!text.trim() || busy) return;
    var sid = activeId;
    composerEl.value = '';

    if (text.charAt(0) === '!') {
      renderPendingUser(text);
      call('exec', { command: text.slice(1).trim() }, function (err, r) {
        appendMessage({ kind: 'assistant', content: err ? ('Error: ' + err) : formatExec(r) });
      });
      return;
    }

    renderPendingUser(text);
    busy = true;
    call('chat', { session_id: sid, prompt: text }, function (err, r) {
      busy = false;
      if (err) {
        appendMessage({ kind: 'assistant', content: '**Error:** ' + (err.message || err) });
      } else if (r) {
        if (r.session_id) activeId = r.session_id;
        appendMessage({ kind: 'assistant', content: r.reply });
      }
      refreshSessionsAfterChat();
    });
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
    if (busy) appendElement(makePendingEl());
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

    if (turn.kind === 'assistant') {
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

  function appendMessage(turn) {
    var turns = ChatState.visibleTurns(activeThread);
    appendElement(makeTurnEl(turn, turns.length));
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
    var body = document.createElement('div');
    body.className = 'body';
    body.textContent = 'Thinking…';
    wrap.appendChild(body);
    return wrap;
  }

  function appendElement(el) {
    messagesEl.appendChild(el);
    scrollToBottom();
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
      busy = false;
      sessions.unshift({ id: f.id, title: f.title, message_count: f.messages.length });
      renderTabs();
      render();
    });
  }

  // ---------- wire events ----------
  newBtn.onclick = newSession;
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