#!/usr/bin/env node
// Deterministic runner wire fixture; tool results come from the real Dar bridge.
const { spawn } = require('node:child_process');
const readline = require('node:readline');
const fs = require('node:fs');
const path = require('node:path');
const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
let call = 0;
let queue = Promise.resolve();
async function bridgeCall(label) {
  const child = spawn(process.env.DAR_APPS_TEST_BINARY, ['__mcp-bridge', '--dir', process.cwd()], { stdio: ['pipe', 'pipe', 'pipe'] });
  const pending = new Map();
  const lines = readline.createInterface({ input: child.stdout });
  lines.on('line', line => { const response = JSON.parse(line); pending.get(response.id)?.(response); });
  const request = (id, method, params) => new Promise(resolve => {
    pending.set(id, resolve);
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  try {
    await request(1, 'initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'browser-test', version: '1' } });
    const response = await request(2, 'tools/call', { name: 'canvas__draw', arguments: { label } });
    if (response.error) throw new Error(response.error.message);
    return response.result;
  } finally { child.stdin.end(); }
}
readline.createInterface({ input: process.stdin }).on('line', line => {
  const command = JSON.parse(line);
  if (command.type === 'get_state') {
    emit({ type: 'response', command: 'get_state', id: command.id, success: true, data: { sessionId: 'apps-acceptance' } });
    return;
  }
  if (command.type !== 'prompt') return;
  queue = queue.then(async () => {
    emit({ type: 'response', command: 'prompt', id: command.id, success: true });
    emit({ type: 'agent_start' });
    emit({ type: 'message_start', message: { role: 'user' } });
    fs.appendFileSync(path.join(process.cwd(), 'prompts.jsonl'), JSON.stringify({ message: command.message }) + '\n');
    if (command.message.includes('draw first') || command.message.includes('draw second')) {
      const label = command.message.includes('draw second') ? 'second' : 'first';
      const id = `drawing-${++call}`;
      emit({ type: 'message_update', assistantMessageEvent: { type: 'toolcall_end', toolCall: { id, name: 'canvas__draw', arguments: { label } } } });
      const result = await bridgeCall(label);
      emit({ type: 'tool_execution_end', toolCallId: id, result, isError: false });
    }
    emit({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: 'Done.' } });
    emit({ type: 'agent_end', messages: [] });
  }).catch(error => { emit({ type: 'message_update', assistantMessageEvent: { type: 'error', error: error.message } }); emit({ type: 'agent_end' }); });
});
