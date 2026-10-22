const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');
const notifySource = fs.readFileSync(process.argv[2].replace('renderer.js', 'notify.js'), 'utf8');
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const wait = ms => new Promise(r => setTimeout(r, ms));
const session = (id, label, extra = {}) => ({ id, label, modified_ms: 100, archived: false, slots: [], ...extra });

// One browser tab. Tabs that share `store` behave like tabs of one browser profile.
function browser(store = {}, { withChat = true } = {}) {
  const calls = [], played = [], sources = [];
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
    hidden: false, getElementById: id => (withChat || id !== 'chat-root' ? elements[id] || null : null),
    addEventListener() {},
  };
  const sandbox = {
    document, queueMicrotask, crypto: { randomUUID: () => 'c' },
    window: { innerHeight: 1000, addEventListener() {} },
    localStorage: { getItem: k => (k in store ? store[k] : null), setItem: (k, v) => { store[k] = String(v); } },
    Audio: function (url) { this.url = url; this.play = async () => { played.push({ url, volume: this.volume }); }; },
    EventSource: function (url) { this.url = url; sources.push(this); this.close = () => {}; },
    setTimeout, clearTimeout, setInterval() {},
    fetch: (url, options) => new Promise((resolve, reject) => calls.push({ url, options, resolve, reject })),
  };
  const context = vm.createContext(sandbox);
  if (withChat) vm.runInContext(source, context);
  vm.runInContext(notifySource, context);
  return {
    document, calls, played, store, window: sandbox.window, sources,
    html: () => elements['chat-history-list'].innerHTML,
    // Server pushes a live turn end on the agent-wide stream.
    push: ended => sources.find(s => s.url === '/chat/events').onmessage({ data: JSON.stringify(ended) }),
    async sidebar(entries) {
      this.window.__chatWeb.refreshSidebar();
      await settle();
      this.calls.at(-1).resolve({ ok: true, json: async () => entries });
      await settle();
    },
  };
}
const ended = (turn, slot, extra = {}) => ({ turn, slot, session: null, kind: 'finished', ...extra });
const dots = html => (html.match(/aria-label="Unread messages"/g) || []).length;

async function finished_turn_in_another_chat_plays_sound_and_marks_it_unread() {
  const page = browser();
  page.push(ended('t1', 'other'));
  await wait(250);
  assert.equal(page.played.length, 1);
  assert.equal(page.played[0].volume, 0.6);
  assert.match(page.played[0].url, /notify\.mp3$/);
  await page.sidebar([session('a', 'Alpha', { slots: ['main'] }), session('b', 'Beta', { slots: ['other'] })]);
  assert.equal(dots(page.html()), 1);
  assert.ok(page.html().indexOf('Unread messages') < page.html().indexOf('<span>Beta'), 'dot sits on Beta');
}

async function new_chat_answer_after_leaving_shows_unread() {
  const page = browser();
  page.push(ended('t1', 'fresh', { session: 'archive-1' }));
  await page.sidebar([session('archive-1', 'New chat', { slots: ['fresh'] }), session('a', 'Alpha', { slots: ['main'] })]);
  assert.equal(dots(page.html()), 1);
}

async function finished_turn_in_the_viewed_chat_stays_read() {
  const page = browser();
  page.push(ended('t1', 'main'));
  await wait(250);
  await page.sidebar([session('a', 'Alpha', { slots: ['main'] })]);
  assert.equal(dots(page.html()), 0);
  assert.equal(page.played.length, 1, 'sound still plays');
}

async function hidden_page_marks_the_viewed_chat_unread_until_it_is_shown_again() {
  const page = browser();
  page.document.hidden = true;
  page.push(ended('t1', 'main'));
  page.document.hidden = false;
  await page.sidebar([session('a', 'Alpha', { slots: ['main'] })]);
  assert.equal(dots(page.html()), 0, 'visible page clears the dot on open');
  page.document.hidden = true;
  page.push(ended('t2', 'main'));
  await page.sidebar([session('a', 'Alpha', { slots: ['main'] })]);
  assert.equal(dots(page.html()), 1, 'hidden page keeps the dot');
}

async function unread_survives_reload_and_opening_clears_it() {
  const store = {};
  browser(store).push(ended('t1', 'other'));
  const reloaded = browser(store);
  await reloaded.sidebar([session('b', 'Beta', { slots: ['other'] })]);
  assert.equal(dots(reloaded.html()), 1);
  await reloaded.sidebar([session('b', 'Beta', { slots: ['main'] })]);
  assert.equal(dots(reloaded.html()), 0);
}

async function aborted_and_replayed_turns_stay_silent() {
  const page = browser();
  page.push(ended('t1', 'other', { kind: 'aborted' }));
  page.window.renderChatEvent({ type: 'finished', historical: true });
  page.window.renderChatEvent({ type: 'finished' });
  await wait(250);
  assert.equal(page.played.length, 0);
  await page.sidebar([session('b', 'Beta', { slots: ['other'] })]);
  assert.equal(dots(page.html()), 0);
}

