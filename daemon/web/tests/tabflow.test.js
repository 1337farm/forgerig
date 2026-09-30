'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');

function makeEl(id) {
  const el = {
    id, children: [], style: {}, textContent: '',
    value: '', onclick: null,
    attrs: {},
    appendChild(c) { c.parentNode = this; this.children.push(c); return c; },
    replaceChild(n, o) {
      const i = this.children.indexOf(o);
      if (i >= 0) { n.parentNode = this; this.children[i] = n; }
      return o;
    },
    addEventListener(k, f) { (this._h = this._h || {})[k] = f; },
    fire(k, e) { if (this._h && this._h[k]) this._h[k](e || {}); },
    click() {
      let node = this, root = this;
      while (root.parentNode) root = root.parentNode;
      const fireOn = (el) => {
        if (el._h && el._h.click) el._h.click({ target: this, stopPropagation() {}, preventDefault() {} });
      };
      fireOn(this);
      if (this.parentNode && this.parentNode !== root) fireOn(this.parentNode);
      fireOn(root);
    },
    setAttribute(k, v) { this.attrs[k] = v; },
    getAttribute(k) { return this.attrs[k]; },
    removeAttribute(k) { delete this.attrs[k]; },
    focus() {},
  };
  let html = '';
  Object.defineProperty(el, 'innerHTML', {
    get() { return html; },
    // Detached children must lose their parentNode, exactly as the real DOM
    // does: app.js uses `node.parentNode` as its only "is this still in the
    // document?" test, so leaving the link in place makes the fake DOM agree
    // with code that would never be reached in a browser.
    set(v) {
      el.children.forEach((c) => { c.parentNode = null; });
      html = v;
      el.children = [];
    },
  });
  return el;
}

const ids = ['tab-list', 'messages', 'composer', 'send-btn', 'new-session',
  'history-btn', 'closed-panel', 'closed-list',
  'status', 'provider', 'lean-status', 'lean-btn', 'lean-bar', 'lean-fill',
  'lean-pct', 'setup-notice', 'setup-notice-text'];
const els = {};
ids.forEach((id) => { els[id] = makeEl(id); });

const listeners = {};
const sent = [];
let nextId = 100;

global.document = {
  getElementById: (id) => els[id] || makeEl(id),
  createElement: (tag) => makeEl(tag),
};
// Chunks are coalesced into one paint per animation frame. Model that here so
// the tests exercise the same scheduling the WebView does: queue frames, and
// run them only when a test explicitly flushes.
const rafQueue = [];
global.window = {
  addEventListener: (k, f) => { listeners[k] = f; },
  requestAnimationFrame(cb) { rafQueue.push(cb); return rafQueue.length; },
};
function flushFrames() {
  const due = rafQueue.splice(0, rafQueue.length);
  due.forEach((cb) => cb());
  return due.length;
}
// The NativeHost bridge is how the app hands the daemon's auth token to the UI.
global.window.NativeHost = { getAuthToken: () => 'bridge-token' };
global.location = { protocol: 'http:', host: 'localhost' };
global.confirm = () => { throw new Error('confirm must never be called'); };

