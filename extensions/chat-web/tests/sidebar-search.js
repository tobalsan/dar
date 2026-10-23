const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(process.argv[2], 'utf8');
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const session = (id, label, extra = {}) => ({ id, label, modified_ms: Date.now(), archived: false, ...extra });

function browser(saved = {}) {
  const handlers = {}, windowHandlers = {}, timers = new Map(), polls = [], calls = [];
  let clock = 0, timerId = 0;
  const elements = {};
  let editor;
  const editHandlers = {};
  const decode = value => value.replace(/<[^>]*>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');
  const element = id => {
    let content = '', attributes = {};
    return {
      id, dataset: {}, style: { setProperty(k, v) { this[k] = v; } }, value: '', hidden: false,
      disabled: false, isConnected: true, scrollHeight: 10, scrollTop: 0, clientHeight: 10,
      get innerHTML() { return content; }, set innerHTML(value) { content = value; },
      get textContent() { return decode(content); }, set textContent(value) { content = String(value).replace(/</g, '&lt;'); },
      setAttribute(k, v) { attributes[k] = String(v); }, getAttribute(k) { return attributes[k] ?? null; }, removeAttribute(k) { delete attributes[k]; },
      focus() { document.activeElement = this; },
      contains(target) { return target === this || (this.id === 'chat-search-palette' && ['chat-search-input', 'chat-search-results', 'chat-search-status'].includes(target?.id)); },
      closest(selector) { return selector === '#' + id ? this : null; },
      querySelectorAll: selector => selector === 'mark' ? [...content.matchAll(/<mark>([\s\S]*?)<\/mark>/g)].map(match => ({ textContent: decode(match[1]) })) : [], getBoundingClientRect: () => ({ top: 0 }),
      classList: { contains: () => false, toggle() {} },
    };
  };
  for (const id of ['chat-root', 'chat-transcript', 'chat-input', 'chat-chips', 'chat-send', 'chat-abort', 'chat-history-list', 'chat-search-open', 'chat-search-palette', 'chat-search-input', 'chat-search-results', 'chat-search-status', 'chat-session-title', 'chat-context-ring', 'chat-context-tooltip', 'chat-context-warning']) elements[id] = element(id);
  elements['chat-search-palette'].hidden = true;
  elements['chat-context-ring'].hidden = true;
  elements['chat-context-tooltip'].hidden = true;
  const listen = (list, type, fn) => (list[type] ||= []).push(fn);
  const document = {
    hidden: false, activeElement: elements['chat-input'], getElementById: id => elements[id] || null,
    addEventListener: (type, fn) => listen(handlers, type, fn),
    createElement() { editor = element('rename-editor'); editor.select = () => {}; editor.addEventListener = (type, fn) => editHandlers[type] = fn; return editor; },
    querySelector() { return { querySelector: () => ({ textContent: 'Previous title', replaceWith() {} }) }; },
  };
  const window = { innerHeight: 1000, addEventListener: (type, fn) => listen(windowHandlers, type, fn) };
  const storage = new Map(Object.entries(saved));
  const sandbox = {
    document, window, queueMicrotask, CSS: { escape: value => value }, crypto: { randomUUID: () => 'command' },
    sessionStorage: { getItem: k => storage.get(k), setItem: (k, v) => storage.set(k, v) },
    EventSource: function () { this.close = () => {}; },
    setTimeout: (fn, delay) => { timers.set(++timerId, { fn, due: clock + delay }); return timerId; },
    clearTimeout: id => timers.delete(id), setInterval: (fn, delay) => polls.push({ fn, delay }),
    fetch: (url, options) => new Promise((resolve, reject) => calls.push({ url, options, resolve, reject })),
  };
  const context = vm.createContext(sandbox);
  const evaluate = () => vm.runInContext(source, context);
  evaluate();
  const emit = (type, target, extra = {}) => {
    const event = { target, preventDefault() { this.defaultPrevented = true; }, stopPropagation() {}, ...extra };
    for (const fn of handlers[type] || []) fn(event);
    return event;
  };
  return {
    elements, document, calls, polls, evaluate, storage, emit,
    input(value, id = 'chat-search-input') { elements[id].value = value; emit('input', elements[id]); },
    key(key, extra = {}, target = document.activeElement) { return emit('keydown', target, { key, ...extra }); },
    open() { elements['chat-search-open'].focus(); emit('click', elements['chat-search-open']); },
    text: id => elements[id].textContent,
    async advance(ms) { clock += ms; for (const [id, timer] of timers) if (timer.due <= clock) { timers.delete(id); timer.fn(); } await settle(); },
    async respond(call, entries) { call.resolve({ ok: true, json: async () => entries }); await settle(); },
    event: event => window.renderChatEvent(event),
    newChat: () => window.__chatWeb.newChat(),
    rename(id, title) {
      const action = { dataset: { sessionAction: 'rename', sessionId: id }, closest: selector => selector === '[data-session-action]' ? action : null };
      emit('click', action);
      editor.value = title;
      editHandlers.keydown({ key: 'Enter', preventDefault() {} });
    },
  };
}

async function opening_preserves_draft_and_escape_restores_focus() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.input('/compact draft', 'chat-input');
  page.open();
  assert.equal(page.elements['chat-search-palette'].hidden, false);
  assert.equal(page.document.activeElement.id, 'chat-search-input');
  assert.equal(page.text('chat-search-results'), '');
  page.key('Escape');
  assert.equal(page.elements['chat-search-palette'].hidden, true);
  assert.equal(page.document.activeElement.id, 'chat-search-open');
  assert.equal(page.elements['chat-input'].value, '/compact draft');
  assert.equal(page.calls.some(call => call.url.endsWith('/abort')), false);
  for (const modifier of ['metaKey', 'ctrlKey']) {
    page.elements['chat-input'].focus();
    const event = page.key('k', { [modifier]: true });
    assert.equal(event.defaultPrevented, true);
    assert.equal(page.document.activeElement.id, 'chat-search-input');
    assert.equal(page.elements['chat-input'].value, '/compact draft');
    page.key('Escape');
  }
  delete page.elements['chat-root'];
  assert.equal(page.key('k', { metaKey: true }).defaultPrevented, undefined);
  assert.equal(page.elements['chat-search-palette'].hidden, true);
}

