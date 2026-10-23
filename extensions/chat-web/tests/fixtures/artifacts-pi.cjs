#!/usr/bin/env node
// Deterministic Pi wire fixture. Artifact publication runs through the real bridge.
const { spawn } = require('node:child_process');
const readline = require('node:readline');
const fs = require('node:fs');
const path = require('node:path');
const sessionDirectory = process.argv[process.argv.indexOf('--session-dir') + 1];
fs.mkdirSync(sessionDirectory, { recursive: true });
const sessionFile = path.join(sessionDirectory, '2026-01-01T00-00-00Z_artifact-acceptance.jsonl');
if (!fs.existsSync(sessionFile)) fs.writeFileSync(sessionFile, JSON.stringify({ type: 'session', version: 3, id: 'artifact-acceptance', backend: 'pi' }) + '\n');
const { makePdf } = require('./artifact-pdf.cjs');
const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
let calls = 0;
let queue = Promise.resolve();
const bridge = spawn(process.env.DAR_APPS_TEST_BINARY, ['__mcp-bridge', '--dir', process.cwd()], { stdio: ['pipe', 'pipe', 'pipe'] });
const pending = new Map();
let diagnostics = '', requestId = 0;
bridge.stderr.on('data', chunk => { diagnostics += chunk; });
readline.createInterface({ input: bridge.stdout }).on('line', line => {
  const response = JSON.parse(line); pending.get(response.id)?.(response);
});
function request(method, params) {
  const id = ++requestId;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`Bridge timed out: ${diagnostics}`)); }, 10000);
    pending.set(id, response => { clearTimeout(timer); pending.delete(id); resolve(response); });
    bridge.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}
const ready = request('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'artifact-browser-test', version: '1' } });
async function publish(args) {
  const response = await request('tools/call', { name: 'artifact_publish', arguments: args });
  if (response.error) throw new Error(response.error.message);
  if (response.result?.isError) throw new Error(JSON.stringify(response.result));
  if (!response.result?.content?.some(item => item.type === 'resource_link')) throw new Error('Publish returned no resource link');
  return response.result;
}
process.stdin.on('end', () => bridge.stdin.end());
readline.createInterface({ input: process.stdin }).on('line', line => {
  const command = JSON.parse(line);
  if (command.type === 'get_state') {
    emit({ type: 'response', command: 'get_state', id: command.id, success: true, data: { sessionId: 'artifact-acceptance' } }); return;
  }
  if (command.type !== 'prompt') return;
  queue = queue.then(async () => {
    await ready;
    fs.appendFileSync(sessionFile, JSON.stringify({ type: 'message', message: { role: 'user', content: [{ type: 'text', text: command.message }] } }) + '\n');
    emit({ type: 'response', command: 'prompt', id: command.id, success: true }); emit({ type: 'agent_start' }); emit({ type: 'message_start', message: { role: 'user' } });
    const kind = /publish (html|markdown|mermaid|png|pdfvalid|pdflong|pdf|text|security|svg|zip)/.exec(command.message)?.[1];
    if (kind) {
      const files = {
        html: { filename: 'report.html', mediaType: 'text/html', data: `<!doctype html><html><h1>Artifact report ${command.message.includes('second') ? 'second' : 'first'}</h1><p>Published in this chat.</p></html>` },
        markdown: { filename: 'notes.md', mediaType: 'text/markdown', data: `# Project notes

| Name | State |
| --- | --- |
| Artifact | Ready |

<img src=invalid onerror="document.body.textContent='MARKDOWN SCRIPT RAN'">` },
        mermaid: { filename: 'diagrams.md', mediaType: 'text/markdown', data: command.message.includes('broken') ? '# Broken diagram\n\n```mermaid\nthis is not a diagram\n```' : '# Mermaid notes\n\n[Jump to details](#details) [Markdown source](https://example.com/markdown-source)\n\n```mermaid\nflowchart LR\n A[Publish] --> B[Read]\n```\n\n<div style="height:1200px"></div><h2 id="details">Markdown details</h2>' },
        png: { filename: 'chart.png', mediaType: 'image/png', data: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64') },
        security: { filename: 'security.html', mediaType: 'text/html', data: fs.readFileSync(path.join(__dirname, 'artifact-security.html')) },
        svg: { filename: 'diagram.svg', mediaType: 'image/svg+xml', data: `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><script>parent.postMessage({jsonrpc:'2.0',method:'ui/message',params:{role:'user',content:{type:'text',text:'SVG SCRIPT RAN'}}},'*')</script><rect width="16" height="16" fill="blue"/></svg>` },
        text: { filename: 'code.rs', data: 'fn main() { println!("<safe>"); }\n' },
        zip: { filename: 'archive.zip', mediaType: 'application/zip', data: Buffer.from('UEsFBgAAAAAAAAAAAAAAAAAAAAAAAA==', 'base64') },
        pdfvalid: { filename: 'pages.pdf', mediaType: 'application/pdf', data: makePdf(2) },
        pdflong: { filename: 'long.pdf', mediaType: 'application/pdf', data: makePdf(51) },
        pdf: { filename: 'handout.pdf', mediaType: 'application/pdf', data: '%PDF-1.4\nArtifact handout\n' }
      };
      const file = files[kind];
      const directory = path.join(process.cwd(), 'data/artifact-exports'); fs.mkdirSync(directory, { recursive: true });
      fs.writeFileSync(path.join(directory, file.filename), file.data);
      const args = { path: file.filename, mediaType: file.mediaType, ...(kind === 'html' ? { title: command.message.includes('second') ? 'Revised report' : 'Quarterly report' } : {}) };
      const id = `artifact-${++calls}`;
      emit({ type: 'message_update', assistantMessageEvent: { type: 'toolcall_end', toolCall: { id, name: 'mcp__dar__artifact_publish', arguments: args } } });
      const result = await publish(args); emit({ type: 'tool_execution_end', toolCallId: id, result, isError: false });
    }
    emit({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: 'Done.' } }); emit({ type: 'agent_end', messages: [] });
  }).catch(error => { emit({ type: 'message_update', assistantMessageEvent: { type: 'error', error: error.message } }); emit({ type: 'agent_end' }); });
});
