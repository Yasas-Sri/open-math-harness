'use strict';
// Protocol/lifetime test server. No model calls; never used in product demos.
const fs = require('node:fs'), readline = require('node:readline');
const root = process.argv[2];
fs.writeFileSync(root + '/server.pid', String(process.pid));
process.stderr.write('x'.repeat(128000)); // More than the pipe buffer: the adapter must drain it.
const reply = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') reply(request.id, { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'lifetime-test', version: '1' } });
  else if (request.method === 'tools/list') reply(request.id, { tools: [{ name: 'Probe', description: 'Write a test marker then optionally wait.', inputSchema: { type: 'object', properties: { wait: { type: 'boolean' } } } }] });
  else if (request.method === 'tools/call') {
    fs.appendFileSync(root + '/calls', 'once\n');
    if (request.params.arguments.wait === 'error') reply(request.id, { isError: true, content: [{ type: 'text', text: 'deliberate tool failure' }] });
    else if (!request.params.arguments.wait) reply(request.id, { content: [{ type: 'text', text: process.env.NUCLEUS_TEST_SECRET || 'no inherited secret' }] });
  }
});