async function query_states_and_latest_response() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.open();
  page.input('  tea & 雪 ');
  await page.advance(199);
  assert.equal(page.calls.length, 1);
  await page.advance(1);
  assert.equal(page.calls[1].url, '/chat/sessions?q=tea%20%26%20%E9%9B%AA');
  assert.equal(page.text('chat-search-status'), 'Searching…');
  const stale = page.calls[1];
  page.input('body words');
  await page.respond(stale, [session('stale', 'Stale title')]);
  assert.equal(page.text('chat-search-results'), '');
  await page.advance(200);
  await page.respond(page.calls[2], [session('latest', 'Body title', { snippet: 'words <img src=x> body', archived: true })]);
  assert.equal(page.text('chat-search-status'), '');
  assert.match(page.text('chat-search-results'), /Body title/);
  assert.match(page.text('chat-search-results'), /Archived/);
  assert.match(page.text('chat-search-results'), /words <img src=x> body/);
  assert.deepEqual(page.elements['chat-search-results'].querySelectorAll('mark').map(mark => mark.textContent), ['Body', 'words', 'body'], 'title and snippet visibly highlight each query word');
  page.polls[0].fn();
  assert.equal(page.calls[3].url, '/chat/sessions', 'sidebar refresh continues during search');
  await page.respond(page.calls[3], [session('sidebar', 'Sidebar stays current')]);
  assert.match(page.text('chat-history-list'), /Sidebar stays current/);
  assert.match(page.text('chat-search-results'), /Body title/);
  page.input('none');
  await page.advance(200);
  await page.respond(page.calls[4], []);
  assert.equal(page.text('chat-search-status'), 'No matches');
  page.input('broken');
  await page.advance(200);
  page.calls[5].reject(new Error('search unavailable'));
  await settle();
  assert.equal(page.text('chat-search-status'), 'Search unavailable. Try again.');
  page.input('');
  assert.equal(page.text('chat-search-status'), '');
  assert.equal(page.text('chat-search-results'), '');
}

