// Public seam approved by ALG-532 Testing Decisions: browser-visible output from
// real bridge publication. Catches rendering/isolation/replay; misses runner auth.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
async function visibleFrame(page, role, name) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    for (const frame of page.frames()) if (await frame.getByRole(role, { name, exact: true }).isVisible().catch(() => false)) return frame;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`${role} ${name} did not appear`);
}
async function send(page, message) {
  const finished = page.getByRole('log').getByText('Done.', { exact: true });
  const before = await finished.count();
  await page.getByRole('combobox', { name: 'Message', exact: true }).fill(message);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  const deadline = Date.now() + 15000;
  while (await finished.count() <= before) {
    if (Date.now() > deadline) throw new Error(`Runner did not finish ${message}: ${await page.getByRole('log').innerText()}`);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
}
export async function artifactChecks({ page, evidence }) {
  if (process.env.APPS_ARTIFACTS_FAILURE === '1') return artifactFailureChecks({ page, evidence });
  await send(page, 'publish html');
  const firstReport = await visibleFrame(page, 'heading', 'Artifact report first');
  const reportViewer = firstReport.parentFrame();
  await page.screenshot({ path: path.join(evidence, 'artifact-01-inline.png'), fullPage: true });
  console.log('PASS artifact HTML visible under tool call without mcp.json');
  await send(page, 'publish markdown');
  const notes = await visibleFrame(page, 'heading', 'Project notes');
  await notes.getByRole('cell', { name: 'Ready', exact: true }).waitFor();
  assert.equal(await notes.getByText('MARKDOWN SCRIPT RAN', { exact: true }).count(), 0);
  console.log('PASS Markdown heading and table render with raw scripts sanitized');
  await send(page, 'publish png');
  const image = await visibleFrame(page, 'img', 'chart.png');
  assert.equal(await image.getByRole('img', { name: 'chart.png', exact: true }).evaluate(image => image.naturalWidth), 1);
  console.log('PASS PNG displayed with accessible filename');
  await send(page, 'publish pdf');
  const card = await visibleFrame(page, 'heading', 'handout.pdf');
  await card.getByText('application/pdf', { exact: true }).waitFor();
  await card.getByText('26 bytes', { exact: true }).waitFor();
  console.log('PASS unsupported PDF displays filename, MIME type and size card');
  await send(page, 'publish text');
  const deadlineText = Date.now() + 15000;
  let code;
  while (!code && Date.now() < deadlineText) {
    for (const frame of page.frames()) if (await frame.getByText('fn main() { println!("<safe>"); }', { exact: true }).isVisible().catch(() => false)) code = frame;
    if (!code) await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(code, 'Code extension without media type must show literal preformatted text');
  assert.ok((await (await code.frameElement()).boundingBox()).height < 300, 'Short note must shrink below initial view height');
  console.log('PASS absent MIME falls back to code extension, literal text and natural short-note height');
  assert.equal(await page.getByRole('region', { name: 'MCP App: dar', exact: true }).count(), 5, 'Different filenames must retain distinct views');
  for (const frame of [notes, reportViewer]) await frame.evaluate(() => {
    window.receivedInputs = [];
    addEventListener('message', event => { if (event.source === parent && event.data?.method === 'ui/notifications/tool-input') window.receivedInputs.push(event.data.params.arguments.path); });
  });
  await send(page, 'publish html second');
  await visibleFrame(page, 'heading', 'Artifact report second');
  assert.equal(await page.getByRole('region', { name: 'MCP App: dar', exact: true }).count(), 5, 'Same filename must update one existing view');
  await page.getByRole('button', { name: /newer version below/ }).waitFor();
  assert.equal(await page.getByRole('button', { name: /newer version below/ }).count(), 1, 'Older publish must show one marker');
  assert.equal(await page.getByRole('button', { name: 'View updated: dar', exact: true }).count(), 0);
  assert.deepEqual(await notes.evaluate(() => window.receivedInputs), []);
  console.log('PASS filenames isolate five views while repeat publish updates the original; early input reaches only matching file');
  await page.getByText('Revised report', { exact: true }).waitFor({ timeout: 2000 });
  await page.getByRole('region', { name: 'MCP App: dar', exact: true }).getByText('notes.md', { exact: true }).waitFor({ timeout: 2000 });
  console.log('PASS frame header shows updated title and filename fallback');
  const reportView = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'Revised report' });
  const downloadLink = reportView.getByRole('link', { name: 'Download', exact: true });
  await downloadLink.waitFor({ timeout: 2000 });
  assert.equal(await page.getByRole('link', { name: 'Download', exact: true }).count(), 5);
  const waitingDownload = page.waitForEvent('download');
  await downloadLink.click();
  const download = await waitingDownload;
  assert.equal(download.suggestedFilename(), 'report.html');
  assert.equal(await fs.readFile(await download.path(), 'utf8'), '<!doctype html><html><h1>Artifact report second</h1><p>Published in this chat.</p></html>');
  const response = await page.request.get(new URL(await downloadLink.getAttribute('href'), page.url()).href);
  assert.equal(response.headers()['content-type'], 'application/octet-stream');
  assert.equal(response.headers()['x-content-type-options'], 'nosniff');
  assert.ok(response.headers()['content-disposition'].startsWith('attachment;'));
  console.log('PASS every frame offers Download and latest original HTML downloads as exact bytes with attachment headers');
  await reportView.getByRole('button', { name: 'Expand view: dar', exact: true }).click();
  const panel = await reportView.boundingBox();
  const composer = await page.getByRole('combobox', { name: 'Message', exact: true }).boundingBox();
  assert.ok(panel && composer && composer.x + composer.width <= panel.x + 1, 'Expanded report must sit beside the chat input');
  await page.screenshot({ path: path.join(evidence, 'artifact-02-expanded.png'), fullPage: true });
  await send(page, 'chat progresses beside report');
  await reportView.getByRole('button', { name: 'Close expanded view: dar', exact: true }).click();
  console.log('PASS expand sits beside usable chat and close returns inline');
  await page.reload();
  await visibleFrame(page, 'heading', 'Artifact report second');
  await visibleFrame(page, 'heading', 'Project notes');
  assert.equal(await page.getByRole('region', { name: 'MCP App: dar', exact: true }).count(), 5);
  await page.screenshot({ path: path.join(evidence, 'artifact-03-reloaded.png'), fullPage: true });
  console.log('PASS reload restores distinct views and latest artifact versions');
  const forged = [];
  const observe = request => { if (request.url().endsWith('/rpc') && /FORGED REPORT MESSAGE|00000000-0000-0000-0000-000000000000/.test(request.postData() || '')) forged.push(request.postData()); };
  page.on('request', observe);
  await send(page, 'publish security');
  const sandbox = await visibleFrame(page, 'heading', 'Sandbox report');
  for (const name of ['Fetch unknown','Fetch CDN','XHR unknown','XHR CDN','WebSocket unknown','WebSocket CDN']) await sandbox.getByLabel(name, { exact: true }).filter({ hasText: 'blocked' }).waitFor();
  await sandbox.getByLabel('Connect violations', { exact: true }).filter({ hasText: '6' }).waitFor();
  await sandbox.getByLabel('Viewer DOM', { exact: true }).filter({ hasText: 'isolated' }).waitFor();
  await sandbox.getByLabel('CDN script', { exact: true }).filter({ hasText: 'loaded' }).waitFor({ timeout: 15000 });
  await sandbox.getByLabel('Unknown script', { exact: true }).filter({ hasText: 'blocked' }).waitFor();
  assert.deepEqual(forged, [], 'Report JSON-RPC must never reach host RPC');
  assert.equal(await page.getByRole('log').getByText('FORGED REPORT MESSAGE', { exact: true }).count(), 0);
  page.off('request', observe);
  console.log('PASS allowed CDN scripts load; unknown scripts and all six API transports blocked; opaque report cannot forge bridge requests');
  await sandbox.getByRole('link', { name: 'Jump to section', exact: true }).click();
  const scrolling = { report: await sandbox.evaluate(() => scrollY), viewer: await sandbox.parentFrame().evaluate(() => scrollY) };
  assert.ok(scrolling.report > 0 || scrolling.viewer > 0, 'Anchor link scrolls within artifact viewer');
  const opening = page.waitForEvent('popup');
  await sandbox.getByRole('link', { name: 'Source site', exact: true }).click();
  const popup = await opening;
  await popup.waitForURL('https://example.com/artifact-source');
  await popup.close();
  await sandbox.getByRole('link', { name: 'Unsafe link', exact: true }).click();
  await sandbox.getByLabel('Unsafe link status', { exact: true }).filter({ hasText: 'unchanged' }).waitFor();
  console.log('PASS anchors scroll inside report; external HTTP link opens tab; unsafe scheme blocked');
  const bootstrap = await page.request.get(new URL((await downloadLink.getAttribute('href')).replace(/\/download$/, ''), page.url()).href, { headers: { Referer: page.url() } });
  assert.ok(bootstrap.ok(), 'Own-session view bootstrap must be available');
  const original = await bootstrap.json();
  const denied = await sandbox.parentFrame().evaluate(uri => new Promise((resolve,reject) => {
    const timer = setTimeout(() => reject(new Error('No resources/read response')), 5000);
    const listener = event => { if (event.source === parent && event.data?.id === 'cross-artifact-test') { clearTimeout(timer); removeEventListener('message', listener); resolve(event.data); } };
    addEventListener('message', listener);
    parent.postMessage({jsonrpc:'2.0',id:'cross-artifact-test',method:'resources/read',params:{uri}},location.origin);
  }), original.result.structuredContent.uri);
  assert.ok(denied.error, 'View must reject another existing artifact URI');
  console.log('PASS view cannot read another published artifact');
  await send(page, 'publish svg');
  const svg = await visibleFrame(page, 'img', 'diagram.svg');
  assert.equal(await svg.getByRole('img', { name: 'diagram.svg', exact: true }).evaluate(image => image.naturalWidth), 16);
  assert.equal(await page.getByRole('log').getByText('SVG SCRIPT RAN', { exact: true }).count(), 0);
  assert.ok(!(await page.getByRole('log').innerText()).includes('[mcp-app-result:'));
  await page.screenshot({ path: path.join(evidence, 'artifact-04-security.png'), fullPage: true });
  console.log('PASS SVG renders as image without running embedded script; transport markers stay hidden');
}

