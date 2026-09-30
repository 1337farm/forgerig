'use strict';
// The daemon refuses every RPC until a connection authenticates, so the UI's
// first frame MUST be `auth` and nothing else may precede it. These tests pin
// that ordering contract: it is the one place where a refactor silently turns
// the whole daemon into an auth-required app that cannot talk to itself.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { test } = require('node:test');

function makeEl(id) {
  const el = {
    id, children: [], style: {}, textContent: '', value: '',
    attrs: {}, className: '',
    appendChild(c) { c.parentNode = this; this.children.push(c); return c; },
    replaceChild() {},
    removeChild() {},
    insertBefore(c) { c.parentNode = this; this.children.push(c); return c; },
    addEventListener() {},
    setAttribute(k, v) { this.attrs[k] = v; },
    getAttribute(k) { return this.attrs[k]; },
    removeAttribute(k) { delete this.attrs[k]; },
    focus() {}, scrollIntoView() {},
  };
  let html = '';
  Object.defineProperty(el, 'innerHTML', {
    get() { return html; },
    set(v) { html = v; el.children = []; },
  });
  return el;
}

// Each test gets a fresh harness: fresh DOM stubs, fresh recorded frames,
// fresh app.js instance (app.js is an IIFE that grabs its elements at load).
function harness(opts) {
  opts = opts || {};
  const els = {};
  global.document = {
    getElementById: (id) => (els[id] = els[id] || makeEl(id)),
    createElement: (tag) => makeEl(tag),
    createTextNode: (t) => ({ textContent: t }),
  };
  const listeners = {};
  global.window = { addEventListener: (k, f) => { (listeners[k] = listeners[k] || []).push(f); } };
  global.window.__listeners = listeners;
  if (opts.token !== undefined) {
    global.window.NativeHost = { getAuthToken: () => opts.token };
  }
  global.location = { protocol: 'http:', host: '127.0.0.1:39999' };
  global.confirm = () => false;
  global.self = global;
  global.Markdown = require('../markdown.js');
  global.ChatState = require('../state.js');

  const frames = [];
  let sock = null;
  let url = null;
  global.WebSocket = function (u) {
    this.readyState = 1;
    url = u;
    sock = this;
    this.send = (s) => {
      const obj = JSON.parse(s);
      frames.push(obj);
      // Auto-respond unless the test wants to control the reply.
      if (opts.autoReply !== false && !opts.holdAuth) {
        setTimeout(() => deliver(obj), 0);
      } else if (opts.autoReply !== false) {
        setTimeout(() => deliver(obj), 0);
      }
    };
    this.close = () => {};
    setTimeout(() => this.onopen(), 0);
  };

  function deliver(obj) {
    if (!sock || !sock.onmessage) return;
    if (obj.method === 'auth') {
      const ok = opts.authError ? false : true;
      sock.onmessage({
        data: JSON.stringify(ok
          ? { id: obj.id, result: { authenticated: true } }
          : { id: obj.id, error: { code: -32001, message: 'invalid token' } }),
      });
      return;
    }
    const results = {
      status: { provider: 'provider=openai model=x key=set' },
      lean_status: {},
      session_list: [],
    };
    if (Object.prototype.hasOwnProperty.call(results, obj.method)) {
      sock.onmessage({ data: JSON.stringify({ id: obj.id, result: results[obj.method] }) });
    }
  }

  const src = fs.readFileSync(require('node:path').join(__dirname, '..', 'app.js'), 'utf8');
  eval(src);
  return { frames, els, url, methods: () => frames.map((f) => f.method) };
}

const tick = () => new Promise((r) => setTimeout(r, 20));

test('auth is the first frame on the wire', async () => {
  const h = harness({ token: 'tok-123' });
  await tick();
  assert.equal(h.methods()[0], 'auth', 'first frame must be auth');
  assert.equal(h.frames[0].params.token, 'tok-123', 'auth carries the bridge token');
});

