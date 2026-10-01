(function () {
  const esc = s => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');

  const PREFIX = (typeof window !== 'undefined' && window.__dashPrefix) || '';

  // crypto.randomUUID only exists in secure contexts (https/localhost); dashboards served over plain http (e.g. tailnet hostnames) need the fallback.
  const uuid = () => (crypto.randomUUID ? crypto.randomUUID() : 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => {
    let r = Math.random() * 16 | 0;
    return (c === 'x' ? r : (r & 0x3) | 0x8).toString(16);
  }));

  const fallbackMarkdown = s => {
    let code = [];
    let out = esc(s).replace(/```([^\n]*)\n([\s\S]*?)```/g, (_, lang, text) => `\0${code.push(`<pre><code${lang ? ` data-language="${esc(lang)}"` : ''}>${text}</code></pre>`) - 1}\0`).replace(/`([^`\n]+)`/g, (_, text) => `\0${code.push(`<code>${text}</code>`) - 1}\0`);
    out = out.replace(/^[-*] (.+)$/gm, '<li>$1</li>').replace(/(?:<li>[\s\S]*?<\/li>\n?)+/g, m => `<ul>${m}</ul>`).replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>').replace(/\*(.+?)\*/g, '<em>$1</em>').replace(/\n/g, '<br>').replace(/<\/li><br><\/ul>/g, '</li></ul>');
    return out.replace(/\0(\d+)\0/g, (_, i) => code[i]);
  };
  const markdown = s => {
    if (typeof marked === 'undefined' || typeof DOMPurify === 'undefined') return fallbackMarkdown(s);
    const renderer = new marked.Renderer();
    renderer.html = token => esc(typeof token === 'string' ? token : token.text || token.raw || '');
    renderer.link = function (token) { return `<a href="${esc(token.href || '')}" target="_blank" rel="noopener noreferrer"${token.title ? ` title="${esc(token.title)}"` : ''}>${this.parser.parseInline(token.tokens || [])}</a>`; };
    return DOMPurify.sanitize(marked.parse(s, { gfm: true, breaks: true, renderer }), { ADD_ATTR: ['target'] });
  };

  // Mutate a private transcript buffer and report the changed block. Live
  // rendering keeps the copy-on-write wrapper below, while history can append
  // pages without repeatedly copying its complete archive.
  const reduceInto = (blocks, event) => {
    let next = blocks, text = event.text || '';
    switch (event.type) {
      case 'user': next.push({ kind: 'user', text, attachments: event.attachments || [], ts: event.ts || event.timestamp }); return next.length - 1;
      case 'started': return -1;
      // A reset (new chat or resume) starts from an empty transcript: the hero
      // shows for a new chat, and a resume replays history right after.
      case 'reset': next.splice(0, next.length); return null;
      case 'delta': case 'thinking': {
        let kind = event.type === 'thinking' ? 'thinking' : 'assistant', last = next.at(-1);
        if (last && last.kind === kind) { last.text += text; return next.length - 1; }
        next.push({ kind, text, ts: event.ts || event.timestamp }); return next.length - 1;
      }
      case 'tool_call': next.push({ kind: 'tool', id: event.id, name: event.name || event.id, args: event.args || '', ts: event.ts || event.timestamp, text: '', is_error: false, done: false }); return next.length - 1;
      case 'tool_output': {
        let index = next.length - 1;
        while (index >= 0 && (next[index].kind !== 'tool' || next[index].id !== event.id)) index--;
        let tool = next[index];
        if (!tool) { tool = { kind: 'tool', id: event.id, name: event.id, args: '', text: '', is_error: false, done: false }; next.push(tool); index = next.length - 1; }
        tool.text = text; tool.is_error = !!event.is_error; tool.done = !!event.done; return index;
      }
      case 'question': next.push({ kind: 'question', id: event.id, questions: event.questions || [], done: false, rejected: false, answerText: '' }); return next.length - 1;
      case 'question_done': { let index = next.length - 1; while (index >= 0 && (next[index].kind !== 'question' || next[index].id !== event.id)) index--; let q = next[index]; if (q) { q.done = true; q.rejected = !!event.is_error; q.answerText = event.text || ''; } return index >= 0 ? index : next.length - 1; }
      case 'error': next.push({ kind: 'error', text: event.error || 'unknown error' }); return next.length - 1;
      case 'context_usage': return -1;
      // Agent chose silence (NO_REPLY) or the loop guard dropped the turn.
      case 'silent': return -1;
      case 'aborted': { let dismissed = dismissPendingQuestions(next); next.push(event.error === 'aborted' ? { kind: 'interrupted', text: 'Interrupted', ts: event.ts } : { kind: 'error', text: `turn failed: ${event.error || 'unknown error'}`, ts: event.ts }); return dismissed ? null : next.length - 1; }
      case 'closed': { let dismissed = dismissPendingQuestions(next); next.push({ kind: 'error', text: `chat session closed${event.error ? `: ${event.error}` : ''}` }); return dismissed ? null : next.length - 1; }
      case 'finished': return dismissPendingQuestions(next) ? null : -1;
    }
    return next.length - 1;
  };

  const reduce = (blocks, event) => {
    let next = blocks.slice();
    reduceInto(next, event);
    return next;
  };

  // Pending questions can't outlive their turn: any terminal turn event marks
  // them dismissed so the UI never sticks on "pending" (e.g. when an abort
  // discards a late opencode rejected event). 'reset' already replaces every
  // block wholesale.
  const dismissPendingQuestions = blocks => {
    let changed = false;
    for (const block of blocks) if (block.kind === 'question' && !block.done) { block.done = true; block.rejected = true; block.answerText = 'dismissed'; changed = true; }
    return changed;
  };

  const agentName = () => (typeof document !== 'undefined' && document.getElementById('chat-root') && document.getElementById('chat-root').dataset.agentName) || 'Agent';

  const timeLabel = ts => { if (!ts) return ''; let d = new Date(typeof ts === 'number' ? ts : Date.parse(ts)); return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }); };
  const argSummary = args => { try { let value = typeof args === 'string' ? JSON.parse(args) : args; for (let key of ['path','command','query','url']) if (value && value[key]) return String(value[key]).replace(/\s+/g, ' ').slice(0, 90); } catch (_) {} return String(args || '').replace(/\s+/g, ' ').slice(0, 90); };
  const stamp = block => block.ts ? `<time class="chat-time" datetime="${esc(new Date(block.ts).toISOString())}">${esc(timeLabel(block.ts))}</time>` : '';

  const html = (blocks, qsel = {}, readOnly = false) => blocks.map((block, i) => {
    if (block.kind === 'tool') {
      let state = block.is_error ? 'bad' : block.done ? 'done' : 'live';
      let label = block.is_error ? 'error' : block.done ? 'done' : 'running';
      return `<details class="chat-tool" data-tool-id="${esc(block.id)}" data-bi="${i}"><summary><span class="chat-pill chat-pill-${state}">${label}</span><span class="chat-tool-name">${esc(block.name)}</span><span class="chat-tool-summary">${esc(argSummary(block.args))}</span></summary><pre class="chat-tool-args">${esc(block.args)}</pre><pre class="chat-tool-output${block.is_error ? ' is-error' : ''}${block.done ? ' is-done' : ''}">${esc(block.text)}</pre></details>`;
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
              return `<button type="button" class="chat-q-opt${selected ? ' is-selected' : ''}" data-qbi="${i}" data-qi="${qi}" data-label="${esc(o.label)}" title="${esc(o.description || '')}" aria-pressed="${selected}"${block.done || readOnly ? ' disabled' : ''}>${esc(o.label)}</button>`;
            }).join('')}</div>`
          : (block.questions.length > 1 ? `<div class="chat-q-note">No options — this question needs a text answer, not supported in a multi-question request.</div>` : '');
        return `<div class="chat-q"><div class="chat-q-header">${esc(q.header || '')}</div><div class="chat-q-text">${esc(q.question || '')}</div>${optsHtml}</div>`;
      }).join('');
      // A question with no options is implicitly free-text even if `custom`
      // is false; the custom row stays limited to single-question blocks.
      let q0 = block.questions[0], effectiveCustom = block.questions.length === 1 && (q0.custom || !(q0.options || []).length);
      let custom = !block.done && effectiveCustom ? `<div class="chat-q-customrow"><input class="chat-q-custom" data-qbi="${i}" placeholder="Custom answer"${readOnly ? ' readonly' : ''}><button type="button" class="chat-q-send" data-qbi="${i}"${readOnly ? ' disabled' : ''}>Answer</button></div>` : '';
      // Any block containing a `multiple` question (pure or mixed) defers to
      // an explicit Answer button instead of auto-submitting on first click.
      let answerBtn = !block.done && !effectiveCustom && hasMultiple ? `<div class="chat-q-customrow"><button type="button" class="chat-q-answer-btn" data-qbi="${i}"${readOnly ? ' disabled' : ''}>Answer</button></div>` : '';
      let answered = block.done && block.answerText ? `<div class="chat-q-answer">${esc(block.answerText)}</div>` : '';
      return `<div class="chat-question" data-question-id="${esc(block.id)}"><span class="chat-pill chat-pill-${state}">${label}</span>${body}${custom}${answerBtn}${answered}</div>`;
    }
    if (block.kind === 'thinking') {
      return `<details class="chat-think" data-bi="${i}"><summary>Thinking</summary><pre>${esc(block.text)}</pre></details>`;
    }
    if (block.kind === 'interrupted') return `<div class="chat-interrupted">Interrupted${stamp(block)}</div>`;
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
    return `<div class="chat-turn chat-${block.kind}"><div class="chat-role"><span>${roleLabel}</span>${stamp(block)}</div><div class="chat-body">${markdown(block.text)}${attachments ? `<div class="chat-attach-row">${attachments}</div>` : ''}</div></div>`;
  }).join('');

  const usageText = event => event.context_window ? `${event.tokens_used} / ${event.context_window} tokens` : `${event.tokens_used} tokens`;

  const request = async (url, options) => {
    const response = await fetch(url, options);
    if (response.ok) return response;
    let detail = await response.text().catch(() => '');
    try { detail = JSON.parse(detail).error || detail; } catch (_) { /* plain-text response */ }
    throw new Error(detail || `request failed (${response.status})`);
  };

  if (typeof module !== 'undefined') module.exports = { reduce, html, markdown, usageText, request };
  if (typeof document === 'undefined') return;

  const SESSION = 'main', MAX_ATTACHMENTS = 8;
  const $ = id => document.getElementById(id);
  let pasteSeq = 0;

  const sendEnabled = app => app.draft.trim() !== '' || app.pending.length > 0;

  const autogrow = el => { if (!el) return; el.style.height = 'auto'; el.style.height = Math.min(el.scrollHeight, Math.round(0.4 * window.innerHeight)) + 'px'; };


  const renderChips = app => {
    let host = $('chat-chips'); if (!host) return;
    host.innerHTML = app.pending.map((file, i) => `<span class="chat-chip">${file.type && file.type.startsWith('image/') ? `<img src="${file._preview || (file._preview = URL.createObjectURL(file))}" alt="">` : ''}<span class="chat-chip-name">${esc(file.name)}</span><button type="button" class="chat-chip-x" data-chip="${i}" aria-label="Remove attachment">×</button></span>`).join('');
  };

  const capHint = (app, dropped) => {
    let host = $('chat-cap-hint'); if (!host) return;
    host.textContent = dropped > 0 ? `Only ${MAX_ATTACHMENTS} attachments per message — ${dropped} file${dropped === 1 ? '' : 's'} skipped.` : '';
  };

  // Single intake point for the file input, drag-and-drop, and clipboard
  // paste: caps at MAX_ATTACHMENTS and surfaces how many were dropped.
  const addFiles = (app, fileList) => {
    let files = Array.from(fileList), room = Math.max(MAX_ATTACHMENTS - app.pending.length, 0);
    for (const file of files.slice(0, room)) app.pending.push(file);
    capHint(app, Math.max(files.length - room, 0));
    renderChips(app); refreshBusy(app);
  };

  const toggleDropzone = (app, show) => { let el = $('chat-dropzone'); if (el) el.hidden = !show; };

  // Clipboard screenshots all arrive as a generic "image.png"; give repeated
  // pastes distinct names so they don't collide in the chip row.
  const pasteName = name => /^image\.\w+$/i.test(name) ? name.replace(/^image/i, `pasted-${Date.now()}-${++pasteSeq}`) : name;

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
    let transcript = $('chat-transcript'); if (!transcript) return;
    let stick = app.stick;
    let empty = !app.blocks.some(b => b.kind === 'user' || b.kind === 'assistant');
    let root = $('chat-root');
    if (empty && root?.classList && !root.classList.contains('is-empty')) pickHeroLine();
    root?.classList?.toggle('is-empty', empty);
    let last = app.blocks[app.blocks.length - 1];
    let pending = app.turns > 0 && last && last.kind === 'user';
    let open = new Set(Array.from(transcript.querySelectorAll('details[open]'), d => d.dataset.bi));
    transcript.innerHTML = html(app.blocks, app.qsel) + (pending ? pendingHtml(app.workingWord || 'Working') : '');
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
    if (event.type === 'context_usage') { let m = $('chat-token-meter'), w=$('chat-context-warning'); if (m) m.textContent = usageText(event); if(w) w.hidden=!(event.context_window && event.tokens_used/event.context_window>=.7); return; }
    if (event.type === 'reset') { app.turns=0; app.qsel={}; app.qsent={}; let m=$('chat-token-meter'),w=$('chat-context-warning'); if(m)m.textContent=''; if(w)w.hidden=true; }
    if (!event.historical && event.type === 'started' && event.origin === 'autonomous') { app.turns++; refreshBusy(app); return; }
    if (!event.historical && event.type === 'user') { app.turns++; app.workingWord = WORKING_WORDS[Math.floor(Math.random() * WORKING_WORDS.length)]; }
    if (!event.historical && (event.type === 'finished' || event.type === 'aborted' || event.type === 'closed')) app.turns = Math.max(0, app.turns - 1);
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
    if (!files.length && command === '/stop') { fetch(`/chat/${SESSION}/abort`, { method: 'POST' }); app.draft=''; input.value=''; autogrow(input); refreshBusy(app); return; }
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
    document.addEventListener('keydown', e => {
      if (e.target.id === 'chat-input' && e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendFlow(app); return; }
      // Session rows are role=button: Enter/Space resumes, like a click.
      if (e.target.classList?.contains('chat-session') && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); resumeSession(app, e.target.dataset.sessionId); }
    });
    document.addEventListener('input', e => { if (e.target.id === 'chat-input') { app.draft = e.target.value; autogrow(e.target); refreshBusy(app); } });
    document.addEventListener('change', e => {
      if (e.target.id !== 'chat-attachments') return;
      addFiles(app, e.target.files);
      e.target.value = '';
    });
    document.addEventListener('paste', e => {
      if (e.target.id !== 'chat-input') return;
      let files = e.clipboardData && e.clipboardData.files;
      if (!files || !files.length) return;
      e.preventDefault();
      addFiles(app, Array.from(files).map(file => new File([file], pasteName(file.name), { type: file.type })));
    });
    // dragover must preventDefault() unconditionally wherever a file is over
    // the document, not just the drop zone — otherwise a near-miss drop
    // outside #chat-root navigates the whole page to the file.
    document.addEventListener('dragover', e => { if (e.dataTransfer.types.includes('Files')) e.preventDefault(); });
    document.addEventListener('dragenter', e => {
      if (!e.dataTransfer.types.includes('Files') || !e.target.closest('#chat-root')) return;
      e.preventDefault();
      app.dragDepth++; toggleDropzone(app, true);
    });
    // Enter/leave depth counter (not bare dragleave) so moving over child
    // elements inside the zone doesn't flicker the overlay.
    document.addEventListener('dragleave', e => {
      if (!e.dataTransfer.types.includes('Files') || !e.target.closest('#chat-root')) return;
      app.dragDepth = Math.max(0, app.dragDepth - 1);
      if (!app.dragDepth) toggleDropzone(app, false);
    });
    document.addEventListener('drop', e => {
      if (!e.dataTransfer.types.includes('Files')) return;
      e.preventDefault();
      app.dragDepth = 0; toggleDropzone(app, false);
      if (e.target.closest('#chat-root')) addFiles(app, e.dataTransfer.files);
    });
    document.addEventListener('click', e => {
      if (e.target.closest('[data-sidebar-toggle]')) { toggleSidebar(); return; }
      let action = e.target.closest('[data-session-action]');
      if (action) { sessionAction(app, action.dataset.sessionAction, action.dataset.sessionId); return; }
      let session = e.target.closest('[data-session-id]');
      if (session && !e.target.closest('input,button,[data-session-action]')) { resumeSession(app, session.dataset.sessionId); return; }
      let chip = e.target.closest('.chat-chip-x');
      if (chip) { let removed = app.pending.splice(Number(chip.dataset.chip), 1)[0]; if (removed && removed._preview) URL.revokeObjectURL(removed._preview); renderChips(app); refreshBusy(app); return; }
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
    // Keep the Archived section's open state across periodic list refreshes.
    document.addEventListener('toggle', e => { if (e.target.classList?.contains('chat-archived')) app.archivedOpen = e.target.open; }, true);
    document.addEventListener('mouseover', e => { let row = e.target.closest?.('.chat-session'); if (row && !row.contains(e.relatedTarget)) revealTitle(row); });
    document.addEventListener('scroll', e => { if (e.target.id === 'chat-transcript') { let t = e.target; app.stick = (t.scrollHeight - t.scrollTop - t.clientHeight) < 64; } }, true);
  };

  const mount = app => {
    let transcript = $('chat-transcript'), input = $('chat-input');
    if (!transcript || !input) return;
    paint(app);
    input.value = app.draft; autogrow(input);
    renderChips(app); refreshBusy(app);
    transcript.scrollTop = transcript.scrollHeight;
    pickHeroLine();
    // Avatar images get their src here so the fleet dashboard prefix applies to local paths.
    for (const img of document.querySelectorAll?.('img[data-avatar-src]') || []) img.src = /^https?:/.test(img.dataset.avatarSrc) ? img.dataset.avatarSrc : PREFIX + img.dataset.avatarSrc;
    $('chat-root').classList?.toggle('sidebar-collapsed', typeof localStorage !== 'undefined' && localStorage.getItem('dar-chat-sidebar') === 'collapsed');
    showHistoryList(app);
  };

  // Empty-chat invite; a fresh line is drawn each time the chat becomes empty.
  const HERO_LINES = [
    "What are we building today?", "Where do you want to start?", "What's on your mind?", "Ready when you are.",
    "What can I help you ship?", "Got something to untangle?", "Let's make something good.", "What's the mission?",
    "First thought, best thought.", "What are you curious about?", "Drop the big question.", "What needs doing?",
    "Say the word.", "New session, fresh ideas.", "What's brewing?", "Pick a thread to pull.",
    "What should we tackle first?", "Blank slate, endless options.", "What's the puzzle today?", "Ask me anything.",
    "Let's get to work.", "What's on the docket?", "Bring me your hardest problem.", "What are we exploring?",
    "Start anywhere. I'll keep up.", "What's the goal today?", "Give me the gist.", "What's next on the list?",
    "Fire away.", "What's the plan, boss?",
  ];
  const pickHeroLine = () => {
    let hero = $('chat-hero-line');
    if (hero) hero.textContent = HERO_LINES[Math.floor(Math.random() * HERO_LINES.length)];
  };

  // Slide an overflowing title left on hover so its tail clears the action icons.
  const revealTitle = row => {
    let box = row.querySelector('.chat-session-label'), text = box?.firstElementChild;
    if (!text) return;
    let reserve = row.querySelector('.chat-session-actions')?.offsetWidth || 0;
    let shift = text.scrollWidth + reserve - box.clientWidth;
    row.style.setProperty('--title-shift', `${Math.max(0, shift)}px`);
    row.style.setProperty('--title-ms', `${Math.max(1200, shift * 16) / 0.3}ms`);
  };

  const ICONS = {
    rename: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z"/>',
    archive: '<rect x="3" y="4" width="18" height="4" rx="1"/><path d="M5 8v11a1 1 0 001 1h12a1 1 0 001-1V8M10 12h4"/>',
    unarchive: '<rect x="3" y="4" width="18" height="4" rx="1"/><path d="M5 8v11a1 1 0 001 1h12a1 1 0 001-1V8M12 18v-6M9.5 14.5L12 12l2.5 2.5"/>',
    delete: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6"/>',
  };
  const actionButton = (action, id, label) => `<button type="button" data-session-action="${action}" data-session-id="${esc(id)}" aria-label="${label}" title="${label}"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[action]}</svg></button>`;

  const relativeTime = ms => { let n=Math.max(0,Date.now()-ms), m=Math.floor(n/60000); if(m<1)return 'now'; if(m<60)return `${m}m`; let h=Math.floor(m/60); if(h<24)return `${h}h`; return `${Math.floor(h/24)}d`; };
  const groupName = ms => { let d=new Date(ms), now=new Date(), day=86400000, today=new Date(now.getFullYear(),now.getMonth(),now.getDate()).getTime(), t=new Date(d.getFullYear(),d.getMonth(),d.getDate()).getTime(); if(t===today)return 'Today'; if(t===today-day)return 'Yesterday'; let age=(today-t)/day; if(age<7)return 'Earlier this week'; if(d.getMonth()===now.getMonth()&&d.getFullYear()===now.getFullYear())return 'Earlier this month'; return d.toLocaleDateString([], {month:'long',year:'numeric'}); };
  const sessionRow = s => `<div class="chat-session${s.is_live ? ' is-live' : ''}" data-session-id="${esc(s.id)}" role="button" tabindex="0" title="${esc(s.label)}">`
    + `<div class="chat-session-copy"><span class="chat-session-label"><span>${esc(s.label)}</span></span><small>${s.is_live ? '<span class="chat-live-dot"></span>Live · ' : ''}${relativeTime(s.modified_ms)}</small></div>`
    + `<div class="chat-session-actions">${actionButton('rename', s.id, 'Rename')}${s.archived ? actionButton('unarchive', s.id, 'Unarchive') : actionButton('archive', s.id, 'Archive')}${actionButton('delete', s.id, 'Delete')}</div></div>`;
  const showHistoryList = async app => { let host=$('chat-history-list'); if(!host||app.editingSession)return; let sequence=++app.sidebarSequence; try { let all=await request('/chat/sessions').then(r=>r.json()); if(app.editingSession||sequence!==app.sidebarSequence)return; let q=($('chat-search')?.value||'').toLowerCase(), active=all.filter(s=>!s.archived&&s.label.toLowerCase().includes(q)), archived=all.filter(s=>s.archived&&s.label.toLowerCase().includes(q)), groups=[]; for(let s of active){let name=groupName(s.modified_ms), g=groups.at(-1);if(!g||g.name!==name)groups.push(g={name,items:[]});g.items.push(s)} host.innerHTML=groups.map(g=>`<div class="chat-session-group"><h3>${g.name}</h3>${g.items.map(sessionRow).join('')}</div>`).join('')+(archived.length?`<details class="chat-archived"${app.archivedOpen?' open':''}><summary>Archived (${archived.length})</summary>${archived.map(sessionRow).join('')}</details>`:'')||'<div class="chat-empty">No conversations</div>'; } catch(error){if(sequence===app.sidebarSequence&&!app.editingSession)host.textContent=`Sessions unavailable: ${error.message}`;} };
  const resumeSession = async (app,id) => { try { await request(`/chat/${SESSION}/resume`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({id})}); showHistoryList(app); } catch(error){render(app,{type:'error',error:`Session not resumed: ${error.message}`});} };
  const sessionAction = async (app,action,id) => { try { if(action==='delete'){if(!confirm('Delete this conversation permanently?'))return;await request(`/chat/sessions/${encodeURIComponent(id)}`,{method:'DELETE'});} else if(action==='rename'){let row=document.querySelector(`[data-session-id="${CSS.escape(id)}"]`), label=row?.querySelector('.chat-session-label');if(!label)return;let input=document.createElement('input');input.className='chat-session-edit';input.value=label.textContent;label.replaceWith(input);input.focus();input.select();app.editingSession=id;let settled=false;let done=async save=>{if(settled)return;settled=true;app.editingSession=null;if(save)await request(`/chat/sessions/${encodeURIComponent(id)}`,{method:'PATCH',headers:{'content-type':'application/json'},body:JSON.stringify({title:input.value.trim()})});showHistoryList(app)};input.addEventListener('keydown',e=>{if(e.key==='Enter'){e.preventDefault();done(true)}if(e.key==='Escape'){e.preventDefault();done(false)}});input.addEventListener('blur',()=>done(true),{once:true});return;} else await request(`/chat/sessions/${encodeURIComponent(id)}`,{method:'PATCH',headers:{'content-type':'application/json'},body:JSON.stringify({archived:action==='archive'})}); showHistoryList(app); } catch(error){render(app,{type:'error',error:`Session action failed: ${error.message}`});} };
  const toggleSidebar = () => { let root=$('chat-root'); if(matchMedia('(max-width: 760px)').matches){root.classList.toggle('sidebar-open');return;} let collapsed=!root.classList.contains('sidebar-collapsed');root.classList.toggle('sidebar-collapsed',collapsed);localStorage.setItem('dar-chat-sidebar',collapsed?'collapsed':'open'); };

  if (!window.__chatWeb) {
    let app = { blocks: [], draft: '', pending: [], turns: 0, stick: true, es: null, paintScheduled: false, qsel: {}, qsent: {}, editingSession: null, sidebarSequence: 0, dragDepth: 0 };
    window.__chatWeb = app;
    window.renderChatEvent = event => render(app, event);
    app.es = new EventSource(`/chat/${SESSION}/stream`);
    app.es.onmessage = e => render(app, JSON.parse(e.data));
    bindDocument(app);
    window.addEventListener('focus',()=>showHistoryList(app)); document.addEventListener('visibilitychange',()=>{if(!document.hidden)showHistoryList(app)}); if (typeof module === 'undefined') setInterval(()=>{if(!document.hidden)showHistoryList(app)},5000); document.addEventListener('input',e=>{if(e.target.id==='chat-search')showHistoryList(app)});
  }
  mount(window.__chatWeb);
})();
