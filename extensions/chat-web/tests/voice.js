// Voice replies: what gets read aloud, and in which pieces.
const assert = require('node:assert/strict');
const { speakable, splitChunks, lastReply } = require(process.argv[2]);

// Fenced code and tables are not read; inline code stays; a URL is read as its host.
const reply = [
  '# Result',
  'I fixed **the parser** and updated [the guide](https://example.com/guide).',
  '```rust',
  'fn main() { println!("hi"); }',
  '```',
  '- edited `src/lib.rs`',
  '- renamed `parse` to `read`',
  '1. run the tests',
  '| file | change |',
  '|------|--------|',
  '| a.rs | fixed |',
  'See https://example.com/docs for more',
].join('\n');
const spoken = speakable(reply);
assert.equal(spoken, 'Result. I fixed the parser and updated the guide. edited src/lib.rs. renamed parse to read. run the tests. See example.com for more.');
for (const noise of ['```', 'fn main', 'a.rs', '|', 'https', '**', '`', '#']) assert.ok(!spoken.includes(noise), `not spoken: ${noise}`);
assert.equal(speakable('```\nunclosed code'), '', 'an unclosed fence is dropped');
assert.equal(speakable(''), '');
assert.equal(speakable(undefined), '');

// Sentences pack into chunks of at most 250 characters, in order.
const sentence = n => `Sentence number ${n} is a short one.`;
const text = Array.from({ length: 20 }, (_, i) => sentence(i)).join(' ');
const chunks = splitChunks(text);
assert.ok(chunks.length > 1);
assert.ok(chunks.every(c => c.length <= 250), 'no chunk is over 250 characters');
assert.equal(chunks.join(' '), text, 'nothing lost, order kept');
assert.equal(chunks[0], sentence(0), 'the first chunk is the first sentence');
chunks.slice(1).forEach((c, i) => assert.ok(c.length <= Math.min(250, Math.max(80, 2 * chunks[i].length)), `chunk ${i + 1} is at most twice the one before`));
assert.ok(chunks[1].length > chunks[0].length, 'chunks grow');
assert.ok(chunks.slice(0, -1).every(c => /[.!?]$/.test(c)), 'chunks end on sentence boundaries');
assert.deepEqual(splitChunks('Hello there.'), ['Hello there.'], 'one sentence is one chunk');
assert.deepEqual(splitChunks('Hello there. How are you?'), ['Hello there.', 'How are you?']);
const mid = 'Your first meeting starts at ten, and the client report is ready in the reports folder today.';
assert.deepEqual(splitChunks(`Hi. Good morning. ${mid} Done.`), ['Hi.', 'Good morning.', mid, 'Done.'], 'a sentence over the growth limit stays whole');
assert.equal(speakable('the [client report](https://example.com/r) is ready in `reports/w45.html`'), 'the client report is ready in reports/w45.html.');
assert.equal(speakable('Open https://github.com/x/y now'), 'Open github.com now.');
assert.deepEqual(splitChunks(''), []);
const long = splitChunks('word '.repeat(200).trim());
assert.ok(long.length > 1 && long.every(c => c.length <= 250), 'a sentence with no stop is cut at spaces');
assert.equal(splitChunks('x'.repeat(600)).map(c => c.length).join(), '250,250,100', 'an unbroken run is cut at the limit');

// Only the last assistant message of the latest turn is spoken.
const blocks = [
  { kind: 'user', text: 'old question' },
  { kind: 'assistant', text: 'old answer' },
  { kind: 'user', text: 'new question' },
  { kind: 'assistant', text: 'Let me check.' },
  { kind: 'tool', id: 't1' },
  { kind: 'assistant', text: 'Final answer.' },
];
assert.equal(lastReply(blocks).text, 'Final answer.');
assert.equal(lastReply(blocks.slice(0, 3)), null, 'a turn with no reply speaks nothing');
// Tables are skipped, with or without outer pipes.
assert.equal(speakable('Before.\n\nName | Status\n--- | ---\nparser | fixed\nlexer | open\n\nAfter.'), 'Before. After.');
assert.equal(speakable('| a | b |\n|---|---|\n| 1 | 2 |\nDone'), 'Done.');
assert.equal(speakable('Use a | b in a pipeline.'), 'Use a | b in a pipeline.', 'a lone pipe is not a table');

// Through the renderer: an autonomous reply is its own turn, not appended to the last one.
const { reduce } = require(require('node:path').join(__dirname, '../src/renderer.js'));
const play = events => events.reduce((acc, event) => reduce(acc, event), []);
const turns = play([
  { type: 'user', text: 'q', ts: 1 }, { type: 'delta', text: 'First answer.', ts: 2 }, { type: 'finished', ts: 3 },
  { type: 'started', origin: 'autonomous', ts: 4 }, { type: 'delta', text: 'Background update.', ts: 5 }, { type: 'finished', ts: 6 },
]);
assert.equal(lastReply(turns).text, 'Background update.', 'only the autonomous turn is spoken');
assert.equal(lastReply(turns.slice(0, 2)).text, 'First answer.', 'the first turn still speaks its own reply');
const toolEnd = play([
  { type: 'user', text: 'q', ts: 1 }, { type: 'delta', text: 'Done.', ts: 2 }, { type: 'tool_call', id: 't', name: 'bash', ts: 3 },
  { type: 'tool_output', id: 't', text: 'ok', done: true, ts: 4 }, { type: 'finished', ts: 5 },
  { type: 'started', origin: 'autonomous', ts: 6 }, { type: 'finished', ts: 7 },
]);
assert.equal(lastReply(toolEnd), null, 'an autonomous turn with no text does not repeat the previous reply');
const abortedTurn = play([
  { type: 'user', text: 'q', ts: 1 }, { type: 'delta', text: 'Half an answer', ts: 2 }, { type: 'aborted', error: 'aborted', ts: 3 },
  { type: 'started', origin: 'autonomous', ts: 4 }, { type: 'finished', ts: 5 },
]);
assert.equal(lastReply(abortedTurn), null, 'an aborted answer is not spoken for the next empty turn');
console.log('voice ok');