// Browser HTTP transport fault injection: normal publication/provider behavior
// remains real; these failures exercise the viewer's reaction and chat progress.
async function artifactFailureChecks({ page, evidence }) {
  let fault = 'abort';
  const releases = [];
  const route = async route => {
    const message = route.request().postDataJSON();
    if (message?.method !== 'resources/read') return route.continue();
    if (fault === 'hold') await new Promise(resolve => releases.push(resolve));
    await route.abort('failed').catch(() => {});
  };
  await page.route('**/chat/*/apps/*/rpc', route);
  try {
    await send(page, 'publish html');
    let viewer;
    const deadline = Date.now() + 15000;
    while (!viewer && Date.now() < deadline) {
      for (const frame of page.frames()) if (await frame.getByRole('status').filter({ hasText: 'Unable to load artifact.' }).isVisible().catch(() => false)) viewer = frame;
      if (!viewer) await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(viewer, 'Failed resource transport must show a short viewer error');
    await page.getByText('mcp__dar__artifact_publish', { exact: true }).click();
    await page.getByRole('log').getByText('artifact published', { exact: true }).waitFor();
    await page.screenshot({ path: path.join(evidence, 'artifact-failure-fallback.png'), fullPage: true });
    console.log('PASS failed artifact read retains text fallback and short viewer error');
    fault = 'hold';
    await send(page, 'publish html second');
    await viewer.getByRole('status').filter({ hasText: 'Loading artifact…' }).waitFor();
    const started = Date.now();
    await send(page, 'chat progresses while artifact hangs');
    await page.getByRole('log').getByText('chat progresses while artifact hangs', { exact: true }).waitFor();
    await page.screenshot({ path: path.join(evidence, 'artifact-hang-chat.png'), fullPage: true });
    await viewer.getByRole('status').filter({ hasText: 'Unable to load artifact.' }).waitFor({ timeout: 35000 });
    assert.ok(Date.now() - started >= 29000, 'Held resource must exercise the actual viewer timeout');
    for (const release of releases) release();
    await send(page, 'chat progresses after artifact timeout');
    await page.screenshot({ path: path.join(evidence, 'artifact-timeout-fallback.png'), fullPage: true });
    console.log('PASS actual resource timeout shows fallback while chat progresses during hang and afterward');
  } finally {
    for (const release of releases) release();
    await page.unroute('**/chat/*/apps/*/rpc', route);
  }
}
