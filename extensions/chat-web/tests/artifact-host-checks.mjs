// Approved seam: ALG-534 Testing Decisions, real browser-visible artifact UI.
// Catches publish/host lifecycle and navigation; misses other browser engines.
import assert from 'node:assert/strict';
import path from 'node:path';

async function send(page, message) {
  const finished = page.getByRole('log').getByText('Done.', { exact: true });
  const before = await finished.count();
  await page.getByRole('combobox', { name: 'Message', exact: true }).fill(message);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  const deadline = Date.now() + 15000;
  while (await finished.count() <= before) {
    if (Date.now() > deadline) throw new Error(`Runner did not finish ${message}`);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
}

export async function artifactHostChecks({ page, evidence, restartHost }) {
  await send(page, 'publish html');
  const report = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'Quarterly report' });
  await report.getByRole('button', { name: 'Close expanded view: dar', exact: true }).waitFor({ timeout: 5000 });
  const panel = await report.boundingBox();
  const composer = await page.getByRole('combobox', { name: 'Message', exact: true }).boundingBox();
  assert.ok(panel && composer && composer.x + composer.width <= panel.x + 1, 'Live report opens beside usable chat');
  const card = page.getByRole('button', { name: 'Quarterly report Open in side panel', exact: true });
  await card.waitFor();
  await card.click();
  await page.screenshot({ path: path.join(evidence, 'artifact-host-auto-open.png'), fullPage: true });
  await report.getByRole('button', { name: 'Close expanded view: dar', exact: true }).click();
  assert.equal(await card.count(), 0, 'Closing side panel removes its compact card');
  await report.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  console.log('PASS live artifact auto-opens beside chat, transcript card focuses it, closing restores inline');
  await report.getByText('HTML · 88 B', { exact: true }).waitFor({ timeout: 3000 });
  await report.getByRole('button', { name: 'Open in browser', exact: true }).waitFor();
  console.log('PASS artifact header shows its file type, byte size and browser action');
  const opening = page.waitForEvent('popup', { timeout: 3000 });
  await report.getByRole('button', { name: 'Open in browser', exact: true }).click();
  const tab = await opening;
  await tab.getByRole('heading', { name: 'Artifact report first', exact: true }).waitFor();
  assert.equal(new URL(tab.url()).hostname, 'dar.localhost', 'Open tab uses assigned sandbox host');
  assert.equal(await tab.evaluate(() => opener), null, 'Artifact tab has no opener');
  await tab.close();
  console.log('PASS browser action opens a sandbox tab with report content and no opener');
  await page.reload();
  await report.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor({ timeout: 5000 });
  assert.equal(await page.getByRole('button', { name: 'Close expanded view: dar', exact: true }).count(), 0, 'Reload keeps artifacts inline');
  console.log('PASS reload replays artifact inline without auto-opening');
  await send(page, 'publish html second');
  const revised = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'Revised report' });
  await revised.getByRole('button', { name: 'Close expanded view: dar', exact: true }).waitFor();
  await page.getByRole('button', { name: 'Revised report Open in side panel', exact: true }).waitFor();
  await send(page, 'publish markdown');
  const notes = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'notes.md' });
  await notes.getByRole('button', { name: 'Close expanded view: dar', exact: true }).waitFor();
  assert.equal(await page.getByRole('button', { name: 'Close expanded view: dar', exact: true }).count(), 1, 'New publish replaces the current side panel');
  await revised.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  await notes.getByRole('button', { name: 'Close expanded view: dar', exact: true }).click();
  console.log('PASS every live publish auto-opens, including reused filenames, and replaces the previous side panel');

  await page.setViewportSize({ width: 700, height: 1000 });
  await send(page, 'publish html');
  await report.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  assert.equal(await page.getByRole('button', { name: 'Close expanded view: dar', exact: true }).count(), 0, 'Narrow screen keeps new publish inline');
  await page.screenshot({ path: path.join(evidence, 'artifact-host-narrow.png'), fullPage: true });
  await page.setViewportSize({ width: 1440, height: 1000 });
  console.log('PASS 700px screen keeps live publication inline');

  const reconnected = page.waitForResponse(response => new URL(response.url()).pathname.endsWith('/stream') && response.ok(), { timeout: 15000 });
  await restartHost();
  await reconnected;
  await report.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  await send(page, 'chat progresses after reconnect');
  assert.equal(await page.getByRole('button', { name: 'Close expanded view: dar', exact: true }).count(), 0);
  console.log('PASS actual SSE reconnect keeps artifact inline and chat progresses');

  const sessions = await (await page.request.get(new URL('/chat/sessions', page.url()).href)).json();
  assert.equal(sessions.length, 1, 'Fixture exposes its persisted live session');
  const renamed = await page.request.patch(new URL(`/chat/sessions/${encodeURIComponent(sessions[0].id)}`, page.url()).href, { data: { title: 'Artifact host acceptance' } });
  assert.ok(renamed.ok());
  const conversation = page.getByRole('complementary', { name: 'Conversations', exact: true }).getByRole('button', { name: /Artifact host acceptance/ }).first();
  await conversation.waitFor();
  await page.getByRole('button', { name: 'New chat', exact: true }).click();
  await report.waitFor({ state: 'detached' });
  await conversation.click();
  await report.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  await notes.getByRole('button', { name: 'Expand view: dar', exact: true }).waitFor();
  assert.equal(await page.getByRole('button', { name: 'Close expanded view: dar', exact: true }).count(), 0);
  console.log('PASS resuming live conversation keeps restored artifacts inline');
  const reopening = page.waitForEvent('popup');
  await report.getByRole('button', { name: 'Open in browser', exact: true }).click();
  const freshTab = await reopening;
  await freshTab.getByRole('heading', { name: 'Artifact report first', exact: true }).waitFor();
  const firstOpenUrl = freshTab.url();
  await freshTab.close();
  const openingAgain = page.waitForEvent('popup');
  await report.getByRole('button', { name: 'Open in browser', exact: true }).click();
  const again = await openingAgain;
  await again.getByRole('heading', { name: 'Artifact report first', exact: true }).waitFor();
  assert.equal(new URL(again.url()).pathname, new URL(firstOpenUrl).pathname);
  assert.notEqual(again.url(), firstOpenUrl, 'Same old restored record requests a fresh sandbox ticket on every click');
  await again.close();
  console.log('PASS restored old view opens with a fresh sandbox URL');
  await artifactOpenFailures(page, report, evidence);
  await send(page, 'publish zip');
  const archive = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'archive.zip' });
  await archive.getByRole('link', { name: 'Download', exact: true }).waitFor();
  await archive.getByText('File · 22 B', { exact: true }).waitFor();
  assert.equal(await archive.getByRole('button', { name: 'Open in browser', exact: true }).count(), 0, 'Unsupported binary artifact offers no browser action');
  console.log('PASS actual ZIP publish offers Download and no Open in browser');
}

