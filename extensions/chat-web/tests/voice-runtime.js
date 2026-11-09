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

// Browser profile: storage shared by all tabs, fires `storage` in the others.
const profile = () => { const store = {}, tabs = []; return { store, tabs, set(tab, k, v) { store[k] = String(v); for (const t of tabs) if (t !== tab) t.fire('window', 'storage', { key: k }); } }; };

function tab(shared, { fetchFor = () => 'ok', prefix } = {}) {
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
  const document = {
    get hidden() { return t.hidden; }, hasFocus: () => !t.hidden,
    getElementById: id => (id === 'chat-root' ? { hasAttribute: () => true } : id === 'chat-voice' ? button : null),
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
  await wait(50);
  assert.deepEqual(unhandled, [], 'no unhandled rejections');
  console.log('voice runtime ok');
})().catch(e => { console.error(e); process.exit(1); });
