#!/usr/bin/env node
// Deterministic Pi wire fixture: archives and usage cross the real chat backend.
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');
const { randomUUID } = require('node:crypto');
const argument = name => process.argv[process.argv.indexOf(name) + 1];
const directory = argument('--session-dir');
fs.mkdirSync(directory, { recursive: true });
const id = process.argv.includes('--session') ? argument('--session') : randomUUID();
let file = fs.readdirSync(directory).find(name => name.endsWith('.jsonl') && fs.readFileSync(path.join(directory, name), 'utf8').split('\n')[0].includes(id));
if (!file) {
  file = `${new Date().toISOString().replaceAll(':', '-')}_${id}.jsonl`;
  fs.writeFileSync(path.join(directory, file), JSON.stringify({ type: 'session', version: 3, id, backend: 'pi' }) + '\n');
}
const append = message => fs.appendFileSync(path.join(directory, file), JSON.stringify({ type: 'message', message }) + '\n');
const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
readline.createInterface({ input: process.stdin }).on('line', line => {
  const command = JSON.parse(line);
  if (command.type === 'get_state') {
    emit({ type: 'response', command: 'get_state', id: command.id, success: true, data: { sessionId: id } });
    return;
  }
  if (command.type !== 'prompt') return;
  const title = command.message.startsWith('Return a concise 3-6 word title');
  const text = title ? 'Fixture conversation title' : `Fixture reply: ${command.message}`;
  append({ role: 'user', content: [{ type: 'text', text: command.message }] });
  emit({ type: 'response', command: 'prompt', id: command.id, success: true });
  emit({ type: 'agent_start' });
  emit({ type: 'message_start', message: { role: 'user' } });
  emit({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: text } });
  const tokens = Number(/usage (\d+)/.exec(command.message)?.[1] || 83632);
  append({ role: 'assistant', content: [{ type: 'text', text }] });
  emit({ type: 'message_end', message: { role: 'assistant', usage: { input: tokens - 632, output: 632, cacheRead: 0, cacheWrite: 0 } } });
  emit({ type: 'agent_end', messages: [] });
});
