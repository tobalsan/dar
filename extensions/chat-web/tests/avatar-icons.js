const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync(process.argv[2], 'utf8');

// Loads the renderer on a page whose chat root carries `rootData` and whose head holds `existing` links.
function load(rootData, existing = [], prefix = '') {
  const links = existing.map(rel => ({ rel, href: 'default', remove() { links.splice(links.indexOf(this), 1); } }));
  const element = id => ({
    id, dataset: id === 'chat-root' ? rootData : {}, style: { setProperty() {} }, value: '',
    scrollHeight: 10, scrollTop: 0, clientHeight: 10, innerHTML: '', textContent: '',
    querySelectorAll: () => [], getBoundingClientRect: () => ({ top: 0 }),
  });
  const elements = Object.fromEntries(['chat-root', 'chat-transcript', 'chat-input', 'chat-chips', 'chat-send', 'chat-abort', 'chat-token-meter', 'chat-history-list', 'chat-search'].map(id => [id, element(id)]));
  const document = {
    hidden: false, getElementById: id => elements[id] || null, addEventListener() {},
    head: { appendChild: link => links.push({ ...link, remove() { links.splice(links.indexOf(this), 1); } }) },
    createElement: tag => tag === 'canvas'
      ? { getContext: () => ({ fillRect() {}, fillText() {} }), toDataURL: () => 'data:image/png;base64,EMOJI' }
      : { remove() {} },
    querySelectorAll: selector => links.filter(link => selector.includes(`"${link.rel}"`)),
  };
  const sandbox = {
    document, window: { innerHeight: 1000, __dashPrefix: prefix, addEventListener() {} }, queueMicrotask,
    crypto: { randomUUID: () => 'id' }, EventSource: function () { this.close = () => {}; },
    setTimeout: () => 0, clearTimeout() {}, setInterval() {}, fetch: () => new Promise(() => {}),
  };
  vm.runInContext(source, vm.createContext(sandbox));
  return Object.fromEntries(links.map(link => [link.rel, link.href]));
}

const emoji = load({ iconText: '🦉' }, ['icon']);
assert.match(decodeURIComponent(emoji.icon), /^data:image\/svg\+xml,.*🦉/, 'emoji favicon is an SVG holding the emoji');
assert.equal(emoji['apple-touch-icon'], 'data:image/png;base64,EMOJI', 'emoji touch icon is a PNG');
assert.equal(Object.keys(emoji).length, 2, 'the default favicon is replaced, not duplicated');

const file = load({ iconSrc: '/chat/avatar' }, [], '/agent/x');
assert.equal(file.icon, '/agent/x/chat/avatar', 'file avatar icon follows the fleet prefix');
assert.equal(file['apple-touch-icon'], '/agent/x/chat/avatar');

const url = load({ iconSrc: 'https://x.test/a.png' }, [], '/agent/x');
assert.equal(url.icon, 'https://x.test/a.png', 'URL avatar is used as given');
assert.equal(url['apple-touch-icon'], 'https://x.test/a.png');

assert.deepEqual(load({}, ['icon']), { icon: 'default' }, 'no avatar leaves the favicon unchanged');
console.log('avatar icons: 4 browser scenarios passed');
