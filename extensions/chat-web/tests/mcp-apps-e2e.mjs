// Public seam: real dashboard + Pi wire + MCP stdio + browser, approved in ALG-530
// Testing Decisions. Catches rendering/lifecycle/isolation; misses vendor OAuth.
// Run with DAR_APPS_TEST_BINARY, PLAYWRIGHT_MODULE and APPS_EVIDENCE_DIR set.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const playwright = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const chromium = playwright.chromium || playwright.default.chromium;
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = path.resolve(process.env.APPS_EVIDENCE_DIR || 'validation/apps-e2e');
const externalOrigin = process.env.APPS_CHAT_ORIGIN;
const artifacts = process.env.APPS_ARTIFACTS === '1';
const agent = path.resolve(process.env.APPS_AGENT_DIR || path.join(evidence, `agent-${process.pid}`));
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
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: apps-acceptance\nname: Apps acceptance\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, artifacts ? 'fixtures/artifacts-pi.cjs' : 'fixtures/apps-pi.cjs')}\n    idle_minutes: 0\n    apps:\n      sandbox_port: 0\n`);
const fixtureEnv = process.env.APPS_SDK_HTML ? { DAR_APPS_TEST_HTML: path.resolve(process.env.APPS_SDK_HTML) } : {};
if (process.env.APPS_POLICY) fixtureEnv.DAR_APPS_TEST_POLICY = process.env.APPS_POLICY;
if (process.env.APPS_PROTOCOL_VERSION) fixtureEnv.DAR_APPS_TEST_PROTOCOL_VERSION = process.env.APPS_PROTOCOL_VERSION;
if (!artifacts) await fs.writeFile(path.join(agent, 'mcp.json'), JSON.stringify({ mcpServers: {
  canvas: { command: process.execPath, args: [path.join(here, 'fixtures/apps-server.cjs')], env: fixtureEnv },
  other: { command: process.execPath, args: [path.join(here, 'fixtures/apps-server.cjs')], env: fixtureEnv }
} }));
}
let logs = '';
const startHost = () => {
  const child = spawn(binary, ['run', '--dir', agent], { env: { ...process.env, DAR_APPS_TEST_BINARY: binary, ...(artifacts ? { DAR_ARTIFACT_ROOT: path.join(evidence, 'vault') } : {}) }, stdio: ['ignore', 'pipe', 'pipe'] });
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
  if (artifacts) {
    const { artifactChecks } = await import('./artifact-checks.mjs');
    await artifactChecks({ page, evidence, agent });
  } else {
  if (process.env.APPS_NEW_CHAT === '1') {
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('Keep this original conversation');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await page.getByRole('log').getByText('Done.', { exact: true }).waitFor();
    await Promise.all([page.waitForResponse(response => response.url().endsWith('/new')), page.getByRole('button', { name: 'New chat', exact: true }).click()]);
  }
  await page.getByRole('combobox', { name: 'Message', exact: true }).fill('draw first');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  if (process.env.APPS_EXPECT_FAILURE) {
    const region = page.getByRole('region', { name: 'MCP App: canvas', exact: true });
    await region.getByRole('status').filter({ hasText: process.env.APPS_EXPECT_FAILURE }).waitFor({ timeout: 18000 });
    await page.getByText('canvas__draw', { exact: true }).click();
    await page.getByRole('log').getByText('Drawing: first', { exact: false }).waitFor();
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('Chat progresses after app failure');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await page.getByRole('log').getByText('Chat progresses after app failure', { exact: true }).waitFor();
    assert.ok(!(await page.getByRole('log').innerText()).includes('[mcp-app-result:'));
    await page.screenshot({ path: path.join(evidence, 'failure-fallback.png'), fullPage: true });
    console.log('PASS failed app retains visible text fallback and chat still progresses');
  } else {
  const deadlineView = Date.now() + 15_000;
  let drawing;
  while (!drawing && Date.now() < deadlineView) {
    for (const frame of page.frames()) {
      if (await frame.getByRole('heading', { name: 'Drawing: first', exact: true }).isVisible().catch(() => false)) drawing = frame;
    }
    if (!drawing) await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(drawing, `App drawing did not appear. Browser errors: ${failures.join(', ')}`);
  const appLocation = new URL(await drawing.evaluate(() => location.href));
  assert.ok(['http:', 'https:'].includes(appLocation.protocol), 'App must have a real HTTP document URL');
  assert.notEqual(appLocation.hostname, new URL(origin).hostname, 'App and chat must use separate cookie hosts');
  if (!externalOrigin) assert.equal(appLocation.hostname, 'canvas.localhost');
  if (process.env.APPS_POLICY === 'blob') {
    for (const name of ['Blob script', 'Blob image', 'Blob worker', 'Chat isolation']) {
      await drawing.getByLabel(name, { exact: true }).filter({ hasText: name === 'Blob worker' ? 'blocked' : name === 'Chat isolation' ? 'isolated' : 'loaded' }).waitFor({ timeout: 5000 });
    }
    console.log('PASS declared CSP permits blob scripts/images; blob workers blocked; chat remains isolated');
  }
  if (process.env.APPS_PROTOCOL_VERSION === '2025-11-21') {
    assert.equal(await drawing.getByLabel('Negotiated protocol', { exact: true }).textContent(), '2026-01-26');
    assert.equal(await drawing.getByLabel('Invalid version request', { exact: true }).textContent(), 'rejected');
    assert.equal(await drawing.getByLabel('Lifecycle violations', { exact: true }).textContent(), '0');
    console.log('PASS unsupported app version negotiates host version and delivers after initialized; nonstring version rejected');
  }
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
  if (process.env.APPS_POLICY === 'modes-inline') {
    assert.equal(await page.getByRole('button', { name: 'Expand view: canvas', exact: true }).isVisible(), false);
    await drawing.getByRole('button', { name: 'Expand drawing', exact: true }).click();
    await drawing.getByLabel('Requested display mode', { exact: true }).filter({ hasText: 'inline' }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).count(), 0);
    console.log('PASS explicit inline-only capability restricts user and app fullscreen');
  } else {
    if (process.env.APPS_POLICY === 'modes-absent') {
      await page.getByRole('button', { name: 'Expand view: canvas', exact: true }).click({ timeout: 5000 });
      await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).click();
      console.log('PASS absent display modes permits user fullscreen');
    }
    await drawing.getByRole('button', { name: 'Expand drawing', exact: true }).click();
    await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).waitFor();
    const panel = await page.getByRole('region', { name: 'MCP App: canvas', exact: true }).boundingBox();
    const composer = await page.getByRole('combobox', { name: 'Message', exact: true }).boundingBox();
    assert.ok(panel && composer && composer.x + composer.width <= panel.x + 1, 'Expanded view must sit beside the chat input');
    await page.screenshot({ path: path.join(evidence, '02-fullscreen.png'), fullPage: true });
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('chat remains usable');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await page.getByRole('log').getByText('chat remains usable', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Close expanded view: canvas', exact: true }).click();
    await page.getByRole('button', { name: 'Expand view: canvas', exact: true }).waitFor();
    console.log('PASS app-requested fullscreen sits beside usable chat; close returns inline');
  }
  await page.getByRole('combobox', { name: 'Message', exact: true }).fill('draw second');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  if (process.env.APPS_PI_ALIAS || process.env.APPS_HOLD_RESULT) {
    await drawing.getByRole('heading', { name: 'Drawing input: second', exact: true }).waitFor({ timeout: 750 });
    assert.equal(await drawing.getByLabel('Results received', { exact: true }).textContent(), '1');
    console.log('PASS reused-view tool input arrives before held result');
  }
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
  if (process.env.APPS_ORIGIN_SECURITY === '1') {
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('draw other');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    let other;
    const deadline = Date.now() + 5000;
    while (!other && Date.now() < deadline) {
      for (const frame of page.frames()) {
        if (await frame.getByRole('heading', { name: 'Drawing: foreign', exact: true }).isVisible().catch(() => false)) other = frame;
      }
      if (!other) await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(other, 'A second server must render on its own assigned origin');
    assert.notEqual(new URL(other.url()).hostname, new URL(restored.url()).hostname);
    await restored.evaluate(() => {
      localStorage.setItem('alg530-isolation', 'canvas');
      document.cookie = 'alg530-isolation=canvas; SameSite=Lax';
    });
    for (const frame of [other, page.mainFrame()]) {
      assert.deepEqual(await frame.evaluate(() => ({
        value: localStorage.getItem('alg530-isolation'), cookie: document.cookie.includes('alg530-isolation=canvas')
      })), { value: null, cookie: false });
    }
    assert.equal(await restored.evaluate(() => {
      try { void top.document.body; return 'accessible'; } catch { return 'blocked'; }
    }), 'blocked');
    assert.deepEqual(await restored.evaluate(() => Array.from({ length: top.length }, (_, index) => {
      if (top[index] === parent) return 'own server';
      try { void top[index].document.body; return 'accessible'; } catch { return 'blocked'; }
    })), ['own server', 'blocked']);
    const malformed = await restored.evaluate(() => new Promise(resolve => {
      const listener = event => {
        if (event.data?.id !== 'malformed-security') return;
        removeEventListener('message', listener); resolve(event.data.error?.code);
      };
      addEventListener('message', listener);
      parent.postMessage({ jsonrpc: '2.0', id: 'malformed-security', method: 'ui/message', params: [] }, location.origin);
    }));
    assert.equal(malformed, -32600);
    const spoof = { jsonrpc: '2.0', id: 'spoof', method: 'ui/message', params: { role: 'user', content: { type: 'text', text: 'UNTRUSTED ORIGIN MESSAGE' } } };
    const spoofedRequests = [];
    const observe = request => {
      if (request.url().endsWith('/rpc') && request.postData()?.includes('UNTRUSTED ORIGIN MESSAGE')) spoofedRequests.push(request.url());
    };
    page.on('request', observe);
    await other.evaluate(({ origin, message }) => top.postMessage(message, origin), { origin, message: spoof });
    const wrapper = restored.parentFrame();
    await wrapper.goto(other.parentFrame().url());
    await wrapper.evaluate(({ origin, message }) => top.postMessage(message, origin), { origin, message: spoof });
    await page.waitForTimeout(250);
    page.off('request', observe);
    assert.deepEqual(spoofedRequests, [], 'Host must not relay hostile messages to its RPC endpoint');
    assert.equal(await page.getByRole('log').getByText('UNTRUSTED ORIGIN MESSAGE', { exact: true }).count(), 0);
    await page.reload();
    console.log('PASS server/chat storage and cookie isolation; malformed RPC and hostile source/origin messages rejected');
  }
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
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('draw first');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await firstHeld;
    await page.getByRole('combobox', { name: 'Message', exact: true }).fill('draw second');
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
  }
  }
  assert.deepEqual(failures, [], 'Browser must have no uncaught JavaScript errors');
} finally {
  await fs.writeFile(path.join(evidence, 'host.log'), logs);
  await fs.writeFile(path.join(evidence, 'browser-errors.json'), JSON.stringify(failures));
  await browser.close();
  await stopHost();
}
if (!process.env.APPS_EXPECT_FAILURE && !artifacts) {
const audit = await fs.readFile(path.join(agent, 'logs/agent.log'), 'utf8');
assert.ok(audit.includes('MCP App CSP') && audit.includes('connect-src'));
assert.ok(audit.includes('method=tools/call') && audit.includes('status="error"'));
console.log('PASS persistent audit records view CSP and rejected tool calls');
}
