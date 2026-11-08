// Activity lines: consecutive tool/thinking steps fold into one line whose
// label, order and reachable details are asserted as the user reads them.
const assert = require('node:assert/strict');
const { reduce, html } = require(process.argv[2]);

const text = fragment => fragment.replace(/<[^>]*>/g, ' ').replace(/&quot;/g, '"').replace(/\s+/g, ' ').trim();
const play = (events, start = []) => events.reduce((blocks, event) => reduce(blocks, event), start);
const T = 1_700_000_000_000;

// Running group: spinner line names the latest action.
let blocks = play([
  { type: 'user', text: 'Add paste support', ts: T },
  { type: 'thinking', text: 'Find the composer file first.\nThen edit it.', ts: T + 400 },
]);
let page = html(blocks);
assert.match(text(page), /Thinking…/, 'a thinking-only running group reads Thinking');
assert.doesNotMatch(text(page), /\bYou\b/, 'no You label on the user bubble');
assert.match(page, /<time class="chat-time" datetime="[^"]+">[^<]+<\/time>/, 'user message carries a hover time');
blocks = play([
  { type: 'tool_call', id: 't1', name: 'bash', args: '{"command":"rg -n isSupportedFile src"}', ts: T + 1400 },
  { type: 'tool_output', id: 't1', text: 'lib/files.ts:14', ts: T + 1500 },
], blocks);
page = html(blocks);
assert.match(text(page), /bash rg -n isSupportedFile src/, 'running label shows the latest tool and its argument');
assert.equal((page.match(/data-state="run"/g) || []).length >= 1, true);
assert.match(text(page), /Find the composer file first\./, 'the thinking step shows its first line');
assert.doesNotMatch(text(page), /Worked for/, 'no summary while the group runs');

// A failed tool and an intermediate reply end the group; the next group starts fresh.
blocks = play([
  { type: 'tool_output', id: 't1', text: 'rg: exit 2: bad pattern', is_error: true, done: true, ts: T + 4800 },
  { type: 'delta', text: 'The search failed, I will retry with a fixed pattern.', ts: T + 5000 },
  { type: 'tool_call', id: 't2', name: 'bash', args: '{"command":"rg -n isSupportedFile"}', ts: T + 5200 },
  { type: 'tool_output', id: 't2', text: 'lib/files.ts:14', done: true, app: { recordId: 'rec-1' }, ts: T + 5400 },
  { type: 'tool_call', id: 't3', name: 'edit', args: '{"path":"components/ChatView.tsx"}', ts: T + 5600 },
  { type: 'tool_output', id: 't3', text: 'Edited 1 file', done: true, ts: T + 5900 },
  { type: 'delta', text: 'Paste now works.', ts: T + 10_000 },
  { type: 'finished', ts: T + 10_100 },
], blocks);
page = html(blocks);
let flat = text(page);
assert.match(flat, /Worked for 4\.6s · 1 tool call · 1 failed/, 'finished group summarizes duration, singular count and failures');
assert.match(flat, /Worked for 4\.8s · 2 tool calls/, 'second group counts both tools');
assert.doesNotMatch(flat, /2 tool calls · 0 failed/);
assert.equal(page.includes('data-state="run"'), false, 'nothing spins once the turn finished');
assert.ok(flat.indexOf('1 tool call · 1 failed') < flat.indexOf('The search failed') && flat.indexOf('The search failed') < flat.indexOf('2 tool calls') && flat.indexOf('2 tool calls') < flat.indexOf('Paste now works.'), 'order: activity, reply, activity, final reply');
assert.match(flat, /Failed 3\.4s/, 'failed step meta shows Failed and its duration');
assert.match(page, /<pre class="chat-step-out is-error">rg: exit 2: bad pattern<\/pre>/, 'error output is rendered in the error style');
assert.match(page, /Input.*rg -n isSupportedFile src.*Output/s, 'tool step opens to Input and Output');
assert.match(page, /Thought.*Find the composer file first\.\nThen edit it\./s, 'thinking step opens to the full thought');
// The anchor must not sit inside a folded <details>: every details opened
// before it is already closed, so the app view stays visible.
const anchorAt = page.indexOf('data-app-anchor="rec-1"'), before = page.slice(0, anchorAt);
assert.ok(anchorAt > 0 && (before.match(/<details/g) || []).length === (before.match(/<\/details>/g) || []).length, 'app anchor is hoisted out of the collapsed history');
assert.ok(anchorAt < page.indexOf('Paste now works.'), 'anchor precedes the final reply');
assert.equal((page.match(/data-app-anchor=/g) || []).length, 1);

// Aborted turn with a tool that never finished: the group still ends.
let aborted = play([
  { type: 'user', text: 'go', ts: T },
  { type: 'tool_call', id: 'a', name: 'bash', args: '{"command":"sleep 100"}', ts: T + 100 },
  { type: 'aborted', error: 'aborted', ts: T + 2100 },
]);
page = html(aborted);
assert.equal(page.includes('data-state="run"'), false, 'an aborted turn stops the spinner');
assert.match(text(page), /Worked for 2\.0s · 1 tool call/);
assert.match(text(page), /Interrupted/);

// A question ends the group and renders as a question block.
let asked = play([
  { type: 'tool_call', id: 'q0', name: 'bash', args: '{}', ts: T },
  { type: 'tool_output', id: 'q0', text: 'ok', done: true, ts: T + 500 },
  { type: 'question', id: 'q1', questions: [{ header: 'Pick', question: 'Which?', options: [{ label: 'A' }] }], ts: T + 900 },
]);
page = html(asked);
assert.match(text(page), /Worked for 0\.9s · 1 tool call.*Pick Which\? A/s);
assert.equal(page.includes('data-state="run"'), false);

// Replay of an old session (historical events) yields the same structure.
let replay = play([
  { type: 'user', text: 'old', historical: true, ts: T },
  { type: 'tool_call', id: 'h', name: 'read', args: '{"path":"x"}', historical: true, ts: T + 100 },
  { type: 'tool_output', id: 'h', text: 'content', done: true, historical: true, ts: T + 700 },
  { type: 'delta', text: 'done', historical: true, ts: T + 800 },
  { type: 'finished', historical: true, ts: T + 900 },
]);
assert.match(text(html(replay)), /old.*Worked for 0\.7s · 1 tool call read x.*content.*done/);

// Agent replies are plain text with a hover time; user bubbles have no header row.
let plain = play([{ type: 'user', text: 'hey', ts: T }, { type: 'delta', text: 'hello', ts: T + 10 }]);
page = html(plain);
assert.doesNotMatch(page, /chat-role/);
assert.equal((page.match(/<time class="chat-time"/g) || []).length, 2);
console.log('activity ok');
