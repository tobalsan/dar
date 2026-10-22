const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const session = (id, label, modified_ms, extra = {}) => ({ id, label, modified_ms, archived: false, slots: [], ...extra });

function browser(store = {}) {
  const handlers = {}, calls = [], played = [];
  const element = id => {
    let content = '';
    return {
      id, dataset: {}, style: { setProperty() {} }, value: '', hidden: false, disabled: false,
      scrollHeight: 10, scrollTop: 0, clientHeight: 10,
      get innerHTML() { return content; }, set innerHTML(v) { content = v; },
      get textContent() { return content; }, set textContent(v) { content = v; },
      querySelectorAll: () => [], getBoundingClientRect: () => ({ top: 0 }),
    };
  };
  const elements = Object.fromEntries(['chat-root', 'chat-transcript', 'chat-input', 'chat-chips', 'chat-send', 'chat-abort', 'chat-token-meter', 'chat-history-list', 'chat-search'].map(id => [id, element(id)]));
  const document = {
    hidden: false, getElementById: id => elements[id] || null,
    addEventListener: (type, fn) => (handlers[type] ||= []).push(fn),
  };
  const sandbox = {
    document, queueMicrotask, crypto: { randomUUID: () => 'c' },
    window: { innerHeight: 1000, addEventListener() {} },
    localStorage: { getItem: k => (k in store ? store[k] : null), setItem: (k, v) => { store[k] = String(v); } },
    Audio: function (url) { this.url = url; this.play = async () => { played.push({ url, volume: this.volume }); }; },
    EventSource: function () { this.close = () => {}; },
    setTimeout, clearTimeout, setInterval() {},
    fetch: (url, options) => new Promise((resolve, reject) => calls.push({ url, options, resolve, reject })),
  };
  vm.runInContext(source, vm.createContext(sandbox));
  return {
    document, calls, played, store, window: sandbox.window,
    html: () => elements['chat-history-list'].innerHTML,
    async respond(call, entries) { call.resolve({ ok: true, json: async () => entries }); await settle(); },
    async refresh(entries) {
      for (const fn of handlers.visibilitychange || []) fn({});
      await this.respond(this.calls.at(-1), entries);
    },
  };
}

async function new_agent_output_shows_unread_until_opened() {
  const page = browser();
  await page.respond(page.calls[0], [session('a', 'Alpha', 100), session('b', 'Beta', 100)]);
  assert.doesNotMatch(page.html(), /Unread messages/, 'first load marks existing chats as read');
  await page.refresh([session('a', 'Alpha', 200), session('b', 'Beta', 100)]);
  assert.equal((page.html().match(/aria-label="Unread messages"/g) || []).length, 1);
  assert.ok(page.html().indexOf('Unread messages') < page.html().indexOf('<span>Alpha'), 'dot sits before the label');
  await page.refresh([session('a', 'Alpha', 200, { busy: true }), session('b', 'Beta', 100)]);
  assert.doesNotMatch(page.html(), /Unread messages/, 'a running chat is not unread');
  await page.refresh([session('a', 'Alpha', 200, { slots: ['main'] }), session('b', 'Beta', 100)]);
  assert.doesNotMatch(page.html(), /Unread messages/, 'opening the chat clears it');
}

async function unread_survives_reload() {
  const store = {};
  const first = browser(store);
  await first.respond(first.calls[0], [session('a', 'Alpha', 100)]);
  const second = browser(store);
  await second.respond(second.calls[0], [session('a', 'Alpha', 300)]);
  assert.match(second.html(), /Unread messages/);
}

async function unfocused_window_does_not_clear_the_open_chat() {
  const page = browser();
  await page.respond(page.calls[0], [session('a', 'Alpha', 100, { slots: ['main'] })]);
  page.document.hasFocus = () => false;
  await page.refresh([session('a', 'Alpha', 200, { slots: ['main'] })]);
  assert.match(page.html(), /Unread messages/);
}

async function finished_turn_plays_sound_unless_muted_or_aborted() {
  const page = browser();
  page.window.renderChatEvent({ type: 'finished' });
  assert.equal(page.played.length, 1);
  assert.equal(page.played[0].volume, 0.6);
  assert.match(page.played[0].url, /notify\.mp3$/);
  page.window.renderChatEvent({ type: 'aborted' });
  page.window.renderChatEvent({ type: 'finished', historical: true });
  assert.equal(page.played.length, 1, 'aborted and replayed turns stay silent');
  page.store['dar-notify-sound'] = 'off';
  page.window.renderChatEvent({ type: 'finished' });
  assert.equal(page.played.length, 1, 'muted');
}

(async () => {
  await new_agent_output_shows_unread_until_opened();
  await unread_survives_reload();
  await unfocused_window_does_not_clear_the_open_chat();
  await finished_turn_plays_sound_unless_muted_or_aborted();
})().catch(e => { console.error(e); process.exit(1); });
