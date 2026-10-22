const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const session = (id, label, extra = {}) => ({ id, label, modified_ms: Date.now(), archived: false, ...extra });

function browser() {
  const handlers = {}, windowHandlers = {}, timers = new Map(), polls = [], calls = [];
  let clock = 0, timerId = 0;
  const element = id => {
    let content = '';
    return {
      id, dataset: {}, style: { setProperty() {} }, value: '', hidden: false,
      disabled: false, scrollHeight: 10, scrollTop: 0, clientHeight: 10,
      get innerHTML() { return content; }, set innerHTML(value) { content = value; },
      get textContent() { return content; }, set textContent(value) { content = value; },
      querySelectorAll: () => [], getBoundingClientRect: () => ({ top: 0 }),
    };
  };
  const elements = Object.fromEntries(['chat-root', 'chat-transcript', 'chat-input', 'chat-chips', 'chat-send', 'chat-abort', 'chat-token-meter', 'chat-history-list', 'chat-search'].map(id => [id, element(id)]));
  const listen = (list, type, fn) => (list[type] ||= []).push(fn);
  const document = {
    hidden: false, getElementById: id => elements[id] || null,
    addEventListener: (type, fn) => listen(handlers, type, fn),
  };
  const window = { innerHeight: 1000, addEventListener: (type, fn) => listen(windowHandlers, type, fn) };
  const sandbox = {
    document, window, queueMicrotask, crypto: { randomUUID: () => 'command' },
    EventSource: function () { this.close = () => {}; },
    setTimeout: (fn, delay) => { timers.set(++timerId, { fn, due: clock + delay }); return timerId; },
    clearTimeout: id => timers.delete(id),
    setInterval: (fn, delay) => { polls.push({ fn, delay }); },
    fetch: (url, options) => new Promise((resolve, reject) => calls.push({ url, options, resolve, reject })),
  };
  const context = vm.createContext(sandbox);
  const evaluate = () => vm.runInContext(source, context);
  evaluate();
  const emit = (type, target = elements['chat-search']) => { for (const fn of handlers[type] || []) fn({ target, preventDefault() {} }); };
  return {
    elements, document, calls, polls, evaluate,
    input(value) { elements['chat-search'].value = value; emit('input'); },
    emit,
    focus() { for (const fn of windowHandlers.focus || []) fn(); },
    action(action, id) {
      const target = { closest: selector => selector === '[data-session-action]' ? { dataset: { sessionAction: action, sessionId: id } } : null };
      emit('click', target);
    },
    newChat: () => window.__chatWeb.newChat(),
    async advance(ms) {
      clock += ms;
      for (const [id, timer] of timers) if (timer.due <= clock) { timers.delete(id); timer.fn(); }
      await settle();
    },
    async respond(call, entries) { call.resolve({ ok: true, json: async () => entries }); await settle(); },
    html: () => elements['chat-history-list'].innerHTML,
  };
}

async function debounced_server_search() {
  const page = browser();
  await page.respond(page.calls[0], [session('initial', 'Initial conversation')]);
  page.input('  old ');
  await page.advance(200);
  assert.equal(page.calls.length, 1, 'typing waits for the debounce');
  page.input('  tea & 雪  ');
  await page.advance(249);
  assert.equal(page.calls.length, 1, 'the latest input restarts the debounce');
  await page.advance(1);
  assert.equal(page.calls[1].url, '/chat/sessions?q=tea%20%26%20%E9%9B%AA');
  await page.respond(page.calls[1], [session('body-hit', 'Unrelated title')]);
  assert.match(page.html(), /Unrelated title/, 'body-only hits survive rendering');
}

async function changing_input_invalidates_pending_results() {
  for (const outcome of ['success', 'error']) {
    const page = browser();
    await page.respond(page.calls[0], [session('initial', 'Initial conversation')]);
    page.input('first');
    await page.advance(250);
    const stale = page.calls[1];
    page.input('second');
    if (outcome === 'success') await page.respond(stale, [session('stale', 'Stale title')]);
    else { stale.reject(new Error('stale failure')); await settle(); }
    assert.match(page.html(), /Initial conversation/, `stale ${outcome} is ignored during debounce`);
    assert.doesNotMatch(page.html(), /Stale title|stale failure/);
    await page.advance(250);
    await page.respond(page.calls[2], [session('latest', 'Latest title')]);
    assert.match(page.html(), /Latest title/);
  }
}

async function snippets_are_escaped() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.input('body');
  await page.advance(250);
  await page.respond(page.calls[1], [session('hit', 'Title only', { snippet: 'body <img src=x onerror="bad"> & 雪' })]);
  assert.ok(page.html().includes('body &lt;img src=x onerror=&quot;bad&quot;&gt; &amp; 雪'));
  assert.doesNotMatch(page.html(), /<img/);
}

