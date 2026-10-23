// Public seam approved by ALG-534 Testing Decisions: browser-visible output from
// real bridge publication. Catches rendering/isolation/replay; misses runner auth.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
async function artifactAssets(page) {
  const root = process.env.APPS_ARTIFACT_ASSETS || (process.env.PLAYWRIGHT_MODULE ? path.dirname(path.dirname(process.env.PLAYWRIGHT_MODULE)) : path.join(here, 'node_modules'));
  const assets = new Map();
  for (const [name, version, files] of [['pdfjs-dist', '5.4.624', ['build/pdf.mjs', 'build/pdf.worker.mjs']], ['pele', '1.9.0', ['dist/pele.global.js']]]) {
    assert.equal(JSON.parse(await fs.readFile(path.join(root, name, 'package.json'), 'utf8')).version, version);
    for (const file of files) assets.set(`https://cdn.jsdelivr.net/npm/${name}@${version}/${file}`, await fs.readFile(path.join(root, name, file)));
  }
  // The security fixture tests that an allowed script executes, independently of its library.
  assets.set('https://cdn.jsdelivr.net/npm/dayjs@1.11.13/dayjs.min.js', Buffer.from('globalThis.dayjs = function () {};'));
  const requests = [];
  await page.context().route('https://cdn.jsdelivr.net/**', route => {
    requests.push(route.request());
    const body = assets.get(route.request().url());
    return body ? route.fulfill({ body, contentType: 'text/javascript', headers: { 'Access-Control-Allow-Origin': '*' } }) : route.abort();
  });
  await page.context().route('https://example.com/**', route => route.fulfill({ contentType: 'text/html', body: '<h1>Source site</h1>' }));
  return requests;
}
async function visibleFrame(page, role, name) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    for (const frame of page.frames()) {
      const match = role === 'status' ? frame.getByRole(role).filter({ hasText: name }) : frame.getByRole(role, { name, exact: true });
      if (await match.isVisible().catch(() => false)) return frame;
    }
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
export async function artifactChecks({ page, evidence, restartHost }) {
  const cdnRequests = await artifactAssets(page);
  if (process.env.APPS_ARTIFACT_HOST === '1') {
    const { artifactHostChecks } = await import('./artifact-host-checks.mjs');
    return artifactHostChecks({ page, evidence, restartHost });
  }
  if (process.env.APPS_ARTIFACTS_FAILURE === '1') return artifactFailureChecks({ page, evidence });
  if (process.env.APPS_ARTIFACTS_SLICE === 'pdf-open') return nativePdfCheck({ page, evidence });
  if (process.env.APPS_ARTIFACTS_SLICE === 'pdf-failure') return pdfFailureChecks({ page, evidence });
  await send(page, 'publish html');
  const firstReport = await visibleFrame(page, 'heading', 'Artifact report first');
  const reportViewer = firstReport.parentFrame();
  await page.screenshot({ path: path.join(evidence, 'artifact-01-inline.png'), fullPage: true });
  console.log('PASS artifact HTML visible under tool call without mcp.json');
  const beforePlainNote = cdnRequests.length;
  await send(page, 'publish markdown');
  const notes = await visibleFrame(page, 'heading', 'Project notes');
  await notes.getByRole('cell', { name: 'Ready', exact: true }).waitFor();
  assert.equal(await notes.getByText('MARKDOWN SCRIPT RAN', { exact: true }).count(), 0);
  assert.equal(await notes.evaluate(() => origin), 'null', 'Markdown must render with an opaque origin');
  assert.equal(await notes.evaluate(() => { try { void parent.document; return 'accessible'; } catch { return 'blocked'; } }), 'blocked');
  console.log('PASS Markdown heading and table render sanitized inside opaque report frame');
  assert.equal(cdnRequests.length, beforePlainNote, 'Plain Markdown must need no CDN script');
  if (process.env.APPS_ARTIFACTS_SLICE === 'markdown') return;
  if (process.env.APPS_ARTIFACTS_SLICE === 'pdf') return pdfChecks({ page, evidence, cap: false });
  if (process.env.APPS_ARTIFACTS_SLICE === 'pdf-cap') return pdfChecks({ page, evidence });
  if (process.env.APPS_ARTIFACTS_SLICE === 'mermaid') return mermaidChecks({ page, evidence });
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
  const codeView = page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'code.rs' });
  await codeView.getByRole('button', { name: 'Close expanded view: dar', exact: true }).click();
  const sizingDeadline = Date.now() + 5000;
  while ((await (await code.frameElement()).boundingBox()).height >= 300 && Date.now() < sizingDeadline) await new Promise(resolve => setTimeout(resolve, 100));
  assert.ok((await (await code.frameElement()).boundingBox()).height < 300, 'Short inline note must shrink below initial view height');
  console.log('PASS absent MIME falls back to code extension, literal text and natural short-note height');
  assert.equal(await page.getByRole('region', { name: 'MCP App: dar', exact: true }).count(), 5, 'Different filenames must retain distinct views');
  for (const frame of [notes.parentFrame(), reportViewer]) await frame.evaluate(() => {
    window.receivedInputs = [];
    addEventListener('message', event => { if (event.source === parent && event.data?.method === 'ui/notifications/tool-input') window.receivedInputs.push(event.data.params.arguments.path); });
  });
  await send(page, 'publish html second');
  await visibleFrame(page, 'heading', 'Artifact report second');
  assert.equal(await page.getByRole('region', { name: 'MCP App: dar', exact: true }).count(), 5, 'Same filename must update one existing view');
  await page.getByRole('button', { name: /newer version below/ }).waitFor();
  assert.equal(await page.getByRole('button', { name: /newer version below/ }).count(), 1, 'Older publish must show one marker');
  assert.equal(await page.getByRole('button', { name: 'View updated: dar', exact: true }).count(), 0);
  assert.deepEqual(await notes.parentFrame().evaluate(() => window.receivedInputs), []);
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
  const closeExpanded = reportView.getByRole('button', { name: 'Close expanded view: dar', exact: true });
  if (await closeExpanded.isVisible()) await closeExpanded.click();
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
  await mermaidChecks({ page, evidence });
  await pdfChecks({ page, evidence });
  await pdfFailureChecks({ page, evidence });
}

