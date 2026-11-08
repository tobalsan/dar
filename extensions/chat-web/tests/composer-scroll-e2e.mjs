// Composer growth e2e: with a long last agent message, typing a tall multi-line
// reply one key at a time must never hide the end of that message behind the
// composer or scroll the thread away from it.
// DAR_APPS_TEST_BINARY, PLAYWRIGHT_MODULE, PLAYWRIGHT_CHANNEL (e.g. chrome) and PLAYWRIGHT_BROWSER (chromium or webkit; WebKit clamps the thread's scroll while the textarea resizes) are optional.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const playwright = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const engine = process.env.PLAYWRIGHT_BROWSER || 'chromium';
const browserType = playwright[engine] || playwright.default[engine];
const here = path.dirname(fileURLToPath(import.meta.url));
const binary = path.resolve(process.env.DAR_APPS_TEST_BINARY || 'target/release/dar');
const agent = await fs.mkdtemp(path.join(os.tmpdir(), 'dar-composer-scroll-'));
const listener = net.createServer();
await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
const port = listener.address().port;
await new Promise(resolve => listener.close(resolve));
const origin = `http://127.0.0.1:${port}`;
await fs.writeFile(path.join(agent, 'agent.yaml'), `id: composer-scroll\nname: Iris\nrunner:\n  use: fake\nforeground: logs\ndashboard:\n  bind: 127.0.0.1\n  port: ${port}\nextensions:\n  chat-web:\n    backend: pi\n    command: ${path.join(here, 'fixtures/apps-pi.cjs')}\n    idle_minutes: 0\n`);
const host = spawn(binary, ['run', '--dir', agent], { stdio: 'ignore', detached: true });
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
let browser;
try {
  for (const deadline = Date.now() + 20_000; ; await wait(100)) {
    try { if ((await fetch(`${origin}/chat`)).ok) break; } catch {}
    if (Date.now() > deadline) throw new Error('Dashboard failed to start');
  }
  browser = await browserType.launch({ headless: true, channel: engine === 'chromium' ? process.env.PLAYWRIGHT_CHANNEL : undefined });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await page.goto(`${origin}/`);
  const log = page.getByRole('log');
  const input = page.getByRole('combobox', { name: 'Message', exact: true });
  const event = e => page.evaluate(e => window.renderChatEvent(e), e);
  const paragraphs = Array.from({ length: 16 }, (_, i) => `Paragraph ${i + 1} of a long agent reply that fills the thread. `.repeat(4));
  await event({ type: 'user', text: 'Write something long.' });
  await event({ type: 'delta', text: `${paragraphs.join('\n\n')}\n\nTHE END OF THE LONG MESSAGE` });
  await event({ type: 'finished' });
  const end = log.getByText('THE END OF THE LONG MESSAGE');
  await end.waitFor();
  const frames = () => page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  // The reader sees the end of the message: its box sits above the composer and
  // the thread is still scrolled to its bottom.
  const assertEndVisible = async what => {
    await frames();
    const end = await page.getByText('THE END OF THE LONG MESSAGE').boundingBox();
    const composer = await page.getByRole('combobox', { name: 'Message', exact: true }).boundingBox();
    const gap = await log.evaluate(node => node.scrollHeight - node.scrollTop - node.clientHeight);
    assert.ok(end.y + end.height <= composer.y + 1, `${what}: end of the message (${end.y + end.height}) is covered by the composer (top ${composer.y})`);
    assert.ok(gap < 4, `${what}: thread drifted ${gap}px away from its bottom`);
  };
  await wait(1000); // the empty-chat layout eases the composer down once the first message arrives
  await assertEndVisible('before typing');

  // Type a 10-line reply one key at a time (Shift+Enter makes a new line), checking after every key.
  await input.click();
  const lines = Array.from({ length: 10 }, (_, i) => `reply line ${i + 1} with some words`);
  for (const [i, line] of lines.entries()) {
    for (const char of line) { await page.keyboard.type(char); await assertEndVisible(`line ${i + 1}, typing "${char}"`); }
    if (i < lines.length - 1) { await page.keyboard.press('Shift+Enter'); await assertEndVisible(`after line ${i + 1}`); }
  }
  // More characters on the already-tall composer, then shrink it back.
  for (const char of ' and more') { await page.keyboard.type(char); await assertEndVisible(`tall composer, typing "${char}"`); }
  for (let i = 0; i < 60; i++) await page.keyboard.press('Backspace');
  await assertEndVisible('after deleting text');
  console.log('PASS typing a tall multi-line reply key by key keeps the end of the last message above the composer');
} finally {
  await browser?.close();
  try { process.kill(-host.pid, 'SIGTERM'); } catch {}
  await fs.rm(agent, { recursive: true, force: true });
}
