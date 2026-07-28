(function () {
  const esc = s => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');

  const PREFIX = (typeof window !== 'undefined' && window.__dashPrefix) || '';

  // crypto.randomUUID only exists in secure contexts (https/localhost); dashboards served over plain http (e.g. tailnet hostnames) need the fallback.
  const uuid = () => (crypto.randomUUID ? crypto.randomUUID() : 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => {
    let r = Math.random() * 16 | 0;
    return (c === 'x' ? r : (r & 0x3) | 0x8).toString(16);
  }));

  const markdown = s => {
    let code = [];
    let html = esc(s).replace(/```([^\n]*)\n([\s\S]*?)```/g, (_, lang, text) => `\0${code.push(`<pre><code${lang ? ` data-language="${esc(lang)}"` : ''}>${text}</code></pre>`) - 1}\0`).replace(/`([^`\n]+)`/g, (_, text) => `\0${code.push(`<code>${text}</code>`) - 1}\0`);
    html = html.replace(/^[-*] (.+)$/gm, '<li>$1</li>').replace(/(?:<li>[\s\S]*?<\/li>\n?)+/g, m => `<ul>${m}</ul>`).replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>').replace(/\*(.+?)\*/g, '<em>$1</em>').replace(/\n/g, '<br>').replace(/<\/li><br><\/ul>/g, '</li></ul>');
    return html.replace(/\0(\d+)\0/g, (_, i) => code[i]);
  };

  const reduce = (blocks, event) => {
    // Only the outer list needs a copy. Blocks are append-only except the
    // currently-open tool/question, so cloning every prior block made replay
    // of long archived transcripts quadratic.
    let next = blocks.slice(), text = event.text || '';
    switch (event.type) {
      case 'user': next.push({ kind: 'user', text, attachments: event.attachments || [] }); break;
      case 'reset': return [{ kind: 'notice', text: 'Context cleared, started a new session.' }];
      case 'delta': case 'thinking': {
        let kind = event.type === 'thinking' ? 'thinking' : 'assistant', last = next.at(-1);
        if (last && last.kind === kind) last.text += text; else next.push({ kind, text });
        break;
      }
      case 'tool_call': next.push({ kind: 'tool', id: event.id, name: event.name || event.id, args: event.args || '', text: '', is_error: false, done: false }); break;
      case 'tool_output': {
        let tool = [...next].reverse().find(block => block.kind === 'tool' && block.id === event.id);
        if (!tool) { tool = { kind: 'tool', id: event.id, name: event.id, args: '', text: '', is_error: false, done: false }; next.push(tool); }
        tool.text = text; tool.is_error = !!event.is_error; tool.done = !!event.done; break;
      }
      case 'question': next.push({ kind: 'question', id: event.id, questions: event.questions || [], done: false, rejected: false, answerText: '' }); break;
      case 'question_done': { let q = [...next].reverse().find(b => b.kind === 'question' && b.id === event.id); if (q) { q.done = true; q.rejected = !!event.is_error; q.answerText = event.text || ''; } break; }
      case 'error': next.push({ kind: 'error', text: event.error || 'unknown error' }); break;
      case 'context_usage': break;
      case 'aborted': dismissPendingQuestions(next); next.push({ kind: 'error', text: event.error === 'aborted' ? 'turn aborted' : `turn failed: ${event.error || 'unknown error'}` }); break;
      case 'closed': dismissPendingQuestions(next); next.push({ kind: 'error', text: `chat session closed${event.error ? `: ${event.error}` : ''}` }); break;
      case 'finished': dismissPendingQuestions(next); break;
    }
    return next;
  };

  // Pending questions can't outlive their turn: any terminal turn event marks
  // them dismissed so the UI never sticks on "pending" (e.g. when an abort
  // discards a late opencode rejected event). 'reset' already replaces every
  // block wholesale.
  const dismissPendingQuestions = blocks => {
    for (const block of blocks) if (block.kind === 'question' && !block.done) { block.done = true; block.rejected = true; block.answerText = 'dismissed'; }
  };

  const agentName = () => (typeof document !== 'undefined' && document.getElementById('chat-root') && document.getElementById('chat-root').dataset.agentName) || 'Agent';

  const html = (blocks, qsel = {}) => blocks.map((block, i) => {
    if (block.kind === 'tool') {
      let state = block.is_error ? 'bad' : block.done ? 'done' : 'live';
      let label = block.is_error ? 'error' : block.done ? 'done' : 'running';
      return `<details class="chat-tool" data-tool-id="${esc(block.id)}" data-bi="${i}"><summary><span class="chat-pill chat-pill-${state}">${label}</span><span class="chat-tool-name">${esc(block.name)}</span></summary><pre class="chat-tool-args">${esc(block.args)}</pre><pre class="chat-tool-output${block.is_error ? ' is-error' : ''}${block.done ? ' is-done' : ''}">${esc(block.text)}</pre></details>`;
    }
    if (block.kind === 'question') {
      let state = block.done ? (block.rejected ? 'bad' : 'done') : 'live';
      let label = block.done ? (block.rejected ? 'dismissed' : 'answered') : 'question';
      let sel = qsel[block.id] || [];
      let hasMultiple = block.questions.some(q => q.multiple);
      let body = block.questions.map((q, qi) => {
        let opts = q.options || [];
        let optsHtml = opts.length
          ? `<div class="chat-q-opts">${opts.map(o => {
              let picks = sel[qi], selected = q.multiple ? (Array.isArray(picks) && picks.includes(o.label)) : picks === o.label;
              return `<button type="button" class="chat-q-opt${selected ? ' is-selected' : ''}" data-qbi="${i}" data-qi="${qi}" data-label="${esc(o.label)}" title="${esc(o.description || '')}" aria-pressed="${selected}"${block.done ? ' disabled' : ''}>${esc(o.label)}</button>`;
            }).join('')}</div>`
          : (block.questions.length > 1 ? `<div class="chat-q-note">No options — this question needs a text answer, not supported in a multi-question request.</div>` : '');
        return `<div class="chat-q"><div class="chat-q-header">${esc(q.header || '')}</div><div class="chat-q-text">${esc(q.question || '')}</div>${optsHtml}</div>`;
      }).join('');
      // A question with no options is implicitly free-text even if `custom`
      // is false; the custom row stays limited to single-question blocks.
      let q0 = block.questions[0], effectiveCustom = block.questions.length === 1 && (q0.custom || !(q0.options || []).length);
      let custom = !block.done && effectiveCustom ? `<div class="chat-q-customrow"><input class="chat-q-custom" data-qbi="${i}" placeholder="Custom answer"><button type="button" class="chat-q-send" data-qbi="${i}">Answer</button></div>` : '';
      // Any block containing a `multiple` question (pure or mixed) defers to
      // an explicit Answer button instead of auto-submitting on first click.
      let answerBtn = !block.done && !effectiveCustom && hasMultiple ? `<div class="chat-q-customrow"><button type="button" class="chat-q-answer-btn" data-qbi="${i}">Answer</button></div>` : '';
      let answered = block.done && block.answerText ? `<div class="chat-q-answer">${esc(block.answerText)}</div>` : '';
      return `<div class="chat-question" data-question-id="${esc(block.id)}"><span class="chat-pill chat-pill-${state}">${label}</span>${body}${custom}${answerBtn}${answered}</div>`;
    }
    if (block.kind === 'thinking') {
      return `<details class="chat-think" data-bi="${i}"><summary>Thinking</summary><pre>${esc(block.text)}</pre></details>`;
    }
    if (block.kind === 'error') {
      return `<div class="chat-turn chat-error"><span class="chat-pill chat-pill-bad">error</span><div class="chat-error-body">${markdown(block.text)}</div></div>`;
    }
    if (block.kind === 'notice') {
      return `<div class="chat-notice">${esc(block.text)}</div>`;
    }
    let roleLabel = block.kind === 'user' ? 'You' : block.kind === 'assistant' ? esc(agentName()) : block.kind;
    let attachments = (block.attachments || []).map(a => a.image
      ? `<img class="chat-attachment-image" src="${esc(PREFIX + a.url)}" alt="${esc(a.name)}">`
      : `<a class="chat-attachment" href="${esc(PREFIX + a.url)}" target="_blank" rel="noopener noreferrer">${esc(a.name)}</a>`).join('');
    return `<div class="chat-turn chat-${block.kind}"><div class="chat-role">${roleLabel}</div><div class="chat-body">${markdown(block.text)}${attachments ? `<div class="chat-attach-row">${attachments}</div>` : ''}</div></div>`;
  }).join('');

  const usageText = event => event.context_window ? `${event.tokens_used} / ${event.context_window} tokens` : `${event.tokens_used} tokens`;

  const request = async (url, options) => {
    const response = await fetch(url, options);
    if (response.ok) return response;
    let detail = await response.text().catch(() => '');
    try { detail = JSON.parse(detail).error || detail; } catch (_) { /* plain-text response */ }
    throw new Error(detail || `request failed (${response.status})`);
  };

  if (typeof module !== 'undefined') module.exports = { reduce, html, usageText, request };
  if (typeof document === 'undefined') return;

  const SESSION = 'main', MAX_ATTACHMENTS = 8;
  const $ = id => document.getElementById(id);

  const sendEnabled = app => app.draft.trim() !== '' || app.pending.length > 0;

  const autogrow = el => { if (!el) return; el.style.height = 'auto'; el.style.height = Math.min(el.scrollHeight, Math.round(0.4 * window.innerHeight)) + 'px'; };

  const sizeViewport = () => { let root = $('chat-root'); if (root) root.style.setProperty('--chat-top', Math.round(root.getBoundingClientRect().top) + 'px'); };

  const renderChips = app => {
    let host = $('chat-chips'); if (!host) return;
    host.innerHTML = app.pending.map((file, i) => `<span class="chat-chip"><span class="chat-chip-name">${esc(file.name)}</span><button type="button" class="chat-chip-x" data-chip="${i}" aria-label="Remove attachment">×</button></span>`).join('');
  };

  const refreshBusy = app => {
    let busy = app.turns > 0, abort = $('chat-abort'), send = $('chat-send');
    if (abort) { abort.disabled = !busy; abort.hidden = !busy; }
    if (send) send.disabled = !sendEnabled(app);
  };

  // Whimsy pool for the pending placeholder; a fresh word is drawn per turn.
  const WORKING_WORDS = ['Pondering', 'Conjuring', 'Brewing', 'Scheming', 'Ruminating', 'Percolating', 'Noodling', 'Tinkering', 'Divining', 'Musing', 'Incanting', 'Summoning', 'Marinating', 'Sleuthing', 'Untangling', 'Hatching'];
  // Placeholder for the coming agent response: shown right below the user's
  // message until the first assistant/thinking/tool block replaces it.
  const pendingHtml = word => `<div class="chat-turn chat-assistant chat-pending"><div class="chat-role">${esc(agentName())}</div><div class="chat-body"><span class="chat-loader" role="status" aria-label="Working"><em class="chat-loader-word">${esc(word)}</em><span></span><span></span><span></span></span></div></div>`;

  const paint = app => {
    if (app.viewingHistory) return;
    let transcript = $('chat-transcript'); if (!transcript) return;
    let stick = app.stick;
    let last = app.blocks[app.blocks.length - 1];
    let pending = app.turns > 0 && last && last.kind === 'user';
    let open = new Set(Array.from(transcript.querySelectorAll('details[open]'), d => d.dataset.bi));
    transcript.innerHTML = (app.blocks.length ? html(app.blocks, app.qsel) : '<div class="chat-empty">No messages yet. Ask the agent anything.</div>') + (pending ? pendingHtml(app.workingWord || 'Working') : '');
    for (const d of transcript.querySelectorAll('details')) if (open.has(d.dataset.bi)) d.open = true;
    if (stick) transcript.scrollTop = transcript.scrollHeight;
  };

  // per-event repaint is O(events × transcript) during replay
  const raf = typeof requestAnimationFrame !== 'undefined' ? requestAnimationFrame : queueMicrotask;
  const schedulePaint = app => {
    if (app.paintScheduled) return;
    app.paintScheduled = true;
    raf(() => { app.paintScheduled = false; paint(app); refreshBusy(app); });
  };

  const render = (app, event) => {
    if (event.type === 'context_usage') { let m = $('chat-token-meter'); if (m) m.textContent = usageText(event); return; }
    if (event.type === 'user') { app.turns++; app.workingWord = WORKING_WORDS[Math.floor(Math.random() * WORKING_WORDS.length)]; }
    if (event.type === 'finished' || event.type === 'aborted' || event.type === 'closed') app.turns = Math.max(0, app.turns - 1);
    app.blocks = reduce(app.blocks, event);
    schedulePaint(app);
  };

  const restoreDraft = (app, input, message, files, error) => {
    app.draft = message; input.value = message; app.pending = files;
    renderChips(app); autogrow(input); refreshBusy(app);
    render(app, { type: 'error', error: `Message not sent: ${error.message || 'request failed'}` });
  };

  const sendFlow = async app => {
    let input = $('chat-input');
    if (!input || !sendEnabled(app)) return;
    let message = input.value, files = app.pending.slice(), command_id = uuid();
    let command = message.trim();
    if (!files.length && command === '/new') {
      app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
      try {
        await request(`/chat/${SESSION}/new`, { method: 'POST' });
      } catch (error) {
        restoreDraft(app, input, message, files, error);
      }
      return;
    }
    if (!files.length && command === '/compact') {
      app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
      try {
        await request(`/chat/${SESSION}/compact`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ command_id: uuid() }) });
      } catch (error) {
        restoreDraft(app, input, message, files, error);
      }
      return;
    }
    app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
    try {
      if (files.length) {
        let body = new FormData();
        body.append('command_id', command_id); body.append('message', message);
        for (const file of files) body.append('attachment', file);
        await request(`/chat/${SESSION}/upload`, { method: 'POST', body });
      } else {
        await request(`/chat/${SESSION}/send`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ command_id, message }) });
      }
    } catch (error) {
      restoreDraft(app, input, message, files, error);
    }
  };

  // Deliver an answer for a pending question block. No optimistic done-flip:
  // the block re-renders as done only when the server pushes "question_done".
  const submitAnswer = async (app, block, answers) => {
    if (!block || block.done || app.qsent[block.id]) return;
    app.qsent[block.id] = true;
    try { await request(`/chat/${SESSION}/answer`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ request_id: block.id, answers }) }); }
    catch (error) { delete app.qsent[block.id]; render(app, { type: 'error', error: `Answer not sent: ${error.message || 'request failed'}` }); }
  };
  // Single-select questions submit on first click (a block of several
  // single-select questions auto-submits once every question has a pick).
  // A question marked `multiple` toggles its option in-place instead, and
  // any block containing one waits for the explicit Answer button — never
  // auto-submits, even if the other questions in the block are single-select.
  const answerFlow = (app, bi, qi, label) => {
    let block = app.blocks[bi];
    if (!block || block.kind !== 'question' || block.done) return;
    let sel = app.qsel[block.id] || (app.qsel[block.id] = []);
    let q = block.questions[qi];
    if (q.multiple) {
      let picks = sel[qi] || (sel[qi] = []), idx = picks.indexOf(label);
      if (idx >= 0) picks.splice(idx, 1); else picks.push(label);
    } else sel[qi] = label;
    if (block.questions.some(q => q.multiple)) { schedulePaint(app); return; }
    if (block.questions.every((_, i) => sel[i] != null)) submitAnswer(app, block, sel.map(l => [l]));
    else schedulePaint(app);
  };

  const bindDocument = app => {
    document.addEventListener('submit', e => { if (e.target.id === 'chat-composer') { e.preventDefault(); sendFlow(app); } });
    document.addEventListener('keydown', e => { if (e.target.id === 'chat-input' && e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendFlow(app); } });
    document.addEventListener('input', e => { if (e.target.id === 'chat-input') { app.draft = e.target.value; autogrow(e.target); refreshBusy(app); } });
    document.addEventListener('change', e => {
      if (e.target.id !== 'chat-attachments') return;
      for (const file of e.target.files) if (app.pending.length < MAX_ATTACHMENTS) app.pending.push(file);
      e.target.value = ''; renderChips(app); refreshBusy(app);
    });
    document.addEventListener('click', e => {
      let history = e.target.closest('#chat-history');
      if (history) { showHistoryList(app); return; }
      let live = e.target.closest('#chat-live');
      if (live) { returnToLive(app); return; }
      let session = e.target.closest('[data-history-session]');
      if (session) { openHistorySession(app, session.dataset.historySession); return; }
      // A historical transcript is strictly read-only: question controls are
      // rendered for fidelity, but must never answer against the live session.
      if (app.viewingHistory) return;
      let chip = e.target.closest('.chat-chip-x');
      if (chip) { app.pending.splice(Number(chip.dataset.chip), 1); renderChips(app); refreshBusy(app); return; }
      if (e.target.closest('#chat-attach')) { let f = $('chat-attachments'); if (f) f.click(); return; }
      let abort = e.target.closest('#chat-abort');
      if (abort && !abort.disabled) { fetch(`/chat/${SESSION}/abort`, { method: 'POST' }); return; }
      let opt = e.target.closest('.chat-q-opt');
      if (opt) { answerFlow(app, Number(opt.dataset.qbi), Number(opt.dataset.qi), opt.dataset.label); return; }
      let qsend = e.target.closest('.chat-q-send');
      if (qsend) { let input = document.querySelector(`.chat-q-custom[data-qbi="${qsend.dataset.qbi}"]`); if (input && input.value.trim()) submitAnswer(app, app.blocks[Number(qsend.dataset.qbi)], [[input.value.trim()]]); return; }
      let qanswer = e.target.closest('.chat-q-answer-btn');
      if (qanswer) {
        let block = app.blocks[Number(qanswer.dataset.qbi)];
        if (block) { let sel = app.qsel[block.id] || []; submitAnswer(app, block, block.questions.map((q, qi) => q.multiple ? (sel[qi] || []) : (sel[qi] != null ? [sel[qi]] : []))); }
        return;
      }
    });
    document.addEventListener('scroll', e => { if (e.target.id === 'chat-transcript') { let t = e.target; app.stick = (t.scrollHeight - t.scrollTop - t.clientHeight) < 64; } }, true);
  };

  const mount = app => {
    let transcript = $('chat-transcript'), input = $('chat-input');
    if (!transcript || !input) return;
    paint(app);
    input.value = app.draft; autogrow(input);
    renderChips(app); refreshBusy(app); sizeViewport();
    transcript.scrollTop = transcript.scrollHeight;
    mountHistoryControls();
  };

  // History is deliberately view-only. The EventSource remains attached and
  // continues reducing live events into `liveBlocks` while a transcript is open.
  const mountHistoryControls = () => {
    let root = $('chat-root');
    if (!root || $('chat-history') || !document.createElement) return;
    let bar = document.createElement('div'); bar.className = 'chat-history-head';
    bar.innerHTML = '<button type="button" id="chat-history" class="chat-history-button">History</button><button type="button" id="chat-live" class="chat-history-button" hidden>Back to live</button><div id="chat-history-list" class="chat-history-list" hidden></div>';
    root.insertBefore(bar, root.firstChild);
  };
  const historyUi = () => ({ list: $('chat-history-list'), live: $('chat-live'), composer: $('chat-composer') });
  const showHistoryList = async app => {
    let ui = historyUi(); if (!ui.list) return;
    try {
      let sessions = await request('/chat/sessions').then(r => r.json());
      ui.list.hidden = false;
      ui.list.innerHTML = sessions.length ? sessions.map(s => `<button type="button" class="chat-history-entry" data-history-session="${esc(s.id)}"><span>${esc(s.label)}</span><small>${esc(s.start_time || '')}</small></button>`).join('') : '<div class="chat-empty">No previous sessions.</div>';
    } catch (error) { ui.list.hidden = false; ui.list.textContent = `History unavailable: ${error.message}`; }
  };
  const openHistorySession = async (app, id) => {
    let ui = historyUi();
    try {
      let offset = 0, events = [];
      do {
        let page = await request(`/chat/sessions/${encodeURIComponent(id)}?offset=${offset}&count=20`).then(r => r.json());
        events.push(...page.events); offset = page.next_offset;
      } while (offset != null);
      app.historyBlocks = events.reduce((blocks, event) => reduce(blocks, event), []);
      app.viewingHistory = true; ui.list.hidden = true; if (ui.live) ui.live.hidden = false; if (ui.composer) ui.composer.hidden = true;
      let transcript = $('chat-transcript'); if (transcript) transcript.innerHTML = html(app.historyBlocks) || '<div class="chat-empty">No messages.</div>';
    } catch (error) { if (ui.list) ui.list.textContent = `Transcript unavailable: ${error.message}`; }
  };
  const returnToLive = app => {
    let ui = historyUi(); app.viewingHistory = false; if (ui.live) ui.live.hidden = true; if (ui.composer) ui.composer.hidden = false; paint(app);
  };

  if (!window.__chatWeb) {
    let app = { blocks: [], draft: '', pending: [], turns: 0, stick: true, es: null, paintScheduled: false, qsel: {}, qsent: {}, viewingHistory: false };
    window.__chatWeb = app;
    window.renderChatEvent = event => render(app, event);
    app.es = new EventSource(`/chat/${SESSION}/stream`);
    app.es.onmessage = e => render(app, JSON.parse(e.data));
    bindDocument(app);
    window.addEventListener('resize', sizeViewport);
  }
  mount(window.__chatWeb);
})();