async function keyboard_and_click_open_results() {
  const page = browser();
  await page.respond(page.calls[0], []);
  page.open(); page.input('match'); await page.advance(200);
  await page.respond(page.calls[1], [session('first', 'First match'), session('second', 'Second match')]);
  assert.equal(page.elements['chat-search-input'].getAttribute('aria-activedescendant'), 'chat-search-option-0');
  page.key('ArrowDown');
  assert.equal(page.elements['chat-search-input'].getAttribute('aria-activedescendant'), 'chat-search-option-1');
  page.key('ArrowUp');
  assert.equal(page.elements['chat-search-input'].getAttribute('aria-activedescendant'), 'chat-search-option-0');
  page.key('ArrowDown'); page.key('Enter');
  assert.equal(page.calls[2].url, '/chat/main/resume');
  assert.equal(JSON.parse(page.calls[2].options.body).id, 'second');
  assert.equal(page.elements['chat-search-palette'].hidden, true);
  await page.respond(page.calls[2], { slot: 'second-slot' });
  await page.respond(page.calls[3], []);
  assert.equal(page.text('chat-session-title'), 'Second match', 'old result label survives the capped plain list');
  page.open(); page.input('match'); await page.advance(200);
  await page.respond(page.calls[4], [session('click', 'Clicked match')]);
  const row = { dataset: { searchIndex: '0' }, closest: selector => selector === '[data-search-index]' ? row : null };
  page.emit('click', row);
  assert.equal(JSON.parse(page.calls[5].options.body).id, 'click');
  assert.equal(page.elements['chat-search-palette'].hidden, true);
}

async function header_tracks_list_fallback_and_new_chat() {
  const page = browser();
  assert.equal(page.text('chat-session-title'), 'New chat');
  await page.respond(page.calls[0], [session('current', 'Archive label', { title: 'Renamed conversation', slots: ['main'] })]);
  assert.equal(page.text('chat-session-title'), 'Renamed conversation');
  page.polls[0].fn();
  await page.respond(page.calls[1], [session('current', 'Label fallback', { slots: ['main'] })]);
  assert.equal(page.text('chat-session-title'), 'Label fallback');
  const restored = browser(Object.fromEntries(page.storage));
  await restored.respond(restored.calls[0], []);
  assert.equal(restored.text('chat-session-title'), 'Label fallback');
  page.event({ type: 'reset' });
  page.polls[0].fn();
  await page.respond(page.calls[2], [session('current', 'Label fallback', { title: 'Automatic title arrived', slots: ['main'] })]);
  assert.equal(page.text('chat-session-title'), 'Automatic title arrived');
}

