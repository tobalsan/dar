const esc = s => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');

const markdown = s => {
  let code = [];
  let html = esc(s).replace(/```([^\n]*)\n([\s\S]*?)```/g, (_, lang, text) => `\0${code.push(`<pre><code${lang ? ` data-language="${esc(lang)}"` : ''}>${text}</code></pre>`) - 1}\0`);
  html = html.replace(/^[-*] (.+)$/gm, '<li>$1</li>').replace(/(?:<li>[\s\S]*?<\/li>\n?)+/g, m => `<ul>${m}</ul>`).replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>').replace(/\*(.+?)\*/g, '<em>$1</em>').replace(/\n/g, '<br>').replace(/<\/li><br><\/ul>/g, '</li></ul>');
  return html.replace(/\0(\d+)\0/g, (_, i) => code[i]);
};

const reduce = (blocks, event) => {
  let next = blocks.map(block => ({ ...block })), text = event.text || '';
  switch (event.type) {
    case 'user': next.push({ kind: 'user', text }); break;
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
    case 'aborted': next.push({ kind: 'error', text: event.error === 'aborted' ? 'turn aborted' : `turn failed: ${event.error || 'unknown error'}` }); break;
    case 'closed': next.push({ kind: 'error', text: `chat session closed${event.error ? `: ${event.error}` : ''}` }); break;
  }
  return next;
};

const html = blocks => blocks.map(block => block.kind === 'tool'
  ? `<article class="chat-tool" data-tool-id="${esc(block.id)}"><header>${esc(block.name)}</header><pre class="chat-tool-args">${esc(block.args)}</pre><pre class="chat-tool-output${block.is_error ? ' is-error' : ''}${block.done ? ' is-done' : ''}">${esc(block.text)}</pre></article>`
  : `<article class="chat-${block.kind}">${markdown(block.text)}</article>`).join('');

if (typeof module !== 'undefined') module.exports = { reduce, html };
if (typeof document !== 'undefined') {
  let blocks = [], transcript = document.getElementById('chat-transcript');
  const render = event => { blocks = reduce(blocks, event); transcript.innerHTML = html(blocks); };
  window.renderChatEvent = render;
  if (document.getElementById('chat-composer')) {
  const id = sessionStorage.chatSession || (sessionStorage.chatSession = crypto.randomUUID());
  const es = new EventSource(`/chat/${id}/stream`);
  es.onmessage = event => render(JSON.parse(event.data));
  document.getElementById('chat-composer').onsubmit = async event => { event.preventDefault(); let input = document.getElementById('chat-input'); if (!input.value) return; await fetch(`/chat/${id}/send`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ command_id: crypto.randomUUID(), message: input.value }) }); input.value = ''; };
  document.getElementById('chat-abort').onclick = () => fetch(`/chat/${id}/abort`, { method: 'POST' });
  }
}