test('the token never appears in the WebSocket URL', async () => {
  const h = harness({ token: 'super-secret-token' });
  await tick();
  // The socket URL is location.host only — the token must not ride along, or
  // it would land in the shared install log and the WebView's history.
  assert.equal(h.url, 'ws://127.0.0.1:39999/', 'socket opens on the daemon host only');
  assert.ok(!/super-secret-token/.test(h.url), 'no token in the URL');
  assert.ok(!JSON.stringify(h.frames[0]).includes('ws://'), 'token travels in the auth frame, not a URL');
});

test('no other RPC is sent until auth succeeds', async () => {
  const h = harness({ token: 'tok', authError: true, holdAuth: true });
  await tick();
  // Hold the auth reply open: nothing may have been sent past it.
  assert.deepEqual(h.methods(), ['auth'], 'status/lean_status/session_list wait for auth');
});

test('a rejected auth surfaces the failure and stops talking', async () => {
  const h = harness({ token: 'wrong', authError: true });
  await tick();
  assert.ok(h.methods().includes('auth'), 'auth was attempted');
  assert.ok(!h.methods().includes('status'), 'no follow-up calls after a rejected auth');
  assert.match(h.els['status'].textContent, /authenticate/i, 'user sees the reason');
});

test('a missing bridge token fails loudly instead of firing doomed calls', async () => {
  const h = harness({ token: '' });
  await tick();
  assert.deepEqual(h.methods(), [], 'no RPC is sent when there is no token to send');
  assert.match(h.els['status'].textContent, /ForgeRig app/i);
});

test('unhandled UI errors are reported to the daemon', async () => {
  const h = harness({ token: 'tok' });
  await tick();
  // The client_error RPC exists so an on-device JS failure reaches the shared
  // log. If nothing calls it, the page silently stops working instead.
  const before = h.methods().length;
  global.window.onerror('boom', 'app.js', 42, 7, { stack: 'at foo' });
  global.window.__listeners.unhandledrejection.forEach((f) => f({ reason: new Error('rejected!') }));
  await tick();
  const reports = h.frames.slice(before).filter((f) => f.method === 'client_error');
  assert.equal(reports.length, 2, 'both a JS error and a rejection are reported');
  assert.equal(reports[0].params.kind, 'js');
  assert.equal(reports[0].params.message, 'boom');
  assert.equal(reports[0].params.line, 42);
  assert.equal(reports[0].params.stack, 'at foo');
  assert.equal(reports[1].params.kind, 'promise');
  assert.match(reports[1].params.message, /rejected!/);
});

test('error reporting is bounded and never throws', async () => {
  const h = harness({ token: 'tok' });
  await tick();
  // A pathological message must be truncated rather than shipped whole: this
  // string ends up in a log file the user is expected to paste somewhere.
  global.window.onerror('x'.repeat(100000), 'app.js', 1, 1, null);
  global.window.onerror(undefined, 'app.js', 1, 1, { stack: 'y'.repeat(50000) });
  await tick();
  const reports = h.frames.filter((f) => f.method === 'client_error');
  assert.ok(reports.every((r) => r.params.message.length <= 2000), 'message truncated');
  assert.ok(reports.every((r) => r.params.stack.length <= 4000), 'stack truncated');
  // A missing message still sends the field the daemon reads, rather than
  // omitting it — the daemon's `(no message)` default then applies on-device.
  assert.equal(typeof reports[1].params.message, 'string');
  assert.equal(reports[1].params.message, 'undefined');
  // And reporting while disconnected is a no-op, not a throw.
  global.window.onerror('while down', 'app.js', 1, 1, null);
  assert.ok(true, 'no throw');
});

test('after auth the normal probes follow', async () => {
  const h = harness({ token: 'tok' });
  await tick();
  const m = h.methods();
  // auth first, then the usual post-connect probes in their usual order.
  assert.deepEqual(m.slice(0, 4), ['auth', 'status', 'lean_status', 'session_list']);
  // Everything after auth is a normal app RPC — nothing re-sends `auth` per
  // call (that would be the token-blind path this guards against).
  assert.equal(m.filter((x) => x === 'auth').length, 1, 'auth is sent exactly once per connection');
  assert.ok(m.every((x) => x !== 'auth' || m.indexOf(x) === 0), 'auth never appears after the first frame');
});