async function context_ring_reports_usage_and_clears() {
  const page = browser(), ring = page.elements['chat-context-ring'];
  assert.equal(ring.hidden, true);
  page.event({ type: 'context_usage', tokens_used: 83632, context_window: 200000 });
  assert.equal(ring.hidden, false);
  assert.equal(ring.getAttribute('aria-label'), '83,632 / 200,000 tokens · 42%');
  assert.equal(page.text('chat-context-tooltip'), '83,632 / 200,000 tokens · 42%');
  assert.ok(Math.abs(ring.style['--usage-fill'] - 41.816) < 0.001);
  for (const [tokens, color, warning] of [[69, 'var(--muted)', false], [70, 'var(--warn)', true], [89, 'var(--warn)', true], [90, 'var(--bad)', true]]) {
    page.event({ type: 'context_usage', tokens_used: tokens, context_window: 100 });
    assert.equal(ring.style.color, color);
    assert.equal(page.elements['chat-context-warning'].hidden, !warning);
  }
  page.event({ type: 'context_usage', tokens_used: 83632 });
  assert.equal(ring.getAttribute('aria-label'), '83,632 tokens');
  assert.equal(ring.style['--usage-fill'], 0);
  page.emit('mouseover', ring);
  assert.equal(page.elements['chat-context-tooltip'].hidden, false);
  page.emit('mouseout', ring);
  assert.equal(page.elements['chat-context-tooltip'].hidden, true);
  page.emit('focusin', ring);
  assert.equal(page.elements['chat-context-tooltip'].hidden, false);
  page.emit('focusout', ring);
  assert.equal(page.elements['chat-context-tooltip'].hidden, true);
  page.emit('click', ring);
  assert.equal(page.elements['chat-context-tooltip'].hidden, false);
  page.event({ type: 'reset' });
  assert.equal(ring.hidden, true);
  assert.equal(page.elements['chat-context-tooltip'].hidden, true);
}

async function dismissal_focus_and_reexecution_stay_local() {
  const page = browser(); await page.respond(page.calls[0], []);
  page.open();
  page.key('Tab'); assert.equal(page.document.activeElement.id, 'chat-search-input');
  page.key('Tab', { shiftKey: true }); assert.equal(page.document.activeElement.id, 'chat-search-input');
  page.elements['chat-input'].focus(); page.emit('focusin', page.elements['chat-input']);
  assert.equal(page.document.activeElement.id, 'chat-search-input');
  page.emit('click', { closest: () => null });
  assert.equal(page.elements['chat-search-palette'].hidden, true, 'dashboard chrome outside palette dismisses it');
  page.open(); page.input('slow'); await page.advance(200);
  const stale = page.calls[1]; page.input('latest'); await page.advance(200);
  await page.respond(page.calls[2], [session('latest', 'Latest result')]);
  stale.reject(new Error('late failure')); await settle();
  assert.equal(page.text('chat-search-status'), '');
  assert.match(page.text('chat-search-results'), /Latest result/);
  page.elements['chat-search-palette'].hidden = true; // dashboard replaced the fragment
  page.evaluate(); await page.respond(page.calls[3], []);
  page.key('k', { metaKey: true });
  assert.equal(page.elements['chat-search-palette'].hidden, false);
  assert.equal(page.polls.length, 1);
  page.input('one request'); await page.advance(200);
  assert.equal(page.calls.length, 5, 'fragment reexecution binds one input handler');
}

async function local_rename_and_new_chat_update_the_header() {
  const page = browser();
  await page.respond(page.calls[0], [session('current', 'Original title', { slots: ['main'] })]);
  page.rename('current', 'My renamed chat');
  assert.equal(page.calls[1].url, '/chat/sessions/current');
  assert.deepEqual(JSON.parse(page.calls[1].options.body), { title: 'My renamed chat' });
  await page.respond(page.calls[1], {});
  assert.equal(page.text('chat-session-title'), 'My renamed chat', 'rename updates immediately before list refresh');
  await page.respond(page.calls[2], [session('current', 'My renamed chat', { slots: ['main'] })]);
  page.newChat();
  await page.respond(page.calls[3], { slot: 'new-slot' });
  assert.equal(page.text('chat-session-title'), 'New chat');
  await page.respond(page.calls[4], []);
}

(async () => {
  await opening_preserves_draft_and_escape_restores_focus();
  await query_states_and_latest_response();
  await keyboard_and_click_open_results();
  await header_tracks_list_fallback_and_new_chat();
  await context_ring_reports_usage_and_clears();
  await dismissal_focus_and_reexecution_stay_local();
  await local_rename_and_new_chat_update_the_header();
  console.log('search palette: 7 browser scenarios passed');
})().catch(error => { console.error(error); process.exit(1); });