async function artifactOpenFailures(page, report, evidence) {
  const open = report.getByRole('button', { name: 'Open in browser', exact: true });
  const status = report.getByRole('status');
  const pages = page.context().pages().length;
  await page.evaluate(() => { window.artifactTestOpen=window.open;window.open=()=>null; });
  try {
    await open.click();
    await status.filter({ hasText: 'Allow pop-ups to open the file' }).waitFor();
    assert.equal(page.context().pages().length, pages);
  } finally {
    await page.evaluate(() => { window.open=window.artifactTestOpen;delete window.artifactTestOpen; });
  }
  console.log('PASS blocked popup shows a short actionable header error');

  const unavailable = route => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: 'Artifact unavailable' }) });
  await page.route('**/chat/*/apps/*/open', unavailable);
  try {
    const opening = page.waitForEvent('popup');
    await open.click();
    const tab = await opening;
    await status.filter({ hasText: 'Unable to open file: Artifact unavailable' }).waitFor();
    assert.ok(tab.isClosed(), 'Failed open closes the blank tab');
  } finally {
    await page.unroute('**/chat/*/apps/*/open', unavailable);
  }
  console.log('PASS failed open request closes its blank tab and shows the error');

  let release;
  const held = new Promise(resolve => { release=resolve; });
  const hang = async route => { await held;await route.abort('failed').catch(()=>{}); };
  await page.route('**/chat/*/apps/*/open', hang);
  try {
    const started = Date.now();
    const opening = page.waitForEvent('popup');
    await open.click();
    const tab = await opening;
    await send(page, 'chat progresses while browser open hangs');
    await status.filter({ hasText: 'Opening the file timed out' }).waitFor({ timeout: 15000 });
    const elapsed = Date.now()-started;
    assert.ok(elapsed>=9000 && elapsed<15000, 'Open request exercises its actual 10-second bound');
    assert.ok(tab.isClosed(), 'Timed out open closes its blank tab');
    await send(page, 'chat progresses after browser open timeout');
    await page.screenshot({ path: path.join(evidence, 'artifact-host-open-timeout.png'), fullPage: true });
  } finally {
    release();
    await page.unroute('**/chat/*/apps/*/open', hang);
  }
  console.log('PASS actual 10-second open timeout closes tab, reports error, and chat progresses during and after hang');
}
