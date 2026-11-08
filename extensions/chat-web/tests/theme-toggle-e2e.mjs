// Theme e2e: Light / Dark / System segmented toggle in the dashboard shell, plus tab-row polish.
// DAR_THEME_TEST_BINARY, PLAYWRIGHT_MODULE and PLAYWRIGHT_CHANNEL (e.g. chrome) are optional.
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
const agent = await fs.mkdtemp(path.join(os.tmpdir(), 'dar-theme-e2e-'));
const binary = path.resolve(process.env.DAR_THEME_TEST_BINARY || 'target/release/dar');
const listener = net.createServer();
await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
const port = listener.address().port;
await new Promise(resolve => listener.close(resolve));
const origin = `http://127.0.0.1:${port}`;
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: theme-e2e\nname: Theme e2e\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, 'fixtures/search-pi.cjs')}\n    idle_minutes: 0\n`);
let logs = '';
const host = spawn(binary, ['run', '--dir', agent], { stdio: ['ignore', 'pipe', 'pipe'] });
host.stdout.on('data', chunk => { logs += chunk; });
host.stderr.on('data', chunk => { logs += chunk; });
const DARK = 'dark';
const LIGHT = 'light';
const browser = await chromium.launch({ headless: true, channel: process.env.PLAYWRIGHT_CHANNEL });
const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, locale: 'en-US', colorScheme: 'dark' });
const page = await context.newPage();
const failures = [];
page.on('pageerror', error => failures.push(error.message));
const pass = message => console.log(`PASS ${message}`);
const radio = name => page.getByRole('radio', { name, exact: true });
const background = selector => page.locator(selector).evaluate(node => getComputedStyle(node).backgroundColor);
const expectChecked = async name => {
  for (const other of ['Light', 'Dark', 'System']) {
    assert.equal(await radio(other).getAttribute('aria-checked'), String(other === name), `${other} radio checked state with ${name} selected`);
  }
};
// What the user sees: the page background is dark or light (by luminance), whatever the exact palette.
const expectPage = async (mode, step) => {
  await page.waitForFunction(expected => {
    const [r, g, b] = getComputedStyle(document.body).backgroundColor.match(/\d+/g).map(Number);
    return ((0.2126 * r + 0.7152 * g + 0.0722 * b) / 255 < 0.5 ? 'dark' : 'light') === expected;
  }, mode, { timeout: 3000 }).catch(async () => {
    throw new Error(`${step}: page is not ${mode} (body background ${await background('body')})`);
  });
};
try {
  for (const deadline = Date.now() + 20_000; ; await new Promise(resolve => setTimeout(resolve, 100))) {
    try { if ((await fetch(`${origin}/chat`)).ok) break; } catch {}
    if (Date.now() > deadline) throw new Error(`Dashboard failed to start: ${logs}`);
  }
  await page.goto(origin);
  await radio('System').waitFor();

  // Default is System and follows the OS live.
  await expectChecked('System');
  await expectPage(DARK, 'default with dark OS');
  await page.emulateMedia({ colorScheme: 'light' });
  await expectPage(LIGHT, 'OS flips to light');
  await page.emulateMedia({ colorScheme: 'dark' });
  await expectPage(DARK, 'OS flips back to dark');
  pass('default is System and follows the OS live');

  // Explicit choices override the OS and persist across reloads.
  await radio('Light').click();
  await expectChecked('Light');
  await expectPage(LIGHT, 'Light with dark OS');
  await page.reload();
  await radio('Light').waitFor();
  await expectChecked('Light');
  await expectPage(LIGHT, 'Light persists after reload');
  assert.equal(await page.evaluate(() => localStorage.getItem('dar-theme')), 'light');
  await radio('Dark').click();
  await page.emulateMedia({ colorScheme: 'light' });
  await expectChecked('Dark');
  await expectPage(DARK, 'Dark with light OS');
  assert.equal(await page.evaluate(() => localStorage.getItem('dar-theme')), 'dark');
  await radio('System').click();
  await expectChecked('System');
  await expectPage(LIGHT, 'System with light OS');
  assert.equal(await page.evaluate(() => localStorage.getItem('dar-theme')), 'system');
  pass('Light and Dark override the OS, persist, and System resumes following it');

  // Missing or invalid stored values fall back to System.
  await page.evaluate(() => localStorage.setItem('dar-theme', 'sepia'));
  await page.reload();
  await radio('System').waitFor();
  await expectChecked('System');
  await expectPage(LIGHT, 'invalid stored value');
  pass('invalid stored value falls back to System');

  // Keyboard: the group is one tab stop and arrows / Home / End move the choice.
  await radio('System').focus();
  await page.keyboard.press('ArrowLeft');
  await expectChecked('Dark');
  await expectPage(DARK, 'ArrowLeft selects Dark');
  await page.keyboard.press('Home');
  await expectChecked('Light');
  await page.keyboard.press('End');
  await expectChecked('System');
  await page.keyboard.press('ArrowRight');
  await expectChecked('Light');
  assert.equal(await radio('Dark').getAttribute('tabindex'), '-1', 'unchecked radios leave the tab order');
  pass('arrow keys, Home and End move the theme choice like a radio group');

  // Tab row: active tab joins the page, bell and theme controls keep clear of the edge.
  const tab = page.getByRole('button', { name: 'Chat', exact: true });
  const tabStyle = await tab.evaluate(node => { const s = getComputedStyle(node); return { bg: s.backgroundColor, border: s.borderTopWidth + s.borderLeftWidth + s.borderRightWidth + s.borderBottomWidth }; });
  assert.equal(tabStyle.bg, await background('body'), 'active tab matches the page background');
  assert.equal(tabStyle.border, '0px0px0px0px', 'active tab has no border');
  const inactive = page.getByRole('button', { name: 'MCP', exact: true });
  if (await inactive.count()) assert.notEqual(await inactive.evaluate(node => getComputedStyle(node).backgroundColor), tabStyle.bg, 'inactive tab differs from the page');
  const bell = await page.getByRole('button', { name: /Notification sound/ }).boundingBox();
  const group = await page.getByRole('radiogroup', { name: 'Theme' }).boundingBox();
  assert.ok(group.x + group.width <= 1200 - 16, 'theme control keeps at least 1rem from the right edge');
  assert.ok(group.x - (bell.x + bell.width) >= 8, 'bell and theme control are spaced apart');
  pass('active tab is borderless and joined to the page; header controls have margin and spacing');
  assert.deepEqual(failures, [], 'No uncaught browser errors');
} finally {
  await browser.close();
  host.kill('SIGTERM');
  await fs.rm(agent, { recursive: true, force: true });
}
