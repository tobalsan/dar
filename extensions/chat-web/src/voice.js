// Voice replies: speaks the last assistant message of a finished turn through
// POST /chat/tts (a proxy to the configured local TTS server).
(function () {
  const MAX_CHUNK = 250;

  // A table is a header row, a `---|---` rule row, then rows containing `|`; outer pipes are optional.
  const RULE_ROW = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
  const dropTables = lines => {
    const out = [];
    for (let i = 0; i < lines.length; i++) {
      if (lines[i].includes('|') && i + 1 < lines.length && lines[i + 1].includes('|') && RULE_ROW.test(lines[i + 1])) {
        i += 1;
        while (i + 1 < lines.length && lines[i + 1].includes('|') && lines[i + 1].trim()) i++;
        continue;
      }
      out.push(lines[i]);
    }
    return out;
  };

  // Markdown -> plain words a voice can read. Code, tables, URLs and paths are dropped.
  const speakable = md => {
    let text = String(md || '')
      .replace(/^ {0,3}(```|~~~)[^\n]*\n[\s\S]*?(\n {0,3}\1[^\n]*|$)/gm, '\n')
      .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
      .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/`([^`\n]+)`/g, (_, code) => (/[\/\\]|\.\w{1,6}$/.test(code.trim()) ? '' : code))
      .replace(/<[^>\n]+>/g, '')
      .replace(/\b(?:https?:\/\/|www\.)\S+/g, '');
    let lines = [];
    for (let line of dropTables(text.split('\n'))) {
      line = line.trim();
      if (!line || line.startsWith('|') || /^[-*_=]{3,}$/.test(line)) continue;
      line = line.replace(/^#{1,6}\s+/, '').replace(/^>\s?/, '').replace(/^(?:[-*+]|\d+[.)])\s+/, '')
        .replace(/(\*\*|__|\*|_|~~)(?=\S)(.+?)(?<=\S)\1/g, '$2').replace(/[*_~`]+/g, '').trim();
      if (!line) continue;
      lines.push(/[.!?:;,]$/.test(line) ? line : line + '.');
    }
    return lines.join(' ').replace(/\s+/g, ' ').trim();
  };

  // Sentences packed into chunks of at most `max` characters, in order.
  const splitChunks = (text, max = MAX_CHUNK) => {
    let pieces = [];
    for (let sentence of String(text || '').split(/(?<=[.!?])\s+/)) {
      while (sentence.length > max) {
        let cut = sentence.lastIndexOf(' ', max);
        if (cut <= 0) cut = max;
        pieces.push(sentence.slice(0, cut).trim());
        sentence = sentence.slice(cut).trim();
      }
      if (sentence) pieces.push(sentence);
    }
    let chunks = [];
    for (let piece of pieces) {
      let last = chunks.length - 1;
      if (last >= 0 && chunks[last].length + 1 + piece.length <= max) chunks[last] += ' ' + piece;
      else chunks.push(piece);
    }
    return chunks;
  };

  // The reply to speak: the last assistant message of the turn that just ended.
  // The reducer counts turn ends on the final block of each finished turn (`turnEnd`);
  // a count above 1 means later turns added nothing, so there is nothing new to say.
  const lastReply = blocks => {
    if (blocks.at(-1)?.turnEnd > 1) return null;
    for (let i = blocks.length - 1; i >= 0; i--) {
      const block = blocks[i];
      if (block.kind === 'user' || (i < blocks.length - 1 && block.turnEnd)) return null;
      if (block.kind === 'assistant') return block;
    }
    return null;
  };

  if (typeof module !== 'undefined') module.exports = { speakable, splitChunks, lastReply };
  if (typeof document === 'undefined') return;

  const PREFIX = window.__dashPrefix || '';
  const MODE_KEY = 'dar-voice-mode', CLAIM_KEY = `dar-voice-claim|${PREFIX}`;
  const tabId = Math.random().toString(36).slice(2);
  const enabled = () => !!document.getElementById('chat-root')?.hasAttribute('data-voice');
  const isOn = () => { try { return localStorage.getItem(MODE_KEY) === 'on'; } catch { return false; } };
  const syncButton = () => {
    const button = document.getElementById('chat-voice');
    if (button) button.setAttribute('aria-pressed', String(isOn()));
  };

  if (window.__chatVoice) { syncButton(); return; }

  let ctx = null, run = 0, source = null, aborts = new Set(), delay = null;

  const stop = () => {
    run++;
    clearTimeout(delay);
    for (const abort of aborts) abort.abort();
    aborts.clear();
    try { source?.stop(); } catch {}
    source = null;
  };

  const fetchAudio = async (text, token) => {
    const abort = new AbortController();
    aborts.add(abort);
    try {
      const response = await fetch(PREFIX + '/chat/tts', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text }), signal: abort.signal });
      if (!response.ok) throw new Error(`tts ${response.status}`);
      const audio = await response.arrayBuffer();
      return token === run ? { buffer: await ctx.decodeAudioData(audio) } : {};
    } catch (error) {
      return { error };
    } finally { aborts.delete(abort); }
  };

  const playBuffer = (buffer, token) => new Promise(resolve => {
    if (token !== run) return resolve();
    const node = ctx.createBufferSource();
    node.buffer = buffer;
    node.connect(ctx.destination);
    node.onended = () => resolve();
    source = node;
    node.start();
  });

  // Plays chunks in order, requesting chunk N+1 while chunk N plays.
  const speak = async text => {
    stop();
    const token = run, chunks = splitChunks(text);
    if (!ctx || !chunks.length) return;
    try {
      const fetches = new Map();
      const request = i => { if (i < chunks.length && !fetches.has(i)) fetches.set(i, fetchAudio(chunks[i], token)); };
      request(0);
      for (let i = 0; i < chunks.length; i++) {
        request(i + 1);
        const { buffer, error } = await fetches.get(i);
        if (token !== run) return;
        if (error) throw error;
        await playBuffer(buffer, token);
        if (token !== run) return;
      }
    } catch (error) {
      if (token === run) console.warn('voice replies:', error);
    }
  };

  const unlock = () => {
    try {
      ctx ||= new (window.AudioContext || window.webkitAudioContext)();
      ctx.resume?.();
      const silent = ctx.createBufferSource();
      silent.buffer = ctx.createBuffer(1, 1, 24000);
      silent.connect(ctx.destination);
      silent.start();
    } catch (error) { console.warn('voice replies:', error); }
  };

  const visible = () => !document.hidden && document.hasFocus?.() !== false;

  // Called by the renderer when a live turn finishes in the viewed chat.
  const turnEnded = (app, seq) => {
    if (!enabled() || !isOn() || !ctx || !visible()) return;
    const reply = lastReply(app.blocks), text = speakable(reply?.text);
    if (!text) return;
    const slot = app.slot, turn = `${slot}:${seq}`, id = `${turn}|${tabId}`, gen = run;
    // Stop, a mode change, a chat switch or a hidden tab during the claim delay cancels this reply.
    const go = () => { if (gen === run && isOn() && app.slot === slot && visible()) speak(text); };
    try {
      if ((localStorage.getItem(CLAIM_KEY) || '').startsWith(turn + '|')) return;
      localStorage.setItem(CLAIM_KEY, id);
      delay = setTimeout(() => { if (localStorage.getItem(CLAIM_KEY) === id) go(); }, 150);
    } catch { go(); }
  };

  // The audio context needs a gesture: after a reload, or when another tab turned the mode on, the next one unlocks it.
  const resume = () => { if (isOn() && !ctx) unlock(); else if (ctx?.state === 'suspended') ctx.resume(); };
  document.addEventListener('keydown', resume);
  document.addEventListener('click', e => {
    if (!e.target.closest?.('#chat-voice')) return resume();
    const on = !isOn();
    try { localStorage.setItem(MODE_KEY, on ? 'on' : 'off'); } catch {}
    if (on) unlock(); else stop();
    syncButton();
  });

  // Only the tab the user is looking at may speak; mode changes in other tabs apply here too.
  document.addEventListener('visibilitychange', () => { if (document.hidden) stop(); });
  window.addEventListener('blur', stop);
  window.addEventListener('storage', e => {
    if (e.key !== MODE_KEY) return;
    if (!isOn()) stop();
    syncButton();
  });

  window.__chatVoice = { stop, turnEnded };
  if (isOn() && enabled()) unlock();
  syncButton();
})();
