// Activity lines: consecutive tool/thinking steps fold into one line whose
// label, order and reachable details are asserted as the user reads them.
const assert = require('node:assert/strict');
const { reduce, html } = require(process.argv[2]);

// What a reader (or a screen reader) gets: visible text plus accessible names, e.g. "[Running]" for the spinner.
const text = fragment => fragment.replace(/(<[^>]*aria-label="([^"]*)"[^>]*>)/g, '$1 [$2] ').replace(/<[^>]*>/g, ' ').replace(/&quot;/g, '"').replace(/\s+/g, ' ').trim();
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
assert.match(page, /<time\b[^>]*datetime="[^"]+"[^>]*>\s*\d[^<]*<\/time>/, 'user message carries a time for hover');
blocks = play([
  { type: 'tool_call', id: 't1', name: 'bash', args: '{"command":"rg -n isSupportedFile src"}', ts: T + 1400 },
  { type: 'tool_output', id: 't1', text: 'lib/files.ts:14', ts: T + 1500 },
], blocks);
page = html(blocks);
assert.match(text(page), /bash rg -n isSupportedFile src/, 'running label shows the latest tool and its argument');
assert.match(text(page), /\[Running\]/, 'the running group shows a spinner');
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
assert.doesNotMatch(text(page), /\[Running\]/, 'nothing spins once the turn finished');
assert.ok(flat.indexOf('1 tool call · 1 failed') < flat.indexOf('The search failed') && flat.indexOf('The search failed') < flat.indexOf('2 tool calls') && flat.indexOf('2 tool calls') < flat.indexOf('Paste now works.'), 'order: activity, reply, activity, final reply');
assert.match(flat, /Failed 3\.4s/, 'failed step meta shows Failed and its duration');
assert.match(flat, /Output rg: exit 2: bad pattern/, 'the failed step shows its error output (its red color is checked in activity-e2e)');
assert.match(page, /Input.*rg -n isSupportedFile src.*Output/s, 'tool step opens to Input and Output');
assert.match(page, /Thought.*Find the composer file first\.\nThen edit it\./s, 'thinking step opens to the full thought');
// The MCP app anchor stays in the thread: right after the group that made it, before the final reply
// (that it is visible with the history folded is checked by mcp-apps-e2e).
const anchorAt = page.indexOf('data-app-anchor="rec-1"');
assert.ok(anchorAt > page.indexOf('Edited 1 file'), 'app anchor follows its group');
assert.ok(anchorAt < page.indexOf('Paste now works.'), 'anchor precedes the final reply');
assert.equal((page.match(/data-app-anchor=/g) || []).length, 1);

// Aborted turn with a tool that never finished: the group still ends.
let aborted = play([
  { type: 'user', text: 'go', ts: T },
  { type: 'tool_call', id: 'a', name: 'bash', args: '{"command":"sleep 100"}', ts: T + 100 },
  { type: 'aborted', error: 'aborted', ts: T + 2100 },
]);
page = html(aborted);
assert.doesNotMatch(text(page), /\[Running\]/, 'an aborted turn stops the spinner');
assert.match(text(page), /Worked for 2\.0s · 1 tool call/);
assert.match(text(page), /Worked for 2\.0s · 1 tool call.*bash sleep 100 Interrupted/s, 'an unfinished tool reads Interrupted, not done');

// A question ends the group and renders as a question block.
let asked = play([
  { type: 'tool_call', id: 'q0', name: 'bash', args: '{}', ts: T },
  { type: 'tool_output', id: 'q0', text: 'ok', done: true, ts: T + 500 },
  { type: 'question', id: 'q1', questions: [{ header: 'Pick', question: 'Which?', options: [{ label: 'A' }] }], ts: T + 900 },
]);
page = html(asked);
assert.match(text(page), /Worked for 0\.9s · 1 tool call.*Pick Which\? A/s);
assert.doesNotMatch(text(page), /\[Running\]/);

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
assert.doesNotMatch(text(page), /\bYou\b|\bAgent\b/, 'no name header on either side');
assert.equal((page.match(/<time\b/g) || []).length, 2, 'each message carries a hover time');
// Parallel tool calls: one still waiting for output keeps spinning (and says Running…) while another finished.
let parallel = play([
  { type: 'user', text: 'go', ts: T },
  { type: 'tool_call', id: 'p1', name: 'bash', args: '{"command":"slow"}', ts: T + 100 },
  { type: 'tool_call', id: 'p2', name: 'bash', args: '{"command":"fast"}', ts: T + 200 },
  { type: 'tool_output', id: 'p2', text: 'fast done', done: true, ts: T + 300 },
]);
page = text(html(parallel));
assert.match(page, /bash slow.*Output Running…/s, 'the unfinished parallel tool reads Running…');
assert.match(page, /\[Running\]/);

// A new turn never re-opens the previous group: its label and duration stay put.
let turns = play([
  { type: 'user', text: 'first', ts: T },
  { type: 'tool_call', id: 'a', name: 'bash', args: '{"command":"one"}', ts: T + 100 },
  { type: 'tool_output', id: 'a', text: 'ok', done: true, ts: T + 1100 },
  { type: 'silent', ts: T + 1200 },
  { type: 'started', origin: 'autonomous', ts: T + 9000 },
  { type: 'thinking', text: 'second turn thought', ts: T + 9100 },
  { type: 'tool_call', id: 'b', name: 'bash', args: '{"command":"two"}', ts: T + 9200 },
]);
page = text(html(turns));
assert.match(page, /Worked for 1\.1s · 1 tool call/, 'the first turn keeps its own summary');
assert.match(page, /\[Running\].*bash two/s, 'the second turn runs in its own group');
let silentThenThought = play([
  { type: 'thinking', text: 'old thought', ts: T },
  { type: 'silent', ts: T + 500 },
  { type: 'started', origin: 'autonomous', ts: T + 4000 },
  { type: 'thinking', text: 'new thought', ts: T + 4100 },
]);
page = text(html(silentThenThought));
assert.match(page, /Thought for 0\.5s.*\[Running\]/s, 'a new turn\'s thinking starts its own group');
assert.doesNotMatch(page, /old thought new thought/);

// A tool_output that arrives before its tool_call is one step, not two.
let early = play([
  { type: 'tool_output', id: 'e', text: 'early output', done: true, ts: T + 50 },
  { type: 'tool_call', id: 'e', name: 'bash', args: '{"command":"echo hi"}', ts: T },
  { type: 'delta', text: 'done', ts: T + 900 },
]);
page = text(html(early));
assert.match(page, /1 tool call(?!s)/, 'one tool call counted');
assert.match(page, /bash echo hi.*Output early output/s, 'the step carries the call and its output');

console.log('activity ok');