async function polling_pauses_for_active_queries() {
  const page = browser();
  await page.respond(page.calls[0], []);
  assert.equal(page.polls.length, 1, 'browser mode installs polling');
  assert.equal(page.polls[0].delay, 5000);
  page.polls[0].fn();
  assert.equal(page.calls.length, 2, 'idle sidebar still polls');
  await page.respond(page.calls[1], []);
  page.input('needle');
  await page.advance(250);
  assert.equal(page.calls.length, 3);
  page.polls[0].fn();
  assert.equal(page.calls.length, 3, 'active search suppresses polling');
}

async function clearing_restores_groups_and_cancels_search() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.input('needle');
  await page.advance(250);
  const stale = page.calls[1];
  page.input('   ');
  assert.equal(page.calls[2].url, '/chat/sessions', 'clearing reloads immediately');
  await page.respond(page.calls[2], [session('active', 'Active conversation'), session('archived', 'Archived conversation', { archived: true })]);
  assert.match(page.html(), /chat-session-group/);
  assert.match(page.html(), /Active conversation/);
  assert.match(page.html(), /Archived \(1\)/);
  stale.reject(new Error('late search failure'));
  await settle();
  assert.doesNotMatch(page.html(), /late search failure/);
  page.input('cancel before debounce');
  page.input('');
  assert.equal(page.calls[3].url, '/chat/sessions');
  await page.advance(250);
  assert.equal(page.calls.length, 4, 'clearing cancels the pending query timer');
  page.document.hidden = true;
  page.polls[0].fn();
  assert.equal(page.calls.length, 4, 'hidden sidebar does not poll');
  page.document.hidden = false;
  page.polls[0].fn();
  assert.equal(page.calls[4].url, '/chat/sessions', 'clearing restores idle polling');
}

async function refreshes_keep_the_query_and_reexecution_has_one_listener() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.evaluate();
  await page.respond(page.calls[1], []);
  assert.equal(page.polls.length, 1, 'fragment reexecution retains one polling timer');
  page.input('body');
  await page.advance(250);
  assert.equal(page.calls.length, 3, 'fragment reexecution retains one input listener');
  await page.respond(page.calls[2], []);
  page.focus();
  assert.equal(page.calls[3].url, '/chat/sessions?q=body');
  await page.respond(page.calls[3], []);
  page.document.hidden = true;
  page.emit('visibilitychange');
  assert.equal(page.calls.length, 4);
  page.document.hidden = false;
  page.emit('visibilitychange');
  assert.equal(page.calls[4].url, '/chat/sessions?q=body');
  await page.respond(page.calls[4], []);
  page.action('archive', 'hit');
  assert.equal(page.calls[5].url, '/chat/sessions/hit');
  assert.equal(page.calls[5].options.body, '{"archived":true}');
  await page.respond(page.calls[5], {});
  assert.equal(page.calls[6].url, '/chat/sessions?q=body');
  await page.respond(page.calls[6], []);
  page.newChat();
  assert.equal(page.calls[7].url, '/chat/main/new');
  await page.respond(page.calls[7], { slot: 'new-slot' });
  assert.equal(page.calls[8].url, '/chat/sessions?q=body');
}

async function slow_or_failed_search_does_not_block_chat() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.input('slow');
  await page.advance(250);
  const input = page.elements['chat-input'];
  input.value = 'send while search is pending';
  page.emit('input', input);
  page.emit('submit', { id: 'chat-composer' });
  assert.equal(page.calls[2].url, '/chat/main/send');
  assert.equal(JSON.parse(page.calls[2].options.body).message, 'send while search is pending');
  page.calls[1].reject(new Error('search unavailable'));
  await settle();
  assert.match(page.html(), /Sessions unavailable: search unavailable/);
  await page.respond(page.calls[2], {});
  input.value = 'send after search fails';
  page.emit('input', input);
  page.emit('submit', { id: 'chat-composer' });
  assert.equal(page.calls[3].url, '/chat/main/send');
}

async function row_status_and_time_are_described_not_inline() {
  const page = browser();
  await page.respond(page.calls[0], [
    session('live', 'Live chat', { connected: true }),
    session('old', 'Old chat', { modified_ms: Date.now() - 22 * 3600000 }),
  ]);
  assert.match(page.html(), /aria-describedby="info-live"/);
  assert.match(page.html(), /id="info-live"[^>]*>Connected · now</);
  assert.match(page.html(), /id="info-old"[^>]*>22h</);
  assert.doesNotMatch(page.html(), /<small/);
}

(async () => {
  await row_status_and_time_are_described_not_inline();
  await debounced_server_search();
  await changing_input_invalidates_pending_results();
  await snippets_are_escaped();
  await polling_pauses_for_active_queries();
  await clearing_restores_groups_and_cancels_search();
  await refreshes_keep_the_query_and_reexecution_has_one_listener();
  await slow_or_failed_search_does_not_block_chat();
  console.log('sidebar search: 8 browser scenarios passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