async function pdfChecks({ page, evidence, cap = true }) {
  await send(page, 'publish pdfvalid');
  const pdf = await visibleFrame(page, 'img', 'Page 1 of 2');
  await pdf.getByRole('img', { name: 'Page 2 of 2', exact: true }).waitFor();
  await pdf.getByText('PDF · 2 pages', { exact: true }).waitFor();
  assert.equal(await pdf.evaluate(() => origin), 'null');
  const pixels = await pdf.getByRole('img', { name: 'Page 1 of 2', exact: true }).evaluate(canvas => {
    const pixel = [...canvas.getContext('2d').getImageData(canvas.width / 2, canvas.height / 2, 1, 1).data];
    return { pixel, width: canvas.width, css: canvas.getBoundingClientRect().width, frame: innerWidth };
  });
  assert.ok(pixels.pixel[2] > 150 && pixels.pixel[0] < 80, 'The PDF fixture paints a blue rectangle at page center');
  assert.ok(pixels.width >= 900 && pixels.css >= pixels.frame * 0.9 && pixels.css <= pixels.frame, 'Page bitmap stays sharp and fills the report width');
  assert.deepEqual(await pdf.parentFrame().evaluate(() => [typeof globalThis.pele, typeof globalThis.pdfjsWorker, typeof globalThis.pdfjsLib]), ['undefined', 'undefined', 'undefined']);
  await page.screenshot({ path: path.join(evidence, 'artifact-pdf-pages.png'), fullPage: true });
  console.log('PASS PDF pages render opaque, accessible, full width and nonblank with sharp bitmaps; viewer has no CDN globals');
  if (!cap) return;
  await send(page, 'publish pdflong');
  const long = await visibleFrame(page, 'img', 'Page 50 of 51');
  await long.getByText('Showing the first 50 of 51 pages. Download the file for all pages.', { exact: true }).waitFor({ timeout: 5000 });
  assert.equal(await long.getByRole('img').count(), 50);
  assert.equal(await long.getByRole('img', { name: 'Page 51 of 51', exact: true }).count(), 0);
  await page.screenshot({ path: path.join(evidence, 'artifact-pdf-cap.png'), fullPage: true });
  console.log('PASS long PDF renders first 50 pages with total count and download guidance');
}

async function mermaidChecks({ page, evidence }) {
  await send(page, 'publish mermaid');
  const diagram = await visibleFrame(page, 'heading', 'Mermaid notes');
  await diagram.getByRole('img', { name: 'Mermaid diagram', exact: true }).waitFor({ timeout: 5000 });
  await diagram.getByText('Publish', { exact: true }).waitFor();
  await diagram.getByText('Read', { exact: true }).waitFor();
  await diagram.getByRole('link', { name: 'Jump to details', exact: true }).click();
  assert.ok(await diagram.evaluate(() => scrollY) > 0 || await diagram.parentFrame().evaluate(() => scrollY) > 0, 'Markdown anchor must scroll within report');
  const opening = page.waitForEvent('popup');
  await diagram.getByRole('link', { name: 'Markdown source', exact: true }).click();
  const source = await opening;
  await source.waitForURL('https://example.com/markdown-source');
  await source.close();
  assert.deepEqual(await diagram.parentFrame().evaluate(() => [typeof globalThis.pele, typeof globalThis.pdfjsWorker]), ['undefined', 'undefined']);
  await send(page, 'publish mermaid broken');
  const broken = await visibleFrame(page, 'heading', 'Broken diagram');
  await broken.getByText('this is not a diagram', { exact: true }).waitFor();
  assert.equal(await broken.getByRole('img').count(), 0);
  await page.screenshot({ path: path.join(evidence, 'artifact-mermaid-fallback.png'), fullPage: true });
  console.log('PASS Mermaid accessible SVG, Markdown links/anchors, broken source fallback, viewer has no CDN globals');
  const pattern = 'https://cdn.jsdelivr.net/npm/pele@1.9.0/**';
  const blocked = route => route.abort();
  await page.route(pattern, blocked);
  try {
    await send(page, 'publish mermaid');
    const offline = await visibleFrame(page, 'heading', 'Mermaid notes');
    await offline.getByText(/flowchart LR\s+A\[Publish\] --> B\[Read\]/).waitFor();
    assert.equal(await offline.getByRole('img').count(), 0);
    console.log('PASS unavailable Pele leaves Mermaid source readable');
  } finally { await page.unroute(pattern, blocked); }
}

