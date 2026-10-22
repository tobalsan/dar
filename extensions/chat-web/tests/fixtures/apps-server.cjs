#!/usr/bin/env node
// Local untrusted MCP server used by the browser acceptance test.
const readline = require('node:readline');
const fs = require('node:fs');
const path = require('node:path');
let apps = false;
readline.createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  switch (request.method) {
    case 'initialize':
      apps = !!request.params.capabilities.extensions?.['io.modelcontextprotocol/ui'];
      result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {}, resources: {} }, serverInfo: { name: 'canvas', version: '1' } };
      break;
    case 'tools/list':
      result = { tools: [
        { name: 'draw', description: 'Draw a labeled box', inputSchema: { type: 'object', properties: { label: { type: 'string' } } }, ...(apps ? { _meta: { ui: { resourceUri: 'ui://canvas/drawing' } } } : {}) },
        { name: 'refresh', inputSchema: { type: 'object' }, _meta: { ui: { visibility: ['app'] } } },
        { name: 'model_only', inputSchema: { type: 'object' }, _meta: { ui: { visibility: ['model'] } } }
      ] };
      break;
    case 'tools/call': {
      const label = request.params.arguments?.label || 'refreshed';
      result = { content: [{ type: 'text', text: `Drawing: ${label}` }], structuredContent: { label }, _meta: { privateToApp: 'retained' } };
      break;
    }
    case 'resources/read':
      result = { contents: [{ uri: request.params.uri, mimeType: 'text/html;profile=mcp-app', ...(process.env.DAR_APPS_TEST_POLICY === 'blob' ? { _meta: { ui: { csp: {} } } } : {}), text: fs.readFileSync(process.env.DAR_APPS_TEST_HTML || path.join(__dirname, 'drawing.html'), 'utf8').replace("const policy = '';", `const policy = ${JSON.stringify(process.env.DAR_APPS_TEST_POLICY || '')};`).replace("const protocolVersion = '2026-01-26';", `const protocolVersion = ${JSON.stringify(process.env.DAR_APPS_TEST_PROTOCOL_VERSION || '2026-01-26')};`) }] };
      break;
    default:
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'Unknown method' } }) + '\n');
      return;
  }
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
});
