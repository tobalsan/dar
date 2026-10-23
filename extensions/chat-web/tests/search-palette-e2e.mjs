// Public seam approved by ALG-542 Testing Decisions: real dashboard, Pi wire,
// archive files, and browser interactions. Catches integration/layout/focus;
// misses exhaustive archive formats, covered by Rust matcher/route tests.
// DAR_SEARCH_TEST_BINARY, PLAYWRIGHT_MODULE and SEARCH_EVIDENCE_DIR are optional.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = path.resolve(process.env.SEARCH_EVIDENCE_DIR || '../validation/search-e2e');
await fs.mkdir(evidence, { recursive: true });
const agent = await fs.mkdtemp(path.join(evidence, 'agent-'));
const binary = path.resolve(process.env.DAR_SEARCH_TEST_BINARY || 'target/release/dar');
const socket = net.createServer();
await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
const port = socket.address().port;
await new Promise(resolve => socket.close(resolve));
const origin = `http://127.0.0.1:${port}`;
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: search-acceptance
name: Search acceptance
runner:
  use: fake
  model: claude-opus-4-8
foreground: logs
dashboard:
  bind: 127.0.0.1
  port: ${port}
extensions:
  chat-web:
    backend: pi
    command: ${path.join(here, 'fixtures/search-pi.cjs')}
    idle_minutes: 0