async function pdfFailureChecks({ page, evidence }) {
  const pattern = 'https://cdn.jsdelivr.net/npm/pdfjs-dist@5.4.624/**';
  const blocked = route => route.abort();
  await page.route(pattern, blocked);
  try {
    await send(page, 'publish pdfvalid');
    const card = await visibleFrame(page, 'heading', 'pages.pdf');
    await card.getByText('application/pdf', { exact: true }).waitFor();
    console.log('PASS valid PDF falls back to file card when CDN is blocked');
  } finally { await page.unroute(pattern, blocked); }
  const releases = [];
  let requested;
  const startedLoading = new Promise(resolve => { requested = resolve; });
  const held = async route => {
    requested();
    await new Promise(resolve => releases.push(resolve));
    await route.abort().catch(() => {});
  };
  await page.route(pattern, held);
  try {
    const started = Date.now();
    await send(page, 'publish pdfvalid');
    await startedLoading;
    await send(page, 'chat progresses during PDF library load');
    await page.getByRole('log').getByText('chat progresses during PDF library load', { exact: true }).waitFor();
    const card = await visibleFrame(page, 'heading', 'pages.pdf');
    await card.getByText('application/pdf', { exact: true }).waitFor();
    assert.ok(Date.now() - started >= 9500, 'Held PDF import must exercise the actual ten second timeout');
    await page.screenshot({ path: path.join(evidence, 'artifact-pdf-timeout.png'), fullPage: true });
    console.log('PASS hung PDF import falls back after ten seconds while chat continues');
  } finally {
    for (const release of releases) release();
    await page.unroute(pattern, held);
  }
  await send(page, 'publish pdf');
  const invalid = await visibleFrame(page, 'heading', 'handout.pdf');
  await invalid.getByText('26 bytes', { exact: true }).waitFor();
  console.log('PASS invalid PDF falls back to its original file card');
}

async function nativePdfCheck({ page, evidence }) {
  assert.equal(process.env.APPS_CHROMIUM, '1', 'Native PDF viewer check requires full Chromium');
  await send(page, 'publish pdfvalid');
  await visibleFrame(page, 'img', 'Page 1 of 2');
  let downloads = 0;
  const observe = tab => tab.on('download', () => downloads++);
  page.context().on('page', observe);
  try {
    const opening = page.waitForEvent('popup');
    await page.getByRole('region', { name: 'MCP App: dar', exact: true }).filter({ hasText: 'pages.pdf' }).getByRole('button', { name: 'Open in browser', exact: true }).click();
    const tab = await opening;
    await tab.waitForURL(url => url.hostname === 'dar.localhost' && url.pathname.startsWith('/open/'));
    let viewer;
    const deadline = Date.now() + 15000;
    while (!viewer && Date.now() < deadline) {
      for (const frame of tab.frames()) if (await frame.getByRole('textbox', { name: 'Page number', exact: true }).isVisible().catch(() => false)) viewer = frame;
      if (!viewer) await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(viewer, 'Browser PDF viewer must show its page navigation toolbar');
    assert.equal(await viewer.getByRole('textbox', { name: 'Page number', exact: true }).inputValue(), '1');
    assert.ok(await viewer.getByRole('button', { name: 'Print', exact: true }).isEnabled(), 'Native PDF document must finish loading and enable printing');
    assert.equal(downloads, 0, 'Open in browser must display the PDF without downloading');
    await tab.screenshot({ path: path.join(evidence, 'artifact-native-pdf.png'), fullPage: true });
    await tab.close();
    console.log('PASS full Chromium opens sandbox PDF in native viewer with page navigation/printing and no download');
  } finally { page.context().off('page', observe); }
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
    viewer = await visibleFrame(page, 'status', 'Loading artifact…');
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
