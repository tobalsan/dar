// Public seam: real dashboard + Pi wire + MCP stdio + browser, approved in ALG-530
// Testing Decisions. Catches rendering/lifecycle/isolation; misses vendor OAuth.
// Run with DAR_APPS_TEST_BINARY, PLAYWRIGHT_MODULE and APPS_EVIDENCE_DIR set.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = path.resolve(process.env.APPS_EVIDENCE_DIR || 'validation/apps-e2e');
const externalOrigin = process.env.APPS_CHAT_ORIGIN;
const agent = path.resolve(process.env.APPS_AGENT_DIR || path.join(evidence, 'agent'));
await fs.mkdir(agent, { recursive: true });
await fs.mkdir(evidence, { recursive: true });
const binary = path.resolve(process.env.DAR_APPS_TEST_BINARY || 'target/release/dar');
async function freePort() {
  const listener = net.createServer();
  await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
  const port = listener.address().port;
  await new Promise(resolve => listener.close(resolve));
  return port;
}
const port = await freePort();
const origin = externalOrigin || `http://127.0.0.1:${port}`;
if (!externalOrigin) {
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: apps-acceptance\nname: Apps acceptance\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, 'fixtures/apps-pi.cjs')}\n    idle_minutes: 0\n    apps:\n      sandbox_port: 0\n`);
const fixtureEnv = process.env.APPS_SDK_HTML ? { DAR_APPS_TEST_HTML: path.resolve(process.env.APPS_SDK_HTML) } : {};
await fs.writeFile(path.join(agent, 'mcp.json'), JSON.stringify({ mcpServers: {
  canvas: { command: process.execPath, args: [path.join(here, 'fixtures/apps-server.cjs')], env: fixtureEnv },
  other: { command: process.execPath, args: [path.join(here, 'fixtures/apps-server.cjs')], env: fixtureEnv }
} }));
}
let logs = '';
const startHost = () => {
  const child = spawn(binary, ['run', '--dir', agent], { env: { ...process.env, DAR_APPS_TEST_BINARY: binary }, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.on('data', chunk => { logs += chunk; });
  child.stderr.on('data', chunk => { logs += chunk; });
  return child;
};
let host = externalOrigin ? null : startHost();
const stopHost = async () => {
  if (!host) return;
  host.kill('SIGTERM');
  await new Promise(resolve => { if (host.exitCode !== null) resolve(); else host.once('exit', resolve); });
};
const waitForHost = async () => {
  const deadline = Date.now() + 20_000;
  while (true) {
    try { if ((await fetch(`${origin}/chat`)).ok) return; } catch {}
    if ((host && host.exitCode !== null) || Date.now() > deadline) throw new Error(`Dashboard failed to start: ${logs}`);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
};
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const failures = [];
page.on('pageerror', error => failures.push(error.message));
try {
  await waitForHost();
  await page.goto(`${origin}/`);
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill('draw first');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  const deadlineView = Date.now() + 15_000;
  let drawing;
  while (!drawing && Date.now() < deadlineView) {
    for (const frame of page.frames()) {
      if (await frame.getByRole('heading', { name: 'Drawing: first', exact: true }).isVisible().catch(() => false)) drawing = frame;
    }
    if (!drawing) await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(drawing, `App drawing did not appear. Browser errors: ${failures.join(', ')}`);
  await page.screenshot({ path: path.join(evidence, '01-inline.png'), fullPage: true });
  console.log('PASS inline MCP App shows real bridge tool result');
  await drawing.getByText('Undeclared network blocked', { exact: true }).waitFor();
  assert.equal(await drawing.getByLabel('Policy violation', { exact: true }).textContent(), 'connect-src');
  assert.equal(await drawing.getByRole('status').textContent(), '');
  assert.ok(!(await page.getByRole('log').innerText()).includes('[mcp-app-result:'));
  console.log('PASS restrictive default CSP blocks undeclared connect-src; marker hidden');
  await drawing.getByRole('button', { name: 'Select box', exact: true }).click();
  await drawing.getByRole('status').filter({ hasText: 'Selection updated' }).waitFor();
  await drawing.getByRole('button', { name: 'Post selection', exact: true }).click();
  await drawing.getByRole('status').filter({ hasText: 'Message posted' }).waitFor();
  await page.getByRole('log').getByText('Please inspect the selected box', { exact: true }).waitFor();
  const prompts = await fs.readFile(path.join(agent, 'prompts.jsonl'), 'utf8');
  assert.ok(prompts.split('\n').filter(Boolean).map(line => JSON.parse(line).message)
    .some(message => message.includes('Please inspect the selected box') && message.includes('Selected box: blue rectangle')));
  console.log('PASS app button posts chat message with selected context on the runner wire');
  await drawing.getByRole('button', { name: 'Expand drawing', exact: true }).click();
  await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).waitFor();
  const panel = await page.getByRole('region', { name: 'MCP App: canvas', exact: true }).boundingBox();
  const composer = await page.getByRole('textbox', { name: 'Message', exact: true }).boundingBox();
  assert.ok(panel && composer && composer.x + composer.width <= panel.x + 1, 'Expanded view must sit beside the chat input');
  await page.screenshot({ path: path.join(evidence, '02-fullscreen.png'), fullPage: true });
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill('chat remains usable');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.getByRole('log').getByText('chat remains usable', { exact: true }).waitFor();
  await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).click();
  await page.getByRole('button', { name: 'Expand view: canvas', exact: true }).waitFor();
  console.log('PASS app-requested fullscreen sits beside usable chat; close returns inline');
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill('draw second');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await drawing.getByRole('heading', { name: 'Drawing: second', exact: true }).waitFor();
  assert.equal(await drawing.getByLabel('Results received', { exact: true }).textContent(), '2');
  assert.equal(await page.getByRole('region', { name: 'MCP App: canvas', exact: true }).count(), 1);
  await page.getByRole('button', { name: 'View updated: canvas', exact: true }).waitFor();
  await page.screenshot({ path: path.join(evidence, '03-reused.png'), fullPage: true });
  console.log('PASS second tool call updates existing view without reinitializing it');
  await page.reload();
  const deadlineReplay = Date.now() + 15_000;
  let restored;
  while (!restored && Date.now() < deadlineReplay) {
    for (const frame of page.frames()) {
      if (await frame.getByRole('heading', { name: 'Drawing: second', exact: true }).isVisible().catch(() => false)) restored = frame;
    }
    if (!restored) await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(restored, 'Reload must restore the latest drawing');
  assert.equal(await restored.getByLabel('Results received', { exact: true }).textContent(), '1');
  assert.equal(await page.getByRole('region', { name: 'MCP App: canvas', exact: true }).count(), 1);
  await page.screenshot({ path: path.join(evidence, '04-reloaded.png'), fullPage: true });
  console.log('PASS reload restores one view with only its latest input/result');
  await restored.getByRole('button', { name: 'Refresh drawing', exact: true }).click();
  await restored.getByRole('status').filter({ hasText: 'Drawing: refreshed' }).waitFor();
  await restored.getByRole('button', { name: 'Call another server', exact: true }).click();
  await restored.getByRole('status').filter({ hasText: 'Cross-server call blocked' }).waitFor();
  await restored.getByRole('button', { name: 'Call model-only tool', exact: true }).click();
  await restored.getByRole('status').filter({ hasText: 'Model-only call blocked' }).waitFor();
  console.log('PASS own app-only tool works; cross-server and model-only calls are blocked');
  if (process.env.APPS_LOADING_RACE === '1') {
    await page.getByRole('button', { name: 'New chat', exact: true }).click();
    await page.getByRole('region', { name: 'MCP App: canvas', exact: true }).waitFor({ state: 'detached' });
    let firstSeen, secondSeen, releaseFirst;
    const firstHeld = new Promise(resolve => { firstSeen = resolve; });
    const secondReturned = new Promise(resolve => { secondSeen = resolve; });
    const gate = new Promise(resolve => { releaseFirst = resolve; });
    let resources = 0;
    await page.route('**/chat/*/apps/*', async route => {
      if (route.request().method() !== 'GET') return route.continue();
      const response = await route.fetch();
      if (++resources === 1) { firstSeen(); await gate; }
      await route.fulfill({ response });
      if (resources === 2) secondSeen();
    });
    await page.getByRole('textbox', { name: 'Message', exact: true }).fill('draw first');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await firstHeld;
    await page.getByRole('textbox', { name: 'Message', exact: true }).fill('draw second');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await secondReturned;
    releaseFirst();
    let latest;
    const deadline = Date.now() + 5000;
    while (!latest && Date.now() < deadline) {
      for (const frame of page.frames()) {
        if (await frame.getByRole('heading', { name: 'Drawing: second', exact: true }).isVisible().catch(() => false)) latest = frame;
      }
      if (!latest) await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(latest, 'A delayed first resource must not overwrite the second call result');
    await page.unroute('**/chat/*/apps/*');
    console.log('PASS delayed initial resource cannot overwrite newer view data');
  }
  if (!externalOrigin) {
    await stopHost();
    host = startHost();
    await waitForHost();
    await page.reload();
    let restarted;
    const deadline = Date.now() + 15_000;
    while (!restarted && Date.now() < deadline) {
      for (const frame of page.frames()) {
        if (await frame.getByRole('heading', { name: 'Drawing: second', exact: true }).isVisible().catch(() => false)) restarted = frame;
      }
      if (!restarted) await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(restarted, 'Host restart must preserve the latest drawing');
    await page.screenshot({ path: path.join(evidence, '05-restarted.png'), fullPage: true });
    console.log('PASS host restart preserves transcript and restores latest app result');
  }
  assert.deepEqual(failures, [], 'Browser must have no uncaught JavaScript errors');
} finally {
  await fs.writeFile(path.join(evidence, 'host.log'), logs);
  await fs.writeFile(path.join(evidence, 'browser-errors.json'), JSON.stringify(failures));
  await browser.close();
  await stopHost();
}
const audit = await fs.readFile(path.join(agent, 'logs/agent.log'), 'utf8');
assert.ok(audit.includes('MCP App CSP') && audit.includes('connect-src'));
assert.ok(audit.includes('method=tools/call') && audit.includes('status="error"'));
console.log('PASS persistent audit records view CSP and rejected tool calls');
