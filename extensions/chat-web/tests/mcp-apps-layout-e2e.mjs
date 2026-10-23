// Layout e2e: expand icon button, transcript scroll across the main panel, expanded panel full height.
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
const evidence = path.resolve(process.env.APPS_EVIDENCE_DIR || 'validation/apps-layout-e2e');
const agent = path.join(evidence, `agent-${process.pid}`);
await fs.mkdir(agent, { recursive: true });
const binary = path.resolve(process.env.DAR_APPS_TEST_BINARY || 'target/release/dar');
const listener = net.createServer();
await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
const port = listener.address().port;
await new Promise(resolve => listener.close(resolve));
const origin = `http://127.0.0.1:${port}`;
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: apps-layout\nname: Apps layout\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, 'fixtures/apps-pi.cjs')}\n    idle_minutes: 0\n    apps:\n      sandbox_port: 0\n`);
const env = { DAR_APPS_TEST_HTML: path.join(here, 'fixtures/tall.html') };
await fs.writeFile(path.join(agent, 'mcp.json'), JSON.stringify({ mcpServers: { canvas: { command: process.execPath, args: [path.join(here, 'fixtures/apps-server.cjs')], env } } }));
let logs = '';
const host = spawn(binary, ['run', '--dir', agent], { stdio: ['ignore', 'pipe', 'pipe'] });
host.stdout.on('data', chunk => { logs += chunk; });
host.stderr.on('data', chunk => { logs += chunk; });
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 800 } });
const failures = [];
page.on('pageerror', error => failures.push(error.message));
const send = async text => {
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill(text);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
};
const number = async (frame, name) => Number(await frame.getByLabel(name, { exact: true }).textContent());
try {
  for (const deadline = Date.now() + 20_000; ; await new Promise(resolve => setTimeout(resolve, 100))) {
    try { if ((await fetch(`${origin}/chat`)).ok) break; } catch {}
    if (Date.now() > deadline) throw new Error(`Dashboard failed to start: ${logs}`);
  }
  await page.goto(`${origin}/`);
  for (let i = 0; i < 12; i++) {
    await send(`filler ${i}`);
    await page.getByRole('log').getByText(`filler ${i}`, { exact: true }).waitFor();
  }
  await send('draw first');
  let view;
  for (const deadline = Date.now() + 15_000; !view && Date.now() < deadline; await new Promise(resolve => setTimeout(resolve, 100))) {
    for (const frame of page.frames()) if (await frame.getByRole('heading', { name: 'Tall view', exact: true }).isVisible().catch(() => false)) view = frame;
  }
  assert.ok(view, 'Tall view must appear');
  const region = page.getByRole('region', { name: 'MCP App: canvas', exact: true });
  const log = page.getByRole('log');
  const expand = page.getByRole('button', { name: 'Expand view: canvas', exact: true });
  await expand.waitFor();
  assert.equal(await expand.innerText(), '', 'Expand button shows an icon, not text');
  assert.equal(await expand.getAttribute('title'), 'Expand view');
  const size = await expand.boundingBox();
  assert.ok(size.width <= 36 && size.height <= 36, 'Expand button is compact');
  await page.screenshot({ path: path.join(evidence, 'inline-margin-scroll.png') });
  console.log('PASS expand control is a compact icon button with accessible name and tooltip');

  // Wheel over the margins of the main panel scrolls the transcript.
  const box = await log.boundingBox();
  const sidebar = await page.getByRole('complementary', { name: 'Conversations' }).boundingBox();
  assert.ok(Math.abs(box.x - (sidebar.x + sidebar.width)) < 2, 'Scroll area starts at the sidebar edge');
  const column = await page.getByText('filler 0', { exact: true }).boundingBox();
  assert.ok(column.x - box.x > 100, 'Message column is narrower than the scroll area');
  const scrollTop = () => log.evaluate(node => node.scrollTop);
  for (const x of [box.x + 20, box.x + box.width - 20]) {
    await log.evaluate(node => { node.scrollTop = node.scrollHeight; });
    const before = await scrollTop();
    assert.ok(before > 0, 'Transcript must overflow');
    await page.mouse.move(x, box.y + box.height / 2);
    await page.mouse.wheel(0, -200);
    await page.waitForFunction(value => document.getElementById('chat-transcript').scrollTop < value, before);
  }
  console.log('PASS wheel over left and right margins scrolls the transcript');

  // Expanded panel.
  await expand.click();
  const close = page.getByRole('button', { name: 'Close expanded view: canvas', exact: true });
  await close.waitFor();
  assert.equal(await close.getAttribute('title'), 'Close expanded view');
  const panelBox = () => region.boundingBox();
  const iframeHeights = async () => ({ outer: (await page.getByTitle('canvas app', { exact: true }).boundingBox()).height });
  const inner = () => view.evaluate(() => ({ inner: window.innerHeight, width: window.innerWidth }));
  const composer = await page.getByRole('textbox', { name: 'Message', exact: true }).boundingBox();
  let panel = await panelBox();
  assert.ok(composer.x + composer.width <= panel.x + 1, 'Panel sits beside the composer');
  const log2 = await log.boundingBox();
  assert.ok(Math.abs(log2.x + log2.width - panel.x) < 2, 'Scroll area ends at the panel edge');
  const assertFilled = async step => {
    const viewport = page.viewportSize();
    for (let i = 0; i < 30; i++) {
      panel = await panelBox();
      if (Math.abs(panel.y + panel.height - viewport.height) < 2) break;
      await page.waitForTimeout(100);
    }
    assert.ok(Math.abs(panel.y + panel.height - viewport.height) < 2, `${step}: panel reaches the window bottom (${panel.y + panel.height} vs ${viewport.height})`);
    assert.ok(Math.abs(panel.x + panel.width - viewport.width) < 2, `${step}: panel reaches the window right edge`);
    const frameHeight = (await iframeHeights()).outer;
    const headerAndStatus = panel.height - frameHeight;
    assert.ok(headerAndStatus >= 0 && headerAndStatus < 80, `${step}: outer frame fills panel (panel ${panel.height}, frame ${frameHeight})`);
    const innerHeight = (await inner()).inner;
    assert.ok(Math.abs(innerHeight - frameHeight) <= 1, `${step}: inner frame fills outer (${innerHeight} vs ${frameHeight})`);
    await view.getByLabel('Context height', { exact: true }).filter({ hasText: String(Math.round(frameHeight)) }).waitFor({ timeout: 3000 }).catch(async () => {
      throw new Error(`${step}: hostContext height ${await view.getByLabel('Context height', { exact: true }).textContent()} != frame ${frameHeight}`);
    });
  };
  await assertFilled('after expand');
  await page.screenshot({ path: path.join(evidence, 'expanded-tall.png') });
  console.log('PASS expanded frames fill panel height and hostContext matches after layout');

  await page.setViewportSize({ width: 1300, height: 650 });
  await assertFilled('after window resize');
  const gripBox = await page.getByRole('separator', { name: 'Resize app panel' }).boundingBox();
  await page.mouse.move(gripBox.x + 3, gripBox.y + 200);
  await page.mouse.down();
  await page.mouse.move(gripBox.x - 250, gripBox.y + 200, { steps: 5 });
  await page.mouse.up();
  await assertFilled('after divider drag');
  console.log('PASS hostContext height matches after window resize and divider drag');

  // Tall content scrolls inside the panel to its end.
  const lastLine = view.getByText('Last line', { exact: true });
  assert.equal(await lastLine.evaluate(node => { const r = node.getBoundingClientRect(); return r.bottom <= innerHeight; }), false, 'Last line starts below the fold');
  await view.locator('body').hover();
  await page.mouse.wheel(0, 5000);
  await view.waitForFunction(() => { const r = [...document.querySelectorAll('p')].find(p => p.textContent === 'Last line').getBoundingClientRect(); return r.bottom <= innerHeight + 1 && r.top >= 0; });
  const panelFinal = await panelBox();
  const lastBox = await lastLine.boundingBox();
  assert.ok(lastBox.y + lastBox.height <= panelFinal.y + panelFinal.height + 1, 'Last line is visible inside the panel');
  await page.screenshot({ path: path.join(evidence, 'expanded-tall-scrolled.png') });
  console.log('PASS tall content scrolls inside the panel to its last line');

  await close.click();
  await expand.waitFor();
  assert.deepEqual(failures, [], 'No uncaught browser errors');
} finally {
  await browser.close();
  host.kill('SIGTERM');
  await fs.writeFile(path.join(evidence, 'layout-host.log'), logs);
}
