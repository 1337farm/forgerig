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
    setAttribute(k, v) { this.attrs[k] = v; },
    getAttribute(k) { return this.attrs[k]; },
    removeAttribute(k) { delete this.attrs[k]; },
    focus() {},
  };
  let html = '';
  Object.defineProperty(el, 'innerHTML', {
    get() { return html; },
    set(v) { html = v; el.children = []; },
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
global.window = { addEventListener: (k, f) => { listeners[k] = f; } };
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
    let t = n.textContent || n.innerHTML || '';
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
  els['tab-list'].children[1].children[0].onclick();
  await tick();
  assert.ok(msgTexts().join(' ').includes('q-B'), 'B history shown after switch');
  els.composer.value = 'hello-B2';
  els['send-btn'].onclick();
  assert.ok(tabLabels()[1].startsWith('\u2026'), 'B tab shows waiting glyph');
  els['tab-list'].children[0].children[0].onclick(); // back to A mid-flight
  await tick();
  assert.ok(msgTexts().join(' ').includes('a-A'), 'A history intact, no leak from B flight');

  // Resolve B's chat while viewing A: A untouched, B flagged unread.
  const chatMsg = server.pendingChat.pop();
  server.resolveChat(chatMsg, 'reply-B2');
  await tick();
  assert.ok(!msgTexts().join(' ').includes('reply-B2'), 'late reply NOT shown in wrong tab');
  assert.ok(tabLabels()[1].startsWith('\u25cf'), 'B tab flagged unread');

  // Open B: reply shown, flag cleared, fork works on canonical indices.
  els['tab-list'].children[1].children[0].onclick();
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
  renBtn.onclick({ stopPropagation() {} });
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
  const xBtn = bTab.children.find((c) => c.className === 'tab-close');
  xBtn.onclick({ stopPropagation() {} });
  assert.equal(xBtn.textContent, 'Sure?', 'first tap arms');
  sent.length = 0;
  xBtn.onclick({ stopPropagation() {} });
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

  // Error path: text restored to composer (undo), error bubble, no server dup.
  const aIdx = tabLabels().findIndex((t) => t.includes('first'));
  els['tab-list'].children[aIdx].children[0].onclick(); // open A
  await tick(); await tick();
  els.composer.value = 'doomed-q';
  server.failNextChat = true;
  els['send-btn'].onclick();
  await tick(); await tick(); await tick();
  assert.equal(els.composer.value, 'doomed-q', 'failed text restored to composer');
  assert.ok(msgTexts().join(' ').includes('boom'), 'error bubble shown');
  const dupes = server.sessions.A.filter((m) => m.content === 'doomed-q').length;
  assert.equal(dupes, 0, 'failed turn NOT persisted server-side');
  // Retry from the restored composer yields exactly one copy.
  els['send-btn'].onclick();
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
  els['tab-list'].children[bi].children[0].onclick(); // move to fork tab
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

  // Disconnect mid-flight: pending fails into composer restore, no stuck tab.
  const sockNow = server.sock;
  els.composer.value = 'disc-q';
  els['send-btn'].onclick();
  assert.equal(server.pendingChat.length, 1, 'disconnect test chat in flight');
  sockNow.onclose();
  await tick(); await tick();
  assert.equal(els.composer.value, 'disc-q', 'disconnect restores composer');
  assert.ok(msgTexts().join(' ').includes('disconnected'), 'disconnect error shown');

  // Live streaming on the active tab: chunks paint, tool lines show, done reloads.
  const ai = tabLabels().findIndex((t) => t.includes('first'));
  els['tab-list'].children[ai].children[0].onclick();
  await tick(); await tick();
  els.composer.value = 'stream me';
  els['send-btn'].onclick();
  const sc = server.pendingChat.pop();
  const scSid = sc.params.session_id;
  server.pushChunk(scSid, 'hel');
  await tick();
  server.pushChunk(scSid, 'lo world');
  await tick();
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

  console.log('TABFLOW ALL PASS');

})().catch((e) => { console.error('TABFLOW FAIL:', String((e && e.stack) || e).slice(0, 400)); process.exit(1); });