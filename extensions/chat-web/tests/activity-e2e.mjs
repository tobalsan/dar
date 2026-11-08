// Activity-line e2e in the real chat page: what a reader sees and can click.
// Tool/thinking groups fold into one line, open to Input / Output wells, show
// failures in the danger color, keep replies in order, and the composer's
// context ring reads as a usage gauge with a hover tooltip.
// DAR_APPS_TEST_BINARY, PLAYWRIGHT_MODULE and PLAYWRIGHT_CHANNEL (e.g. chrome) are optional.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const playwright = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const chromium = playwright.chromium || playwright.default.chromium;
const here = path.dirname(fileURLToPath(import.meta.url));
const binary = path.resolve(process.env.DAR_APPS_TEST_BINARY || 'target/release/dar');
const agent = await fs.mkdtemp(path.join(os.tmpdir(), 'dar-activity-e2e-'));
const listener = net.createServer();
await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
const port = listener.address().port;
await new Promise(resolve => listener.close(resolve));
const origin = `http://127.0.0.1:${port}`;
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: activity-e2e\nname: Iris\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, 'fixtures/apps-pi.cjs')}\n    idle_minutes: 0\n`);
const host = spawn(binary, ['run', '--dir', agent], { stdio: 'ignore', detached: true });
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const pass = message => console.log(`PASS ${message}`);
let browser;
try {
  for (const deadline = Date.now() + 20_000; ; await wait(100)) {
    try { if ((await fetch(`${origin}/chat`)).ok) break; } catch {}
    if (Date.now() > deadline) throw new Error('Dashboard failed to start');
  }
  browser = await chromium.launch({ headless: true, channel: process.env.PLAYWRIGHT_CHANNEL });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const failures = [];
  page.on('pageerror', error => failures.push(error.message));
  await page.goto(`${origin}/`);
  const log = page.getByRole('log');
  const event = e => page.evaluate(e => window.renderChatEvent(e), e);
  const T = Date.now() - 60_000;
  const color = locator => locator.evaluate(node => getComputedStyle(node).color);

  await event({ type: 'user', text: 'Add paste support, then run the tests.', ts: T });
  await event({ type: 'thinking', text: 'Paste should reuse the drag and drop check.', ts: T + 500 });
  await event({ type: 'tool_call', id: 't1', name: 'bash', args: '{"command":"rg -n isSupportedFile src"}', ts: T + 1500 });
  // Running: a spinner and the latest action, no history yet.
  await log.getByRole('img', { name: 'Running' }).waitFor();
  await log.getByText('rg -n isSupportedFile src').first().waitFor();
  assert.equal(await log.getByText('Input', { exact: true }).first().isVisible(), false, 'steps stay folded while running');
  pass('a running group shows a spinner and the latest tool with its argument');

  await event({ type: 'tool_output', id: 't1', text: 'lib/files.ts:14', done: true, ts: T + 2400 });
  await event({ type: 'delta', text: 'The check lives in lib/files.ts. I run the tests next.', ts: T + 3000 });
  await event({ type: 'tool_call', id: 't2', name: 'bash', args: '{"command":"pnpm typecheck"}', ts: T + 3500 });
  await event({ type: 'tool_output', id: 't2', text: "Type 'FileList' is not assignable to type 'File[]'.", is_error: true, done: true, ts: T + 8000 });
  await event({ type: 'tool_call', id: 't3', name: 'bash', args: '{"command":"pnpm vitest run"}', ts: T + 8500 });
  await event({ type: 'tool_output', id: 't3', text: 'Tests 1154 passed', done: true, ts: T + 20_000 });
  await event({ type: 'tool_call', id: 't4', name: 'bash', args: JSON.stringify({ command: 'cd ~/code/project && git status --short; git branch --show-current; '.repeat(6) }), ts: T + 20_100 });
  await event({ type: 'tool_output', id: 't4', text: 'main', done: true, ts: T + 20_300 });
  await event({ type: 'delta', text: 'All 1154 tests pass.', ts: T + 21_000 });
  await event({ type: 'finished', ts: T + 21_100 });
  const first = log.getByText(/Worked for .* · 1 tool call$/), second = log.getByText(/Worked for .* · 3 tool calls · 1 failed/);
  await first.waitFor(); await second.waitFor();
  assert.equal(await log.getByRole('img', { name: 'Running' }).count(), 0, 'nothing spins after the turn');
  assert.ok(await log.getByText(/\bYou\b/).count() === 0, 'no "You" label');
  // Order in the thread: activity line, reply, activity line, final reply.
  const y = async locator => (await locator.boundingBox()).y;
  const ys = [await y(first), await y(log.getByText('The check lives in lib/files.ts.')), await y(second), await y(log.getByText('All 1154 tests pass.'))];
  assert.deepEqual(ys, [...ys].sort((a, b) => a - b), 'activity, reply, activity, final reply in order');
  assert.equal(await log.getByText('Input', { exact: true }).first().isVisible(), false, 'history is folded after the group ends');
  pass('finished groups read "Worked for … · N tool calls" and replies sit between them in order');

  // Open the failing group: steps, Failed meta, Input / Output wells, red error output.
  await second.click();
  // A long argument summary never wraps the tool name: each "bash" label keeps one line height.
  const names = log.getByText('bash', { exact: true }).filter({ visible: true });
  const heights = await names.evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().height));
  assert.ok(heights.length >= 3 && heights.every(h => h === heights[0]), `tool names keep one line: ${heights}`);
  const typecheck = log.getByText('pnpm typecheck', { exact: true });
  await typecheck.click();
  await log.getByText('Failed', { exact: true }).waitFor();
  await log.getByText('Input', { exact: true }).filter({ visible: true }).waitFor();
  const error = log.getByText("Type 'FileList' is not assignable to type 'File[]'.");
  await error.waitFor();
  await log.getByText('pnpm vitest run', { exact: true }).click();
  const fine = log.getByText('Tests 1154 passed');
  await fine.waitFor();
  assert.notEqual(await color(error), await color(fine), 'error output is in a different (danger) color than normal output');
  pass('a step opens to Input / Output; the failed step says Failed and shows its error in the danger color');

  // Time shows on hover only.
  const stamp = log.locator('time').first();
  const user = log.getByText('Add paste support, then run the tests.');
  assert.equal(await stamp.evaluate(node => getComputedStyle(node).opacity), '0', 'time is hidden at rest');
  await user.hover();
  await page.waitForFunction(() => getComputedStyle(document.querySelector('#chat-transcript time')).opacity === '1');
  pass('the message time appears on hover');

  // Keyboard: Tab reaches the activity line, its chevron shows on focus, Enter opens it.
  await page.evaluate(() => document.activeElement?.blur());
  const summary = first.locator('xpath=ancestor::summary');
  await page.keyboard.press('Tab');
  for (let i = 0; i < 20 && !(await summary.evaluate(node => node === document.activeElement)); i++) await page.keyboard.press('Tab');
  assert.ok(await summary.evaluate(node => node === document.activeElement), 'the activity line takes keyboard focus');
  await wait(300); // chevron fades in
  assert.equal(await summary.locator('svg').last().evaluate(node => getComputedStyle(node).opacity), '1', 'the chevron shows on keyboard focus');
  assert.equal(await log.getByText('rg -n isSupportedFile src').filter({ visible: true }).count(), 0, 'closed before Enter');
  await page.keyboard.press('Enter');
  await log.getByText('rg -n isSupportedFile src').filter({ visible: true }).first().waitFor();
  pass('the activity line is keyboard reachable, shows its chevron on focus, and opens with Enter');

  // Context ring: percentage inside, tooltip with usage on hover.
  await event({ type: 'context_usage', tokens_used: 130_000, context_window: 200_000 });
  const ring = page.getByRole('button', { name: '130,000 / 200,000 tokens · 65%' });
  await ring.waitFor();
  assert.equal((await ring.innerText()).trim(), '65%', 'the ring shows its percentage');
  await ring.hover();
  await page.getByRole('tooltip').filter({ hasText: '130,000 / 200,000 tokens · 65%' }).waitFor();
  pass('the context ring reads as a gauge (65%) and keeps its usage tooltip');
  assert.deepEqual(failures, [], 'No uncaught browser errors');
} finally {
  await browser?.close();
  try { process.kill(-host.pid, 'SIGTERM'); } catch {}
  await fs.rm(agent, { recursive: true, force: true });
}