// Fake server with two pre-existing sessions.
const server = {
  sessions: {
    A: [{ role: 'system', content: 'S' }, { role: 'user', content: 'q-A' }, { role: 'assistant', content: 'a-A' }],
    B: [{ role: 'system', content: 'S' }, { role: 'user', content: 'q-B' }],
  },
  titles: { A: 'first', B: '' },
  pendingChat: [],
  trash: {},
  sock: null,
  onmessage: null,
  receive(obj) {
    sent.push(obj);
    const reply = (result) => server.sock.onmessage({ data: JSON.stringify({ id: obj.id, result }) });
    switch (obj.method) {
      case 'auth': {
        // The daemon refuses everything until this succeeds, and it must be
        // the first frame on the wire.
        assert.equal(obj.params.token, 'bridge-token', 'auth sends the bridge token');
        reply({ authenticated: true });
        break;
      }
      case 'status': reply({ provider: 'provider=openai model=x key=set' }); break;
      case 'lean_status': reply({}); break;
      case 'session_list':
        reply(Object.keys(this.sessions).map((id) => ({ id, title: this.titles[id] || '', message_count: 0, created_ms: 1 })));
        break;
      case 'session_history':
        reply({ id: obj.params.session_id, title: '', messages: this.sessions[obj.params.session_id] || [] });
        break;
      case 'chat': {
        const sid = obj.params.session_id;
        if (server.failNextChat) {
          server.failNextChat = false;
          server.sock.onmessage({ data: JSON.stringify({ id: obj.id, error: { message: 'boom' } }) });
          break;
        }
        reply({ accepted: true, session_id: sid });
        this.pendingChat.push(obj);
        break;
      }
      case 'chat_stop': reply({ stopped: true }); break;
      case 'session_undo': {
        const msgs = this.sessions[obj.params.session_id] || [];
        const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
        reply({ user_message: lastUser ? lastUser.content : '', messages: msgs, nav: null });
        break;
      }
      case 'session_redo': reply({ messages: this.sessions[obj.params.session_id] || [], nav: null }); break;
      case 'session_goto': reply({ messages: this.sessions[obj.params.session_id] || [], nav: null }); break;
      case 'session_nav': reply({ nav: null, messages: this.sessions[obj.params.session_id] || [] }); break;
      case 'session_archive': reply({ archived: true }); break;
      case 'session_create': {
        const nid = 'C' + (nextId++);
        this.sessions[nid] = [{ role: 'system', content: 'S' }];
        this.titles[nid] = '';
        reply({ id: nid, title: '', messages: [] });
        break;
      }
      case 'session_delete': {
        server.trash[obj.params.session_id] = server.sessions[obj.params.session_id];
        delete server.sessions[obj.params.session_id];
        reply({ deleted: true });
        break;
      }
      case 'session_closed':
        reply(Object.keys(server.trash).map((id) => ({ id, title: 'Budget chat', message_count: 2, created_ms: 1 })));
        break;
      case 'session_restore': {
        const rs = server.trash[obj.params.session_id];
        server.sessions[obj.params.session_id] = rs;
        delete server.trash[obj.params.session_id];
        reply({ id: obj.params.session_id, title: 'Budget chat', messages: rs });
        break;
      }
      case 'session_fork': {
        const src = this.sessions[obj.params.session_id];
        const keep = obj.params.message_index;
        const msgs = src.slice(0, keep + 1);
        const nid = 'F' + (nextId++);
        this.sessions[nid] = msgs;
        this.titles[nid] = 'fork';
        reply({ id: nid, title: 'fork', messages: msgs });
        break;
      }
    }
  },
  resolveChat(obj, replyText) {
    // Non-streaming finish (Gemini path): single done notification.
    const sid = obj.params.session_id;
    this.sessions[sid].push({ role: 'user', content: obj.params.prompt }, { role: 'assistant', content: replyText });
    this.pushDone(sid, replyText);
  },
  pushNotif(method, params) {
    server.sock.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', method, params }) });
  },
  pushChunk(sid, delta) { this.pushNotif('chat_chunk', { session_id: sid, delta }); },
  pushTool(sid, phase, extra) {
    this.pushNotif('chat_tool', Object.assign({ session_id: sid, phase }, extra));
  },
  pushDone(sid, reply) {
    this.pushNotif('chat_done', { session_id: sid, reply });
  },
  failChat(obj) {
    // Legacy helper: real server errors arrive as chat_error pushes or
    // instead-of-accept replies; direct duplicate responses are impossible.
    server.sock.onmessage({ data: JSON.stringify({ id: obj.id, error: { message: 'boom' } }) });
  },
  pushError(sid, msg) {
    this.pushNotif('chat_error', { session_id: sid, error: msg || 'boom' });
  },
};
global.WebSocket = function () {
  this.readyState = 1;
  server.sock = this;
  this.send = (s) => server.receive(JSON.parse(s));
  setTimeout(() => this.onopen(), 0);
};

const Markdown = require('../markdown.js');
const ChatState = require('../state.js');
global.Markdown = Markdown;
global.ChatState = ChatState;
global.self = global;

const appSrc = fs.readFileSync(require('node:path').join(__dirname, '..', 'app.js'), 'utf8');
eval(appSrc);

function tick() { return new Promise((r) => setTimeout(r, 20)); }
function tabLabels() {
  return els['tab-list'].children.map((tab) => tab.children[0].textContent);
}
function msgTexts() {
  function text(n) {
    let t = n.textContent || '';
    if (!t && typeof n.innerHTML === 'string') t = n.innerHTML.replace(/<[^>]*>/g, ' ');
    (n.children || []).forEach((c) => { t += '|' + text(c); });
    return t;
  }
  return els.messages.children.map((m) => text(m));
}