`);
const sessions = path.join(agent, 'data/chat/sessions');
await fs.mkdir(sessions, { recursive: true });
async function seed(id, date, messages, modified) {
  const file = path.join(sessions, `${date}_${id}.jsonl`);
  const records = [{ type: 'session', version: 3, id, backend: 'pi' }, ...messages.map(([role, text]) => ({ type: 'message', message: { role, content: [{ type: 'text', text }] } }))];
  await fs.writeFile(file, records.map(record => JSON.stringify(record)).join('\n') + '\n');
  await fs.utimes(file, modified, modified);
}
for (let i = 0; i < 60; i++) {
  await seed(`recent-${i}`, `2026-10-${String(1 + Math.floor(i / 3)).padStart(2, '0')}T00-00-${String(i % 3).padStart(2, '0')}Z`, [['user', `Recent conversation ${i}`], ['assistant', 'Ordinary response']], new Date(2026, 9, 1, 0, i));
}
await seed('deep-old', '2025-01-01T00-00-00Z', [['user', 'Legacy migration notes'], ['user', 'Remember meridian'], ['assistant', 'unrelated prose '.repeat(18000) + 'PgBouncer pooling resolves the connection issue.']], new Date(2026, 9, 22));
await seed('title-old', '2025-02-01T00-00-00Z', [['user', 'Unrelated opening question'], ['assistant', 'Unrelated response']], new Date(2026, 9, 1));
await fs.writeFile(path.join(agent, 'data/chat/sessions-meta.json'), JSON.stringify({
  'deep-old': { title: 'Legacy migration notes', archived: true },
  'title-old': { title: 'Meridian PgBouncer pooling' },
}));
let logs = '';
const host = spawn(binary, ['run', '--dir', agent], { stdio: ['ignore', 'pipe', 'pipe'] });
host.stdout.on('data', chunk => { logs += chunk; });
host.stderr.on('data', chunk => { logs += chunk; });
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: 'en-US', colorScheme: 'dark' });
page.setDefaultTimeout(10000);
const errors = [], results = [], abortRequests = [];
page.on('pageerror', error => errors.push(error.message));
page.on('request', request => { if (request.url().endsWith('/abort')) abortRequests.push(request.url()); });
const pass = name => { results.push({ name, result: 'PASS' }); console.log(`PASS ${name}`); };
const palette = () => page.getByRole('dialog', { name: 'Search conversations', exact: true });
const search = () => page.getByRole('combobox', { name: 'Search titles and messages', exact: true });
const header = () => page.getByLabel('Current conversation', { exact: true });
const message = () => page.getByRole('combobox', { name: 'Message', exact: true });
async function titleIs(text) {
  await header().filter({ hasText: text }).waitFor();
  assert.equal(await header().innerText(), text);
}
async function open(query) {
  await page.getByRole('button', { name: 'Search conversations', exact: true }).click();
  assert.ok(await search().evaluate(element => element === document.activeElement));
  if (query) await search().fill(query);
}
async function reply(text) {
  await message().fill(text);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.getByRole('log').getByText(`Fixture reply: ${text}`, { exact: true }).waitFor();
}
try {
  const deadline = Date.now() + 20000;
  while (true) {
    try { if ((await fetch(`${origin}/chat`)).ok) break; } catch {}
    if (host.exitCode !== null || Date.now() > deadline) throw new Error(`Dashboard startup failed: ${logs}`);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  const plain = await fetch(`${origin}/chat/sessions`).then(response => response.json());
  assert.equal(plain.length, 50);
  assert.ok(!plain.some(session => session.id === 'deep-old' || session.id === 'title-old'));
  const matches = await fetch(`${origin}/chat/sessions?q=PGBOUNCER%20pooling%20meridian`).then(response => response.json());
  assert.deepEqual(matches.map(session => [session.id, session.match, session.archived]), [['title-old', 'title', false], ['deep-old', 'content', true]]);
  assert.ok(matches[1].snippet.includes('PgBouncer'));
  pass('real HTTP finds deep cross-message archived match beyond plain cap, title group first');
  await page.goto(origin);
  await titleIs('New chat');
  assert.equal(await page.getByRole('button', { name: /tokens/ }).count(), 0);
  await message().fill('/compact keep my draft');
  await message().press('Control+k');
  await palette().waitFor();
  assert.equal(await palette().getByRole('option').count(), 0);
  assert.ok(await search().evaluate(element => element === document.activeElement));
  await search().press('Tab');
  assert.ok(await palette().evaluate(element => element.contains(document.activeElement)));
  await search().press('Escape');
  assert.equal(await message().inputValue(), '/compact keep my draft');
  assert.ok(await message().evaluate(element => element === document.activeElement));
  pass('Ctrl+K opens focused input-only palette; Tab stays inside; Escape restores draft and focus');
  await message().fill('/');
  await page.getByRole('listbox', { name: 'Commands', exact: true }).waitFor();
  await message().press('Meta+k');
  await palette().waitFor();
  await search().press('Escape');
  assert.equal(await message().inputValue(), '/');
  assert.ok(await page.getByRole('listbox', { name: 'Commands', exact: true }).isVisible());
  assert.deepEqual(abortRequests, []);
  pass('Cmd+K and Escape retain the composer slash menu without aborting a turn');
  await open('pgbouncer pooling meridian');
  await palette().getByRole('option').filter({ hasText: 'Legacy migration notes' }).waitFor();
  assert.deepEqual(await palette().getByRole('mark').allTextContents(), ['Meridian', 'PgBouncer', 'pooling', 'PgBouncer', 'pooling']);
  assert.deepEqual(await palette().getByRole('option').allTextContents().then(rows => rows.map(row => row.includes('Archived'))), [false, true]);
  await page.screenshot({ path: path.join(evidence, '01-palette-dark.png'), fullPage: true, animations: 'disabled' });
  await search().press('ArrowDown');
  await search().press('Enter');
  await palette().waitFor({ state: 'hidden' });
  await titleIs('Legacy migration notes');
  await page.getByRole('log').getByText('Remember meridian', { exact: true }).waitFor();
  await page.reload();
  await titleIs('Legacy migration notes');
  pass('keyboard opens archived old chat; header fallback survives reload outside list');
  await page.getByRole('button', { name: 'Toggle light and dark theme' }).click();
  await open('pgbouncer pooling meridian');
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).waitFor();
  await page.screenshot({ path: path.join(evidence, '02-palette-light.png'), fullPage: true, animations: 'disabled' });
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).click();
  await palette().waitFor({ state: 'hidden' });
  await titleIs('Meridian PgBouncer pooling');
  pass('click opens title match and updates header');
  await page.getByRole('button', { name: 'Toggle sidebar', exact: true }).click();
  await open();
  await page.screenshot({ path: path.join(evidence, '03-collapsed-rail-light.png'), fullPage: true, animations: 'disabled' });
  await page.mouse.click(5, 5);
  await palette().waitFor({ state: 'hidden' });
  pass('collapsed rail search icon stays operable; outside click closes');
  await page.getByRole('button', { name: 'New chat', exact: true }).click();
  await titleIs('New chat');
  await reply('usage 83632');
  const ring = page.getByRole('button', { name: '83,632 / 200,000 tokens · 42%', exact: true });
  await ring.waitFor();
  await ring.hover();
  assert.equal(await page.getByRole('tooltip').filter({ hasText: '83,632 / 200,000 tokens · 42%' }).isVisible(), true);
  await page.screenshot({ path: path.join(evidence, '04-context-light.png'), fullPage: true, animations: 'disabled' });
  await page.getByRole('button', { name: 'Toggle light and dark theme' }).click();
  await ring.focus();
  assert.equal(await page.getByRole('tooltip').filter({ hasText: '83,632 / 200,000 tokens · 42%' }).isVisible(), true);
  await page.screenshot({ path: path.join(evidence, '05-context-dark.png'), fullPage: true, animations: 'disabled' });
  await header().filter({ hasText: 'Fixture conversation title' }).waitFor({ timeout: 12000 });
  pass('Pi usage renders ring with instant hover/focus tooltip in both themes; automatic header title arrives');
  await page.getByRole('button', { name: 'Toggle sidebar', exact: true }).click();
  const current = page.getByRole('button', { name: /^Fixture conversation title/ });
  await current.hover();
  await current.getByRole('button', { name: 'Rename', exact: true }).click();
  await page.getByRole('textbox').fill('Renamed fixture conversation');
  await page.getByRole('textbox').press('Enter');
  await header().filter({ hasText: 'Renamed fixture conversation' }).waitFor();
  pass('local rename updates header through the real session metadata path');
  await reply('usage 140000');
  await page.getByText('Context is filling up — send /compact to summarize.', { exact: true }).waitFor();
  await page.getByRole('button', { name: '140,000 / 200,000 tokens · 70%', exact: true }).waitFor();
  await page.getByRole('button', { name: 'New chat', exact: true }).click();
  await titleIs('New chat');
  assert.equal(await page.getByRole('button', { name: /tokens/ }).count(), 0);
  pass('70% retains compact warning; new chat clears context ring');
  let release, entered, fulfilled;
  const gate = new Promise(resolve => { release = resolve; });
  const inFlight = new Promise(resolve => { entered = resolve; });
  const completed = new Promise(resolve => { fulfilled = resolve; });
  await page.route('**/chat/sessions?q=held', async route => { entered(); await gate; await route.fulfill({ json: [] }); fulfilled(); });
  await open('held');
  await palette().getByText(/Searching/).waitFor();
  await inFlight;
  await search().press('Escape');
  await reply('chat progresses while search hangs');
  release();
  await completed;
  await page.unroute('**/chat/sessions?q=held');
  await page.route('**/chat/sessions?q=broken', route => route.fulfill({ status: 503, body: 'Fixture unavailable' }));
  await open('broken');
  await palette().getByText(/Search.*(unavailable|failed)|try again/i).waitFor();
  await page.screenshot({ path: path.join(evidence, '06-search-error.png'), fullPage: true, animations: 'disabled' });
  await search().press('Escape');
  await reply('chat progresses after search failure');
  await page.unroute('**/chat/sessions?q=broken');
  await open('zz-no-matches');
  await palette().getByText('No matches', { exact: true }).waitFor();
  await search().press('Escape');
  pass('loading, no matches, failure, and hung-search dismissal leave chat progressing');
  let releaseOld, oldEntered, oldFulfilled;
  const oldGate = new Promise(resolve => { releaseOld = resolve; });
  const oldInFlight = new Promise(resolve => { oldEntered = resolve; });
  const oldCompleted = new Promise(resolve => { oldFulfilled = resolve; });
  await page.route('**/chat/sessions?q=old-response', async route => {
    oldEntered(); await oldGate;
    await route.fulfill({ json: [{ id: 'stale', label: 'Stale overwritten result', modified_ms: Date.now(), archived: false }] });
    oldFulfilled();
  });
  await open('old-response'); await oldInFlight;
  await search().fill('pgbouncer');
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).waitFor();
  releaseOld(); await oldCompleted;
  await page.unroute('**/chat/sessions?q=old-response');
  assert.equal(await palette().getByRole('option').filter({ hasText: 'Stale overwritten result' }).count(), 0);
  assert.ok(await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).isVisible());
  await search().press('Escape');
  pass('an old in-flight response cannot replace newer search results');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('button', { name: 'Open conversations', exact: true }).click();
  await open('pgbouncer');
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).waitFor();
  await page.screenshot({ path: path.join(evidence, '07-mobile-dark.png'), fullPage: true, animations: 'disabled' });
  await search().press('Escape');
  await page.getByRole('button', { name: 'Toggle light and dark theme' }).click();
  await open('pgbouncer');
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).waitFor();
  await page.screenshot({ path: path.join(evidence, '08-mobile-light.png'), fullPage: true, animations: 'disabled' });
  pass('mobile drawer search works in light and dark themes');
  await palette().getByRole('option').filter({ hasText: 'Meridian PgBouncer pooling' }).click();
  await titleIs('Meridian PgBouncer pooling');
  await message().click({ timeout: 1500 });
  assert.equal(await page.getByRole('button', { name: /tokens/ }).count(), 0);
  pass('mobile result reveals the resumed chat and clears the prior context ring');
  await reply('mobile usage 83632');
  await page.getByRole('button', { name: '83,632 / 200,000 tokens · 42%', exact: true }).click();
  assert.ok(await page.getByRole('tooltip').filter({ hasText: '83,632 / 200,000 tokens · 42%' }).isVisible());
  pass('context tooltip opens by tap on a narrow screen');
  assert.deepEqual(abortRequests, []);
  assert.deepEqual(errors, []);
} finally {
  await fs.writeFile(path.join(evidence, 'host.log'), logs);
  await fs.writeFile(path.join(evidence, 'results.json'), JSON.stringify({ agent, results, errors }, null, 2));
  await browser.close();
  host.kill('SIGTERM');
  await new Promise(resolve => { if (host.exitCode !== null) resolve(); else host.once('exit', resolve); });
}
