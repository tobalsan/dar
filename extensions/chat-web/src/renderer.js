(function () {
  const esc = s => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');

  const markdown = s => {
    let code = [];
    let html = esc(s).replace(/```([^\n]*)\n([\s\S]*?)```/g, (_, lang, text) => `\0${code.push(`<pre><code${lang ? ` data-language="${esc(lang)}"` : ''}>${text}</code></pre>`) - 1}\0`).replace(/`([^`\n]+)`/g, (_, text) => `\0${code.push(`<code>${text}</code>`) - 1}\0`);
    html = html.replace(/^[-*] (.+)$/gm, '<li>$1</li>').replace(/(?:<li>[\s\S]*?<\/li>\n?)+/g, m => `<ul>${m}</ul>`).replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>').replace(/\*(.+?)\*/g, '<em>$1</em>').replace(/\n/g, '<br>').replace(/<\/li><br><\/ul>/g, '</li></ul>');
    return html.replace(/\0(\d+)\0/g, (_, i) => code[i]);
  };

  const reduce = (blocks, event) => {
    let next = blocks.map(block => ({ ...block })), text = event.text || '';
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
      case 'error': next.push({ kind: 'error', text: event.error || 'unknown error' }); break;
      case 'context_usage': break;
      case 'aborted': next.push({ kind: 'error', text: event.error === 'aborted' ? 'turn aborted' : `turn failed: ${event.error || 'unknown error'}` }); break;
      case 'closed': next.push({ kind: 'error', text: `chat session closed${event.error ? `: ${event.error}` : ''}` }); break;
    }
    return next;
  };

  const agentName = () => (typeof document !== 'undefined' && document.getElementById('chat-root') && document.getElementById('chat-root').dataset.agentName) || 'Agent';

  const html = blocks => blocks.map(block => {
    if (block.kind === 'tool') {
      let state = block.is_error ? 'bad' : block.done ? 'done' : 'live';
      let label = block.is_error ? 'error' : block.done ? 'done' : 'running';
      return `<details class="chat-tool" data-tool-id="${esc(block.id)}"><summary><span class="chat-pill chat-pill-${state}">${label}</span><span class="chat-tool-name">${esc(block.name)}</span></summary><pre class="chat-tool-args">${esc(block.args)}</pre><pre class="chat-tool-output${block.is_error ? ' is-error' : ''}${block.done ? ' is-done' : ''}">${esc(block.text)}</pre></details>`;
    }
    if (block.kind === 'thinking') {
      return `<details class="chat-think"><summary>Thinking</summary><pre>${esc(block.text)}</pre></details>`;
    }
    if (block.kind === 'error') {
      return `<div class="chat-turn chat-error"><span class="chat-pill chat-pill-bad">error</span><div class="chat-error-body">${markdown(block.text)}</div></div>`;
    }
    if (block.kind === 'notice') {
      return `<div class="chat-notice">${esc(block.text)}</div>`;
    }
    let roleLabel = block.kind === 'user' ? 'You' : block.kind === 'assistant' ? esc(agentName()) : block.kind;
    let attachments = (block.attachments || []).map(a => a.image
      ? `<img class="chat-attachment-image" src="${esc(a.url)}" alt="${esc(a.name)}">`
      : `<a class="chat-attachment" href="${esc(a.url)}" target="_blank" rel="noopener noreferrer">${esc(a.name)}</a>`).join('');
    return `<div class="chat-turn chat-${block.kind}"><div class="chat-role">${roleLabel}</div><div class="chat-body">${markdown(block.text)}${attachments ? `<div class="chat-attach-row">${attachments}</div>` : ''}</div></div>`;
  }).join('');

  const usageText = event => event.context_window ? `${event.tokens_used} / ${event.context_window} tokens` : `${event.tokens_used} tokens`;

  if (typeof module !== 'undefined') module.exports = { reduce, html, usageText };
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
    let busy = app.turns > 0, status = $('chat-status'), abort = $('chat-abort'), send = $('chat-send');
    if (status) { status.className = 'chat-status' + (busy ? ' is-busy' : ''); status.textContent = busy ? 'working' : ''; }
    if (abort) { abort.disabled = !busy; abort.hidden = !busy; }
    if (send) send.disabled = !sendEnabled(app);
  };

  const paint = app => {
    let transcript = $('chat-transcript'); if (!transcript) return;
    let stick = app.stick;
    transcript.innerHTML = app.blocks.length ? html(app.blocks) : '<div class="chat-empty">No messages yet. Ask the agent anything.</div>';
    if (stick) transcript.scrollTop = transcript.scrollHeight;
  };

  const render = (app, event) => {
    if (event.type === 'context_usage') { let m = $('chat-token-meter'); if (m) m.textContent = usageText(event); return; }
    if (event.type === 'user') app.turns++;
    if (event.type === 'finished' || event.type === 'aborted' || event.type === 'closed') app.turns = Math.max(0, app.turns - 1);
    app.blocks = reduce(app.blocks, event);
    paint(app);
    refreshBusy(app);
  };

  const sendFlow = async app => {
    let input = $('chat-input');
    if (!input || !sendEnabled(app)) return;
    let message = input.value, files = app.pending.slice(), command_id = crypto.randomUUID();
    let command = message.trim();
    if (!files.length && command === '/new') {
      app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
      try {
        await fetch(`/chat/${SESSION}/new`, { method: 'POST' });
      } catch (_) {
        app.draft = message; input.value = message; app.pending = files; renderChips(app); autogrow(input); refreshBusy(app);
      }
      return;
    }
    if (!files.length && command === '/compact') {
      app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
      try {
        await fetch(`/chat/${SESSION}/compact`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ command_id: crypto.randomUUID() }) });
      } catch (_) {
        app.draft = message; input.value = message; app.pending = files; renderChips(app); autogrow(input); refreshBusy(app);
      }
      return;
    }
    app.draft = ''; input.value = ''; app.pending = []; renderChips(app); autogrow(input); refreshBusy(app);
    try {
      if (files.length) {
        let body = new FormData();
        body.append('command_id', command_id); body.append('message', message);
        for (const file of files) body.append('attachment', file);
        await fetch(`/chat/${SESSION}/upload`, { method: 'POST', body });
      } else {
        await fetch(`/chat/${SESSION}/send`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ command_id, message }) });
      }
    } catch (_) {
      app.draft = message; input.value = message; app.pending = files; renderChips(app); autogrow(input); refreshBusy(app);
    }
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
      let chip = e.target.closest('.chat-chip-x');
      if (chip) { app.pending.splice(Number(chip.dataset.chip), 1); renderChips(app); refreshBusy(app); return; }
      if (e.target.closest('#chat-attach')) { let f = $('chat-attachments'); if (f) f.click(); return; }
      let abort = e.target.closest('#chat-abort');
      if (abort && !abort.disabled) fetch(`/chat/${SESSION}/abort`, { method: 'POST' });
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
  };

  if (!window.__chatWeb) {
    let app = { blocks: [], draft: '', pending: [], turns: 0, stick: true, es: null };
    window.__chatWeb = app;
    window.renderChatEvent = event => render(app, event);
    app.es = new EventSource(`/chat/${SESSION}/stream`);
    app.es.onmessage = e => render(app, JSON.parse(e.data));
    bindDocument(app);
    window.addEventListener('resize', sizeViewport);
  }
  mount(window.__chatWeb);
})();