(async () => {
  await tick(); // connect + session_list + openSession(A)
  await tick();
  assert.deepEqual(tabLabels(), ['first', '(new)'], 'tabs render, titles shown');

  // Open B, send, switch mid-flight.
  els['tab-list'].children[1].children[0].click();
  await tick();
  assert.ok(msgTexts().join(' ').includes('q-B'), 'B history shown after switch');
  els.composer.value = 'hello-B2';
  els['send-btn'].onclick();
  assert.ok(tabLabels()[1].startsWith('\u2026'), 'B tab shows waiting glyph');
  els['tab-list'].children[0].children[0].click(); // back to A mid-flight
  await tick();
  assert.ok(msgTexts().join(' ').includes('a-A'), 'A history intact, no leak from B flight');

  // Resolve B's chat while viewing A: A untouched, B flagged unread.
  const chatMsg = server.pendingChat.pop();
  server.resolveChat(chatMsg, 'reply-B2');
  await tick();
  assert.ok(!msgTexts().join(' ').includes('reply-B2'), 'late reply NOT shown in wrong tab');
  assert.ok(tabLabels()[1].startsWith('\u25cf'), 'B tab flagged unread');

  // Open B: reply shown, flag cleared, fork works on canonical indices.
  els['tab-list'].children[1].children[0].click();
  await tick(); await tick();
  assert.ok(msgTexts().join(' ').includes('reply-B2'), 'reply shown after opening B');
  assert.ok(!tabLabels()[1].startsWith('\u25cf'), 'flag cleared on open');
  const forkBtns = [];
  (function walk(nodes) {
    nodes.forEach((n) => {
      if (n.textContent === '⤴ Fork') forkBtns.push(n);
      if (n.children) walk(n.children);
    });
  })(els.messages.children);
  assert.ok(forkBtns.length >= 1, 'fork buttons present');
  sent.length = 0;
  forkBtns[forkBtns.length - 1].onclick();
  await tick();
  const forkCall = sent.find((s) => s.method === 'session_fork');
  assert.ok(forkCall, 'fork RPC fired');
  assert.equal(forkCall.params.message_index, 3, 'fork raw index maps to reply-B2');

  // Rename via inline editor, then X two-tap delete (no confirm()).
  const tabsNow = els['tab-list'].children;
  const bIdx = tabsNow.findIndex((t) => t.children[0].textContent.includes('(new)'));
  assert.ok(bIdx >= 0, 'B tab found');
  const bTab = tabsNow[bIdx];
  const renBtn = bTab.children.find((c) => c.className === 'tab-rename');
  renBtn.click();
  const editor = bTab.children.find((c) => c.className === 'tab-edit');
  assert.ok(editor, 'rename editor appears');
  editor.value = 'Budget chat';
  editor.fire('keydown', { key: 'Enter', stopPropagation() {} });
  await tick();
  const renCall = sent.find((s) => s.method === 'session_rename');
  assert.ok(renCall && renCall.params.title === 'Budget chat', 'rename RPC fired');
  server.sock.onmessage({ data: JSON.stringify({ id: renCall.id, result: { id: renCall.params.session_id, title: 'Budget chat' } }) });
  await tick();
  assert.ok(tabLabels().some((t) => t.includes('Budget chat')), 'renamed title shown');
  const tapClose = () => {
    const tabs = els['tab-list'].children;
    const idx = tabs.findIndex((t) => t.children.some((c) => c.textContent.includes('Budget chat')));
    assert.ok(idx >= 0, 'armed tab still present');
    tabs[idx].children.find((c) => c.className === 'tab-close').click();
  };
  tapClose();
  const armedBtn = els['tab-list'].children
    .flatMap((t) => t.children)
    .find((c) => c.className === 'tab-close' && c.textContent === 'Sure?');
  assert.ok(armedBtn, 'first tap arms');
  sent.length = 0;
  tapClose();
  await tick();
  assert.ok(sent.find((s) => s.method === 'session_delete'), 'second tap deletes');
  assert.equal(tabLabels().length, 2, 'B tab removed from strip (fork tab + A remain)');

  // Closed-tab resume: history panel lists trash, Restore reopens with messages.
  els['history-btn'].onclick();
  await tick();
  assert.ok(sent.find((s) => s.method === 'session_closed'), 'closed list fetched');
  const restoreBtn = (function findRestore(nodes) {
    for (const n of nodes) {
      if (n.textContent === 'Restore') return n;
      if (n.children) { const f = findRestore(n.children); if (f) return f; }
    }
    return null;
  })(els['closed-list'].children);
  assert.ok(restoreBtn, 'restore button present');
  restoreBtn.onclick();
  await tick(); await tick();
  assert.ok(tabLabels().some((t) => t.includes('Budget chat')), 'restored tab back in strip');
  assert.ok(msgTexts().join(' ').includes('reply-B2'), 'restored session shows its messages');

  // Error path: sent bubble stays, thinking goes away, composer untouched,
  // error bubble carries Retry; the failed turn is NOT persisted server-side.
  const aIdx = tabLabels().findIndex((t) => t.includes('first'));
  els['tab-list'].children[aIdx].children[0].click(); // open A
  await tick(); await tick();
  els.composer.value = 'doomed-q';
  server.failNextChat = true;
  els['send-btn'].onclick();
  await tick(); await tick(); await tick(); await tick();
  assert.equal(els.composer.value, '', 'composer NOT clobbered by failure');
  assert.ok(msgTexts().join(' ').includes('doomed-q'), 'sent bubble stays in thread');
  assert.ok(!msgTexts().join(' ').includes('Thinking'), 'thinking bubble torn down');
  assert.ok(msgTexts().join(' ').includes('boom'), 'error bubble shown');
  const dupes = server.sessions.A.filter((m) => m.content === 'doomed-q').length;
  assert.equal(dupes, 0, 'failed turn NOT persisted server-side');
  // Retry via the error bubble resends the last user turn exactly once.
  const retryBtn = (function findRetry(nodes) {
    for (const n of nodes) {
      if (n.textContent === '↻ Retry') return n;
      if (n.children) { const f = findRetry(n.children); if (f) return f; }
    }
    return null;
  })(els.messages.children);
  assert.ok(retryBtn, 'retry button present on error bubble');
  retryBtn.onclick({ stopPropagation() {} });
  await tick(); await tick();
  const retryChat = server.pendingChat.pop();
  server.resolveChat(retryChat, 'retry-ok');
  await tick(); await tick();
  assert.equal(server.sessions.A.filter((m) => m.content === 'doomed-q').length, 1, 'retry stores exactly once');
  assert.ok(msgTexts().join(' ').includes('retry-ok'), 'retry reply shown');

  // Background failure (post-accept error push) flags '!' until opened.
  els.composer.value = 'bg-doom';
  els['send-btn'].onclick();
  const bgErr = server.pendingChat.pop();
  const bi = tabLabels().findIndex((t) => t.includes('fork'));
  els['tab-list'].children[bi].children[0].click(); // move to fork tab
  await tick();
  server.pushError(bgErr.params.session_id, 'boom');
  await tick(); await tick();
  assert.ok(tabLabels().some((t) => t.startsWith('!')), 'background failed tab flagged !');

  // Fresh tab: double-send blocked (single flight).
  els['new-session'].onclick();
  await tick();
  const chatsBefore = sent.filter((s) => s.method === 'chat').length;
  els.composer.value = 'fresh-1';
  els['send-btn'].onclick();
  els.composer.value = 'fresh-2';
  els['send-btn'].onclick();
  assert.equal(sent.filter((s) => s.method === 'chat').length, chatsBefore + 1, 'double send on fresh tab blocked');
  server.resolveChat(server.pendingChat.pop(), 'fresh-ok');
  await tick(); await tick();

  // Disconnect mid-flight: stream resumes with backoff; the sent bubble
  // and queued turn survive, no stuck tab, no composer clobber.
  const sockNow = server.sock;
  els.composer.value = 'disc-q';
  els['send-btn'].onclick();
  await tick(); await tick();
  assert.equal(server.pendingChat.length, 1, 'disconnect test chat in flight');
  sockNow.onclose();
  await tick(); await tick();
  assert.ok(msgTexts().join(' ').includes('disc-q'), 'sent bubble survives disconnect');
  assert.equal(els.composer.value, '', 'disconnect does NOT clobber composer');
  // Finish the disconnect-block flight so later sends are not queued behind it.
  server.resolveChat(server.pendingChat.pop(), 'disc-ok');
  await tick(); await tick();

  // Live streaming on the active tab: chunks paint, tool lines show, done reloads.
  // NOTE: the disconnect block above reconnects (new server.sock), so drain
  // any pre-reconnect leftovers first, then use the fresh socket's queue.
  server.pendingChat.length = 0;
  const ai = tabLabels().findIndex((t) => t.includes('first'));
  els['tab-list'].children[ai].children[0].click();
  await tick(); await tick();
  els.composer.value = 'stream me';
  els['send-btn'].onclick();
  let sc = null;
  for (let i = 0; i < 40 && !sc; i++) { await tick(); sc = server.pendingChat.pop(); }
  assert.ok(sc, 'stream chat accepted after reconnect');
  const scSid = sc.params.session_id;

  // Streaming into the *background*, then coming back to it.
  //
  // A frame queued while the tab was hidden is dropped, not painted into a
  // bubble the user is not looking at. So the text only reappears when the tab
  // is reopened, and that reopen must paint synchronously: the user just
  // clicked, so deferring it a frame reads as a broken tab.
  server.pushChunk(scSid, 'mid-stream ');
  await tick();
  const streamTab = tabLabels().findIndex((l) => l.includes('first'));
  const otherTab = tabLabels().findIndex((l) => l.includes('fork'));
  assert.ok(streamTab >= 0 && otherTab >= 0 && otherTab !== streamTab,
    'the forked session tab exists to switch to');
  // Switch away with a paint still queued, then let the frame run. The reply
  // belongs to the tab the user just left, so it must not paint into the tab
  // now in front of them.
  els['tab-list'].children[otherTab].children[0].click();
  await tick();
  flushFrames();
  assert.ok(!msgTexts().join(' ').includes('mid-stream'),
    'a frame pending when the tab was left does not paint into the new tab');
  const queuedWhileAway = rafQueue.length;
  els['tab-list'].children[streamTab].children[0].click();
  await tick();
  assert.ok(msgTexts().join(' ').includes('mid-stream'),
    'opening a streaming tab shows its accumulated text');
  assert.equal(rafQueue.length, queuedWhileAway,
    'opening a streaming tab must paint synchronously, not defer to a frame');

  server.pushChunk(scSid, 'hel');
  await tick();
  server.pushChunk(scSid, 'lo world');
  await tick();
  assert.equal(rafQueue.length, 1, 'a burst of chunks queues one frame, not one per chunk');
  flushFrames();
  assert.ok(msgTexts().join(' ').includes('hello world'), 'streamed chunks paint live');
  server.pushTool(scSid, 'start', { name: 'bash_executor' });
  await tick();
  server.pushTool(scSid, 'result', { preview: 'ok' });
  await tick();
  assert.ok(msgTexts().join(' ').includes('bash_executor'), 'tool activity shown');
  server.sessions[scSid].push({ role: 'user', content: 'stream me' }, { role: 'assistant', content: 'hello world' });
  server.pushDone(scSid, 'hello world');
  await tick(); await tick();
  assert.ok(msgTexts().join(' ').includes('hello world'), 'done reload shows reply');

  // ---- coalescing: the property this change is about ------------------
  //
  // Before, every chunk re-rendered the whole accumulated reply, so an
  // N-token reply cost N renders of growing text. Chunks now queue at most
  // one paint, and the paint shows the complete buffer — coalescing must be
  // invisible to the user, not lossy.

  let sc2 = null;
  els.composer.value = 'long reply please';
  els['send-btn'].onclick();
  for (let i = 0; i < 40 && !sc2; i++) { await tick(); sc2 = server.pendingChat.pop(); }
  assert.ok(sc2, 'second stream accepted');
  const sid2 = sc2.params.session_id;

  let maxQueued = 0;
  for (let i = 0; i < 50; i++) {
    server.pushChunk(sid2, `tok${i} `);
    maxQueued = Math.max(maxQueued, rafQueue.length);
    await tick();
  }
  // The whole burst fits in a single frame; without coalescing this would be 50.
  assert.equal(maxQueued, 1, `expected at most one queued frame, saw ${maxQueued}`);
  assert.equal(rafQueue.length, 1, 'still exactly one pending frame');
  flushFrames();
  {
    const text = msgTexts().join(' ');
    assert.ok(text.includes('tok0'), 'first token painted');
    assert.ok(text.includes('tok49'), 'last token painted');
    assert.ok(text.includes('tok0') && text.includes('tok25') && text.includes('tok49'),
      'a coalesced paint must show every accumulated token, not just the first');
  }

  // A queued paint must not resurrect a bubble after the reply is torn down.
  server.pushChunk(sid2, 'late');
  server.sessions[sid2].push({ role: 'user', content: 'long reply please' }, { role: 'assistant', content: 'tok0 tok1' });
  server.pushDone(sid2, 'tok0 tok1');
  await tick(); await tick();
  const before = msgTexts().join(' ');
  flushFrames();
  assert.equal(msgTexts().join(' '), before, 'a frame queued before teardown repainted a removed bubble');

  console.log('TABFLOW ALL PASS');

})().catch((e) => { console.error('TABFLOW FAIL:', String((e && e.stack) || e).slice(0, 400)); process.exit(1); });