async function muted_bell_keeps_sound_off() {
  const page = browser({ 'dar-notify-sound': 'off' });
  page.push(ended('t1', 'other'));
  await wait(250);
  assert.equal(page.played.length, 0);
}

async function two_tabs_play_one_sound_per_turn() {
  const store = {};
  const first = browser(store), second = browser(store, { withChat: false });
  first.push(ended('t1', 'other'));
  second.push(ended('t1', 'other'));
  await wait(300);
  assert.equal(first.played.length + second.played.length, 1);
  first.push(ended('t2', 'other'));
  second.push(ended('t2', 'other'));
  await wait(300);
  assert.equal(first.played.length + second.played.length, 2, 'next turn plays again');
}

// Page load fetches the session list for catch-up; answer that request.
async function catchUp(page, entries) {
  for (let call of page.calls.filter(c => c.url.startsWith('/chat/sessions'))) call.resolve({ ok: true, json: async () => entries });
  page.calls.length = 0;
  await settle();
}
const NOW = Date.now();

async function turn_missed_while_disconnected_shows_dot_without_sound() {
  const store = { 'dar-chat-seen': JSON.stringify({ created: NOW - 100000, seen: { a: NOW - 50000, b: NOW - 50000 } }) };
  const page = browser(store);
  await catchUp(page, [session('a', 'Alpha', { slots: ['main'], modified_ms: NOW - 60000 }), session('b', 'Beta', { modified_ms: NOW - 10000 })]);
  await wait(250);
  await page.sidebar([session('a', 'Alpha', { slots: ['main'], modified_ms: NOW - 60000 }), session('b', 'Beta', { modified_ms: NOW - 10000 })]);
  assert.equal(dots(page.html()), 1);
  assert.ok(page.html().indexOf('Unread messages') < page.html().indexOf('<span>Beta'), 'dot sits on Beta');
  assert.equal(page.played.length, 0, 'no sound for caught-up turns');
}

async function reconnect_runs_catch_up_again() {
  const store = { 'dar-chat-seen': JSON.stringify({ created: NOW - 100000, seen: {} }) };
  const page = browser(store);
  await catchUp(page, []);
  page.sources.find(s => s.url === '/chat/events').onopen();
  await settle();
  await catchUp(page, [session('b', 'Beta', { modified_ms: NOW - 10000 })]);
  await page.sidebar([session('b', 'Beta', { modified_ms: NOW - 10000 })]);
  assert.equal(dots(page.html()), 1);
}

async function own_viewed_chat_and_running_chat_get_no_dot() {
  const store = { 'dar-chat-seen': JSON.stringify({ created: NOW - 100000, seen: {} }) };
  const page = browser(store);
  const entries = [session('a', 'Alpha', { slots: ['main'], modified_ms: NOW - 1000 }), session('c', 'Gamma', { busy: true, modified_ms: NOW - 1000 })];
  await catchUp(page, entries);
  await page.sidebar(entries);
  assert.equal(dots(page.html()), 0);
  page.push(ended('t9', 'main', { session: 'a' }));
  await page.sidebar([session('a', 'Alpha', { slots: ['main'], modified_ms: NOW + 5 })]);
  assert.equal(dots(page.html()), 0, 'turn finished while viewing leaves no dot');
}

async function first_run_does_not_flood_old_history() {
  const page = browser();
  const entries = [session('a', 'Alpha', { modified_ms: NOW - 86400000 }), session('b', 'Beta', { modified_ms: NOW - 3600000 })];
  await catchUp(page, entries);
  await page.sidebar(entries);
  assert.equal(dots(page.html()), 0);
}

async function sending_marks_a_new_chat_as_seen() {
  const store = { 'dar-chat-seen': JSON.stringify({ created: NOW - 100000, seen: {} }) };
  const page = browser(store);
  page.window.__chatSeen.mark(['other'], NOW);
  const entries = [session('a', 'Alpha', { slots: ['other'], modified_ms: NOW - 1000 })];
  await catchUp(page, entries);
  await page.sidebar(entries);
  assert.equal(dots(page.html()), 0);
}

(async () => {
  await turn_missed_while_disconnected_shows_dot_without_sound();
  await reconnect_runs_catch_up_again();
  await own_viewed_chat_and_running_chat_get_no_dot();
  await first_run_does_not_flood_old_history();
  await sending_marks_a_new_chat_as_seen();
  await finished_turn_in_another_chat_plays_sound_and_marks_it_unread();
  await new_chat_answer_after_leaving_shows_unread();
  await finished_turn_in_the_viewed_chat_stays_read();
  await hidden_page_marks_the_viewed_chat_unread_until_it_is_shown_again();
  await unread_survives_reload_and_opening_clears_it();
  await aborted_and_replayed_turns_stay_silent();
  await muted_bell_keeps_sound_off();
  await two_tabs_play_one_sound_per_turn();
})().catch(e => { console.error(e); process.exit(1); });
