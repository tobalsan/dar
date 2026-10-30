const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');
const { reduce, html } = require(process.argv[2]);

const running = reduce([], { type: 'compaction_started', command: '/compact keep the plan' });
assert.equal(running.length, 1);
assert.equal(running[0].kind, 'compaction');
assert.match(html(running), /Compacting context\.\.\./);
assert.match(html(running), /\/compact keep the plan/);
assert.doesNotMatch(html(running), /chat-user|>You</);

const completed = reduce(running, { type: 'compaction_finished', ok: true, summary: '**Keep the plan**\n- Verify reload' });
assert.equal(completed.length, 1);
assert.equal(completed[0].state, 'done');
assert.match(html(completed), /Context compacted\./);
assert.match(html(completed), /<details[^>]*><summary>Compaction summary<\/summary>/);
assert.match(html(completed), /<strong>Keep the plan<\/strong>/);
assert.doesNotMatch(html(completed), /Compacting context|<details[^>]*\bopen\b/);
const resumed = reduce([], { type: 'compaction_finished', ok: true, summary: 'Earlier context' });
assert.match(html(resumed), /Compaction summary/);
assert.doesNotMatch(html(resumed), /<code>\/compact|Context compacted/);

const failure = reduce(reduce([], { type: 'compaction_started', command: '/compact' }), { type: 'compaction_finished', ok: false, error: 'provider unavailable <retry>' });
assert.equal(failure.length, 1);
assert.match(html(failure), /provider unavailable &lt;retry&gt;/);
assert.doesNotMatch(html(failure), /turn failed|Context compacted|Compaction summary/);
for (const event of [{ type: 'compaction_finished', ok: false, error: 'aborted' }, { type: 'aborted', error: 'aborted' }, { type: 'closed', error: 'connection lost' }]) {
  const stopped = reduce(reduce([], { type: 'compaction_started', command: '/compact' }), event);
  assert.equal(stopped.length, 1, 'terminal state stays in the command row');
  assert.doesNotMatch(html(stopped), /Compacting context|chat-turn/);
  assert.match(html(stopped), event.type === 'closed' ? /connection lost/ : /Interrupted/);
}
const stoppedRow = reduce(reduce([], { type: 'compaction_started', command: '/compact' }), { type: 'compaction_finished', ok: false, error: 'aborted' });
assert.equal(reduce(stoppedRow, { type: 'aborted', error: 'aborted' }).length, 1, 'the terminal wire event does not add another interrupted block');
assert.equal(reduce(reduce([], { type: 'compaction_started', command: '/compact' }), { type: 'finished' })[0].state, 'done');

function browser(respond, { humanChat = false } = {}) {
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
  if (humanChat) elements['chat-root'].dataset.humanChat = '';
  const sandbox = {
    document: { getElementById: id => elements[id] || null, addEventListener: (t, fn) => (handlers[t] ||= []).push(fn) },
    window: { innerHeight: 1000, addEventListener() {} },
    queueMicrotask, crypto: { randomUUID: () => 'command' }, EventSource: function () { this.close = () => {}; },
    setTimeout, clearTimeout, setInterval() {},
    fetch: async (url, options) => { calls.push({ url, options }); if (respond) return respond(url, options); return { ok: true, status: 202, json: async () => (url.endsWith('/new') ? { slot: 'fresh' } : {}), text: async () => '{}' }; },
  };
  vm.runInContext(source, vm.createContext(sandbox));
  const input = elements['chat-input'];
  const fire = (type, extra = {}) => { const e = { target: input, preventDefault() { e.prevented = true; }, ...extra }; for (const fn of handlers[type] || []) fn(e); return e; };
  return {
    calls, attrs, menu: elements['chat-slash-menu'], input, app: () => sandbox.window.__chatWeb,
    event: event => sandbox.window.renderChatEvent(event),
    transcript: () => elements['chat-transcript'].innerHTML,
    abort: elements['chat-abort'],
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
  p.type('/t');
  assert.equal(p.menu.hidden, true, '/topics is hidden when human-chat is off');

  p = browser(null, { humanChat: true });
  p.type('/');
  assert.deepEqual(p.options(), ['/new', '/compact', '/stop', '/topics']);
  p.type('/t');
  p.key('Tab');
  assert.equal(p.input.value, '/topics ');

  p = browser();
  p.type('/');
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
  p.type('/compact'); p.key('Enter');
  p.type('/compact again'); p.key('Enter');
  await settle();
  assert.equal(p.calls.length, 1, 'double Enter sends no second request before SSE or the POST settles');
  assert.equal(p.input.value, '/compact again', 'refused command remains in the composer');
  assert.match(p.transcript(), /compaction running/);
  assert.equal(p.abort.hidden, false, 'Stop is available immediately');
  p.type('next message'); p.key('Enter');
  await settle();
  assert.equal(p.calls.length, 1, 'a refused command does not unlock plain messages');
  p.event({ type: 'compaction_started', command: '/compact' });
  p.event({ type: 'compaction_finished', ok: true, summary: 'Kept context' });
  p.event({ type: 'finished' });
  p.type('next message'); p.key('Enter');
  await settle();
  assert.equal(p.calls[1].url, '/chat/main/send', 'terminal event unlocks the next message');

  let rejectCompact;
  p = browser(() => new Promise((_, reject) => { rejectCompact = reject; }));
  p.type('/compact'); p.key('Enter');
  p.event({ type: 'compaction_started', command: '/compact' });
  rejectCompact(new Error('connection lost'));
  await settle();
  p.type('next message'); p.key('Enter');
  await settle();
  assert.equal(p.calls.length, 1, 'a failed POST response cannot unlock a compaction already confirmed by SSE');

  p = browser(async () => ({ ok: false, status: 409, text: async () => JSON.stringify({ error: 'wait for the current response to finish' }) }));
  p.type('/compact'); p.key('Enter');
  await settle();
  assert.equal(p.input.value, '/compact', 'server rejection restores the command');
  p.type('next message'); p.key('Enter');
  await settle();
  assert.equal(p.calls[1].url, '/chat/main/send', 'a rejected command without a running row releases the composer');

  p = browser(() => new Promise(() => {}));
  p.type('/compact'); p.key('Enter');
  p.event({ type: 'compaction_started', command: '/compact' });
  p.type('/stop'); p.key('Enter');
  assert.equal(p.calls[1].url, '/chat/main/abort', 'Stop remains usable while the POST hangs');
  p.event({ type: 'compaction_finished', ok: false, error: 'aborted' });
  p.event({ type: 'aborted', error: 'aborted' });
  p.type('continue'); p.key('Enter');
  assert.equal(p.calls[2].url, '/chat/main/send', 'a terminal event releases the composer even when the original POST hangs');

  p = browser();
  p.event({ type: 'compaction_started', command: '/compact', historical: true });
  p.event({ type: 'reset' });
  p.type('fresh'); p.key('Enter');
  assert.equal(p.calls[0].url, '/chat/main/send', 'reset clears the pending command');

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
