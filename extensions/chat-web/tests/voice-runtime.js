// Voice replies in the browser: cancellation, one speaking tab, and failing prefetches.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(process.argv[2], 'utf8');
const wait = ms => new Promise(r => setTimeout(r, ms));
const unhandled = [];
process.on('unhandledRejection', e => unhandled.push(e));

const sentence = 'This is a fairly long sentence that fills most of one chunk so the reply needs several chunks to be read out loud now.';
const REPLY = [sentence, sentence, sentence].join(' ');
const splitFirst = text => text.split(/(?<=[.!?])\s+/)[0];

// Browser profile: storage shared by all tabs, fires `storage` in the others.
const profile = () => { const store = {}, tabs = []; return { store, tabs, set(tab, k, v) { store[k] = String(v); for (const t of tabs) if (t !== tab) t.fire('window', 'storage', { key: k }); } }; };

function tab(shared, { fetchFor = () => 'ok', prefix, away = 'pause' } = {}) {
  const handlers = { window: {}, document: {} };
  const t = { playing: new Set(), fetched: [], warnings: [], hidden: false };
  t.fire = (where, type, event = {}) => (handlers[where][type] || []).forEach(h => h(event));
  const listen = where => (type, h) => (handlers[where][type] ||= []).push(h);
  const button = { setAttribute(k, v) { this[k] = v; } };
  t.pressed = () => button['aria-pressed'];
  class Source {
    connect() {}
    start() { if (!this.buffer?.len) return; t.playing.add(this); this.timer = setTimeout(() => this.end(), 80); }
    stop() { this.end(); }
    end() { clearTimeout(this.timer); if (t.playing.delete(this)) this.onended?.(); }
  }
  class AudioContext {
    state = 'running'; destination = {};
    resume() { this.state = 'running'; }
    createBufferSource() { return new Source(); }
    createBuffer() { return { len: 0 }; }
    async decodeAudioData() { return { len: 1 }; }
  }
  // Play buttons as the page shows them: key -> accessible name, kept in step by the page script.
  t.buttons = new Map();
  // Play button elements live for the whole test: the page script must update them in place, so focus stays.
  const buttonList = () => [...t.buttons.values()];
  const makeButton = key => ({
    attrs: { 'data-voice-play': key, 'data-state': 'idle', 'aria-label': 'Play message' },
    getAttribute(name) { return this.attrs[name]; },
    setAttribute(name, value) { this.attrs[name] = value; },
    removeAttribute(name) { delete this.attrs[name]; },
  });
  t.addButton = key => t.buttons.set(String(key), makeButton(String(key)));
  t.label = key => t.buttons.get(String(key)).attrs['aria-label'];
  const document = {
    querySelectorAll: () => buttonList(),
    get hidden() { return t.hidden; }, hasFocus: () => !t.hidden,
    getElementById: id => (id === 'chat-root' ? { hasAttribute: () => true, getAttribute: n => (n === 'data-voice-away' ? away : null) } : id === 'chat-voice' ? button : null),
    addEventListener: listen('document'),
  };
  const sandbox = {
    document, AbortController, ArrayBuffer, JSON, console: { warn: (...a) => t.warnings.push(a.join(' ')) }, setTimeout, clearTimeout,
    window: { addEventListener: listen('window'), AudioContext, __dashPrefix: prefix },
    localStorage: { getItem: k => (k in shared.store ? shared.store[k] : null), setItem: (k, v) => shared.set(t, k, v) },
    fetch: (url, { body, signal }) => new Promise((resolve, reject) => {
      const text = JSON.parse(body).text;
      t.fetched.push(text);
      signal.addEventListener('abort', () => reject(new Error('aborted')));
      setTimeout(() => (fetchFor(t.fetched.length) === 'fail' ? reject(new Error('network down')) : resolve({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) })), 10);
    }),
  };
  sandbox.window.addEventListener = (type, h) => listen('window')(type, h);
  const context = vm.createContext(sandbox);
  vm.runInContext(source, context);
  t.voice = sandbox.window.__chatVoice;
  t.click = target => t.fire('document', 'click', { target: { closest: sel => (target === 'voice' && sel === '#chat-voice' ? button : null) } });
  t.finish = (app, seq = 1) => t.voice.turnEnded(app, seq);
  shared.tabs.push(t);
  return t;
}
const chat = (slot = "main") => ({ slot, blocks: [{ kind: 'user', text: 'q' }, { kind: 'assistant', text: REPLY, turnEnd: true }] });

