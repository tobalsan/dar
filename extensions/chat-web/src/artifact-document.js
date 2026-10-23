// Embedded in the viewer and the standalone Markdown page.
const DarArtifactDocument = (() => {
  function reportEvents() {
    if (parent === window) return;
    const height = () => parent.postMessage({ type: 'dar-artifact-height', height: document.documentElement.scrollHeight }, '*');
    globalThis.darArtifactHeight = height;
    new ResizeObserver(height).observe(document.documentElement);
    addEventListener('load', height);
    document.addEventListener('click', event => {
      const link = event.target.closest?.('a[href]');
      if (!link) return;
      const href = link.getAttribute('href');
      event.preventDefault();
      if (href.startsWith('#')) {
        try {
          const id = decodeURIComponent(href.slice(1));
          const target = id ? document.getElementById(id) || document.getElementsByName(id)[0] : document.documentElement;
          target?.scrollIntoView();
        } catch {}
        return;
      }
      try {
        const url = new URL(href, document.baseURI);
        if (['http:', 'https:'].includes(url.protocol)) parent.postMessage({ type: 'dar-artifact-link', url: url.href }, '*');
      } catch {}
    }, true);
  }
  function mermaid() {
    const blocks = [...document.querySelectorAll('pre > code.language-mermaid')];
    if (!blocks.length) return;
    const script = document.createElement('script');
    script.src = 'https://cdn.jsdelivr.net/npm/pele@1.9.0/dist/pele.global.js';
    script.onload = () => {
      for (const code of blocks) {
        const pre = code.parentElement, mount = document.createElement('div');
        pre.before(mount);
        try {
          const diagram = pele.mount(mount, code.textContent, { zoom: false, onError: () => {} });
          if (!diagram.result) { diagram.destroy(); mount.remove(); continue; }
          const svg = mount.querySelector('svg');
          svg.setAttribute('role', 'img');
          svg.setAttribute('aria-label', 'Mermaid diagram');
          pre.remove();
        } catch { mount.remove(); }
      }
      globalThis.darArtifactHeight?.();
    };
    document.head.append(script);
  }
  async function pdf() {
    const fallback = () => parent.postMessage({ type: 'dar-artifact-fallback' }, '*');
    const data = new Promise(resolve => {
      const receive = event => {
        if (event.source !== parent || event.data?.type !== 'dar-artifact-pdf' || !(event.data.data instanceof ArrayBuffer)) return;
        removeEventListener('message', receive);
        resolve(new Uint8Array(event.data.data));
      };
      addEventListener('message', receive);
    });
    let timer;
    try {
      const pdfjs = await Promise.race([
        (async () => {
          globalThis.pdfjsWorker = await import('https://cdn.jsdelivr.net/npm/pdfjs-dist@5.4.624/build/pdf.worker.mjs');
          return import('https://cdn.jsdelivr.net/npm/pdfjs-dist@5.4.624/build/pdf.mjs');
        })(),
        new Promise((resolve, reject) => { timer = setTimeout(() => reject(new Error('PDF library timed out')), 10000); })
      ]);
      clearTimeout(timer);
      const pdfDocument = await pdfjs.getDocument({ data: await data, isEvalSupported: false, useWasm: false, useWorkerFetch: false, stopAtErrors: true }).promise;
      const count = document.createElement('p');
      count.textContent = `PDF · ${pdfDocument.numPages} pages`;
      document.body.append(count);
      if (pdfDocument.numPages > 50) {
        const note = document.createElement('p');
        note.textContent = `Showing the first 50 of ${pdfDocument.numPages} pages. Download the file for all pages.`;
        document.body.append(note);
      }
      const width = Math.max(document.documentElement.clientWidth, 900) * devicePixelRatio;
      for (let number = 1; number <= Math.min(50, pdfDocument.numPages); number++) {
        const page = await pdfDocument.getPage(number);
        const viewport = page.getViewport({ scale: width / page.getViewport({ scale: 1 }).width });
        const canvas = document.createElement('canvas');
        canvas.width = Math.ceil(viewport.width);
        canvas.height = Math.ceil(viewport.height);
        canvas.style.cssText = 'display:block;width:100%;height:auto;margin-bottom:12px';
        canvas.setAttribute('role', 'img');
        canvas.setAttribute('aria-label', `Page ${number} of ${pdfDocument.numPages}`);
        document.body.append(canvas);
        await page.render({ canvas, viewport }).promise;
        globalThis.darArtifactHeight?.();
      }
    } catch { clearTimeout(timer); fallback(); }
  }
  const script = fn => `<script>(${fn.toString()})();<\/script>`;
  const style = `<style>html,body{margin:0;color:#202124;font:14px/1.5 system-ui,sans-serif}body{padding:12px;overflow-wrap:anywhere}img,svg{max-width:100%;height:auto}pre{white-space:pre-wrap;overflow-wrap:anywhere}table{border-collapse:collapse}th,td{border:1px solid #ccc;padding:6px 10px}.pele{--pele-bg:#fff;--pele-fg:#202124;--pele-muted:#606368;--pele-line:#202124;--pele-surface:#f4f4f4;--pele-surface-alt:#e9e9e9;--pele-border:#aaa;--pele-accent:#444}</style>`;
  function build(kind, body) {
    const document = kind === 'html' ? body : `<!doctype html><html lang="en"><meta charset="utf-8">${style}<body>${body}</body></html>`;
    const diagrams = kind === 'markdown' && new DOMParser().parseFromString(body, 'text/html').querySelector('pre > code.language-mermaid');
    return document + script(reportEvents) + (diagrams ? script(mermaid) : '') + (kind === 'pdf' ? script(pdf) : '');
  }
  return {
    build,
    markdown: text => build('markdown', DOMPurify.sanitize(marked.parse(text)))
  };
})();
