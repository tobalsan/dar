const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');

function browser() {
  const handlers = {}, calls = [];
  const attrs = {};
  const element = id => {
    let content = '';
    return {
      id, dataset: {}, style: { setProperty() {} }, value: '', hidden: false, disabled: false,
      scrollHeight: 10, scrollTop: 0, clientHeight: 10,
      get innerHTML() { return content; }, set innerHTML(v) { content = v; },
      get textContent() { return content; }, set textContent(v) { content = v; },
      setAttribute(k, v) { attrs[k] = String(v); }, removeAttribute(k) { delete attrs[k]; },
      querySelectorAll: () => [], getBoundingClientRect: () => ({ top: 0 }), focus() {},
    };
  };
  const ids = ['chat-root', 'chat-transcript', 'chat-input', 'chat-chips', 'chat-send', 'chat-abort', 'chat-token-meter', 'chat-slash-menu'];
  const elements = Object.fromEntries(ids.map(id => [id, element(id)]));
  elements['chat-slash-menu'].hidden = true;
  const sandbox = {
    document: { getElementById: id => elements[id] || null, addEventListener: (t, fn) => (handlers[t] ||= []).push(fn) },
    window: { innerHeight: 1000, addEventListener() {} },
    queueMicrotask, crypto: { randomUUID: () => 'command' }, EventSource: function () { this.close = () => {}; },
    setTimeout, clearTimeout, setInterval() {},
    fetch: async (url, options) => { calls.push({ url, options }); return { ok: true, status: 202, json: async () => (url.endsWith('/new') ? { slot: 'fresh' } : {}), text: async () => '{}' }; },
  };
  vm.runInContext(source, vm.createContext(sandbox));
  const input = elements['chat-input'];
  const fire = (type, extra = {}) => { const e = { target: input, preventDefault() { e.prevented = true; }, ...extra }; for (const fn of handlers[type] || []) fn(e); return e; };
  return {
    calls, attrs, menu: elements['chat-slash-menu'], input, app: () => sandbox.window.__chatWeb,
    type(value) { input.value = value; fire('input'); },
    key(key, extra = {}) { return fire('keydown', { key, ...extra }); },
    clickOption(name) { fire('click', { target: { closest: s => s === '.chat-slash-option' ? { dataset: { command: name } } : null } }); },
    options: () => [...elements['chat-slash-menu'].innerHTML.matchAll(/data-command="([^"]+)"/g)].map(m => m[1]),
  };
}

const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };

(async () => {
  let p = browser();
  p.type('/');
  assert.deepEqual(p.options(), ['/new', '/compact', '/stop']);
  assert.equal(p.attrs['aria-expanded'], 'true');
  assert.equal(p.attrs['aria-activedescendant'], 'chat-slash-0');
  p.type('/co');
  assert.deepEqual(p.options(), ['/compact']);
  p.type('/zzz');
  assert.equal(p.menu.hidden, true, 'hidden with no match');
  p.type('hello');
  assert.equal(p.menu.hidden, true, 'hidden for plain text');
  p.type('/compact ');
  assert.equal(p.menu.hidden, true, 'hidden once a space follows the command');
  p.type('/stop');
  assert.equal(p.menu.hidden, true, 'hidden on exact match so Enter sends');

  p = browser();
  p.type('/');
  p.key('ArrowDown');
  assert.equal(p.attrs['aria-activedescendant'], 'chat-slash-1');
  p.key('ArrowUp'); p.key('ArrowUp');
  assert.equal(p.attrs['aria-activedescendant'], 'chat-slash-2', 'selection wraps');
  p.key('ArrowDown'); p.key('ArrowDown');
  let e = p.key('Tab');
  assert.equal(e.prevented, true);
  assert.equal(p.input.value, '/compact ');
  assert.equal(p.menu.hidden, true);

  p = browser();
  p.type('/n');
  e = p.key('Enter');
  assert.equal(p.input.value, '/new ', 'Enter completes a partial command');
  assert.equal(p.calls.length, 0, 'completing does not send');

  p = browser();
  p.type('/');
  p.key('Escape');
  assert.equal(p.menu.hidden, true);
  p.type('/c');
  assert.equal(p.menu.hidden, false, 'typing reopens after Escape');

  p = browser();
  p.type('/');
  e = p.key('Enter', { isComposing: true });
  assert.notEqual(e.prevented, true, 'IME composition is left alone');
  e = p.key('Enter', { shiftKey: true });
  assert.notEqual(e.prevented, true, 'Shift+Enter stays a newline');

  p = browser();
  p.type('/');
  p.clickOption('/stop');
  assert.equal(p.input.value, '/stop ');

  p = browser();
  p.type('/compact keep the plan');
  p.key('Enter');
  await settle();
  assert.equal(p.calls[0].url, '/chat/main/compact');
  assert.equal(JSON.parse(p.calls[0].options.body).instructions, 'keep the plan');

  p = browser();
  p.type('/');
  p.key('Enter', { isComposing: true });
  p.calls.length = 0;
  p.app().newChat();
  await settle();
  assert.equal(p.menu.hidden, true, 'switching chats closes the menu');
  p.key('Enter');
  assert.equal(p.input.value, '', 'Enter after a chat switch does not complete a stale command');
})().catch(err => { console.error(err); process.exit(1); });