(async () => {
  // 1. Stop cancels speech that is still in the claim delay.
  for (const cancel of ['toggle off', 'new message', 'switch chat', 'tab hidden']) {
    const p = profile(), a = tab(p), app = chat();
    a.click('voice'); a.finish(app);
    if (cancel === 'toggle off') a.click('voice');
    if (cancel === 'new message') a.voice.stop();
    if (cancel === 'switch chat') app.slot = 'other';
    if (cancel === 'tab hidden') a.hidden = true;
    await wait(300);
    assert.equal(a.fetched.length, 0, `no speech after ${cancel}`);
    assert.equal(a.playing.size, 0);
  }

  // Baseline: a finished turn is spoken in several chunks, in order.
  {
    const p = profile(), a = tab(p);
    a.click('voice'); a.finish(chat());
    await wait(500);
    assert.ok(a.fetched.length >= 2, 'reply is read in chunks');
    assert.equal(a.fetched.join(' '), REPLY);
    assert.equal(a.playing.size, 0, 'playback ends');
  }

  // 2. Two tabs: moving to the other tab stops the first; mode changes sync.
  {
    const p = profile(), a = tab(p), b = tab(p);
    a.click('voice');
    assert.equal(b.pressed(), 'true', 'tab B shows the mode turned on in tab A');
    b.fire('document', 'keydown');
    a.finish(chat(), 1);
    await wait(250);
    assert.equal(a.playing.size, 1, 'tab A speaks');
    a.hidden = true; a.fire('document', 'visibilitychange');
    b.finish(chat(), 2);
    await wait(250);
    assert.equal(a.playing.size, 0, 'tab A stopped when it lost the user');
    assert.equal(b.playing.size, 1, 'tab B speaks');
    a.hidden = false; a.fire('window', 'blur');
    a.click('voice'); // off in A
    assert.equal(b.pressed(), 'false');
    assert.equal(b.playing.size, 0, 'turning voice off in A silences B');
  }

  // A reset restarts the event count of the viewed chat; the next finished turn still speaks.
  {
    const p = profile(), a = tab(p), app = chat();
    a.click('voice'); a.finish(app, 5);
    await wait(250);
    assert.equal(a.playing.size, 1, 'first turn speaks');
    a.voice.stop();
    a.finish(app, 6);
    await wait(250);
    assert.equal(a.playing.size, 1, 'a turn after a reset speaks');
  }

  // Fleet pages share storage: two agents with the same slot and count both speak.
  {
    const p = profile(), a = tab(p, { prefix: '/agents/one' }), b = tab(p, { prefix: '/agents/two' });
    a.click('voice'); b.fire('document', 'keydown');
    a.finish(chat(), 1); b.finish(chat(), 1);
    await wait(250);
    assert.equal(a.playing.size, 1, 'agent one speaks');
    assert.equal(b.playing.size, 1, 'agent two speaks its own reply');
  }

  // 5. A failing prefetch is handled while the first chunk plays; so is a stop mid-playback.
  {
    const p = profile(), a = tab(p, { fetchFor: n => (n === 2 ? 'fail' : 'ok') });
    a.click('voice'); a.finish(chat());
    await wait(500);
    assert.ok(a.warnings.some(w => w.includes('network down')), 'failure is logged, not thrown');
    assert.equal(a.playing.size, 0);
    const q = profile(), b = tab(q);
    b.click('voice'); b.finish(chat());
    await wait(220);
    b.voice.stop();
    await wait(300);
    assert.equal(b.playing.size, 0);
    assert.deepEqual(b.warnings, [], 'a stop is not an error');
  }
  // 6. Play button of a message: works with the voice toggle off, one message at a time, Stop drops the queue.
  {
    const p = profile(), a = tab(p);
    a.addButton(1); a.addButton(2);
    const focused = a.buttons.get('1'); // the same element must stay in the page, so keyboard focus is kept
    assert.equal(a.label(1), 'Play message');
    a.voice.playMessage(1, REPLY); // voice toggle is off
    assert.equal(a.label(1), 'Stop message', 'loading counts as playing: the button offers Stop');
    await wait(60);
    assert.equal(a.playing.size, 1, 'a click plays the message');
    assert.equal(a.buttons.get('1').attrs['data-state'], 'playing');
    assert.equal(a.buttons.get('1'), focused, 'a state change updates the button in place');
    a.voice.playMessage(2, 'Second message.');
    assert.equal(a.label(1), 'Play message', 'the first message went back to Play');
    assert.equal(a.label(2), 'Stop message');
    await wait(40);
    assert.equal(a.fetched.at(-1), 'Second message.');
    assert.equal(a.playing.size, 1, 'only one message plays');
    a.voice.playMessage(2, 'Second message.'); // Stop
    assert.equal(a.playing.size, 0, 'Stop silences the audio');
    assert.equal(a.label(2), 'Play message');
    const spoken = a.fetched.length;
    await wait(300);
    assert.equal(a.fetched.length, spoken, 'Stop drops the queue');
    a.voice.playMessage(1, REPLY);
    await wait(700);
    assert.equal(a.playing.size, 0);
    assert.equal(a.label(1), 'Play message', 'the button returns to Play after the end');
    a.voice.playMessage(1, '```\ncode only\n```');
    assert.equal(a.label(1), 'Play message', 'a message with nothing to read does nothing');
    const f = tab(profile(), { fetchFor: () => 'fail' });
    f.addButton(3);
    f.voice.playMessage(3, 'Will fail.');
    await wait(100);
    assert.equal(f.label(3), 'Play message', 'a failure returns the button to Play');
    assert.ok(f.warnings.some(w => w.includes('network down')), 'the error goes to the console');
    const g = tab(profile());
    g.addButton(4); g.voice.playMessage(4, 'Stopped by a new message.'); g.voice.stop();
    assert.equal(g.label(4), 'Play message', 'a new message or chat switch resets the button');
  }
  // 7. Away: a reply that ends while hidden is held and plays on return; pause resumes at the same chunk.
  const away = t => { t.hidden = true; t.fire('document', 'visibilitychange'); };
  const back = t => { t.hidden = false; t.fire('window', 'focus'); };
  {
    const p = profile(), a = tab(p);
    a.click('voice'); away(a); a.finish(chat());
    await wait(300);
    assert.equal(a.fetched.length, 0, 'nothing is spoken while away');
    back(a);
    await wait(200);
    assert.equal(a.fetched.length, 0, 'the return delay holds the speech');
    await wait(400);
    assert.equal(a.fetched[0], splitFirst(REPLY), 'the held reply plays on return');
    await wait(500);
    assert.equal(a.fetched.join(' '), REPLY);
  }
  {
    const p = profile(), a = tab(p);
    a.click('voice'); a.finish(chat());
    await wait(300); // chunk 0 done, chunk 1 plays
    away(a);
    assert.equal(a.playing.size, 0, 'leaving stops the audio');
    const before = a.fetched.length;
    await wait(200);
    assert.equal(a.fetched.length, before, 'no request while away');
    a.fetched.length = 0;
    back(a);
    await wait(700);
    assert.ok(a.fetched.length >= 1 && a.fetched.length < 3, 'resumes at the chunk that played, not chunk 0');
    assert.equal(a.playing.size, 0);
  }
  {
    const p = profile(), a = tab(p);
    a.addButton(1);
    a.voice.playMessage(1, REPLY); // toggle off
    await wait(100);
    away(a);
    assert.equal(a.label(1), 'Play message', 'paused button is idle');
    a.fetched.length = 0;
    back(a);
    await wait(500);
    assert.ok(a.fetched.length >= 1, 'play-button speech resumes on return');
  }
  {
    const p = profile(), a = tab(p);
    a.click('voice'); away(a);
    a.finish(chat(), 1);
    const app2 = chat(); app2.blocks[1].text = 'Newer reply.';
    a.finish(app2, 2);
    back(a);
    await wait(500);
    assert.deepEqual(a.fetched, ['Newer reply.'], 'only the latest pending reply plays');
  }
  for (const clear of ['stop', 'toggle off', 'switch chat']) {
    const p = profile(), a = tab(p), app = chat();
    a.click('voice'); away(a); a.finish(app);
    if (clear === 'stop') a.voice.stop();
    if (clear === 'toggle off') a.click('voice');
    if (clear === 'switch chat') { app.slot = 'other'; a.voice.stop(); }
    back(a);
    await wait(500);
    assert.equal(a.fetched.length, 0, `${clear} clears the pending reply`);
  }
  {
    const p = profile(), a = tab(p);
    a.click('voice'); away(a); a.finish(chat());
    back(a); await wait(100);
    a.fire('window', 'blur'); a.hidden = true; await wait(100);
    back(a); await wait(100); a.hidden = true; back(a);
    await wait(500);
    assert.equal(a.fetched.join(' '), REPLY, 'a quick blur and focus plays once');
  }
  {
    const p = profile(), a = tab(p), b = tab(p);
    a.click('voice'); b.fire('document', 'keydown');
    away(a); a.finish(chat(), 1);
    b.finish(chat(), 1); // B speaks this turn
    await wait(250);
    assert.equal(b.playing.size, 1);
    back(a);
    await wait(600);
    assert.equal(a.fetched.length, 0, 'a turn another tab claimed is not spoken');
  }
  // 8. when_away modes.
  {
    const p = profile(), a = tab(p, { away: 'hold' });
    a.click('voice'); a.finish(chat());
    await wait(250);
    assert.equal(a.playing.size, 1, 'hold: speech runs');
    away(a);
    assert.equal(a.playing.size, 1, 'hold: speech continues when the user leaves');
    a.fire('window', 'blur');
    await wait(600);
    assert.equal(a.fetched.join(' '), REPLY, 'hold: the whole reply is spoken');
    const q = profile(), b = tab(q, { away: 'hold' });
    b.click('voice'); away(b); b.finish(chat());
    await wait(400);
    assert.equal(b.fetched.length, 0, 'hold: nothing starts while away');
  }
  {
    const p = profile(), a = tab(p, { away: 'play' });
    a.click('voice'); away(a); a.finish(chat());
    await wait(250);
    assert.equal(a.playing.size, 1, 'play: speaks while away');
    a.fire('window', 'blur');
    assert.equal(a.playing.size, 1, 'play: keeps playing');
    await wait(500);
    assert.equal(a.fetched.join(' '), REPLY);
    a.finish(chat(), 9);
    await wait(250);
    a.voice.stop();
    assert.equal(a.playing.size, 0, 'play: stop still stops');
  }
  {
    const p = profile(), a = tab(p, { away: null });
    a.click('voice'); away(a); a.finish(chat());
    await wait(400);
    assert.equal(a.fetched.length, 0, 'no attribute means hold');
  }
  await wait(50);
  assert.deepEqual(unhandled, [], 'no unhandled rejections');
  console.log('voice runtime ok');
})().catch(e => { console.error(e); process.exit(1); });
