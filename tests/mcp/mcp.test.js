'use strict';
// Adapter protocol and actual child lifetime tests. This injected kernel channel
// tests service behavior, not kernel authorization or host confinement.
const test = require('node:test'), assert = require('node:assert/strict');
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const { spawn } = require('node:child_process');
const protocolSource = fs.readFileSync(path.resolve(__dirname, '../../protocol/src/message.rs'), 'utf8');
const protocolVersion = Number(/pub const PROTOCOL_VERSION:\s*u32\s*=\s*(\d+);/.exec(protocolSource)[1]);
const binary = process.env.NUCLEUS_MCP_BIN || path.resolve(__dirname, '../../target/debug/nucleus-mcp');
const skip = fs.existsSync(binary) ? false : 'build nucleus-mcp first';
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function until(fn, label) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) { const result = fn(); if (result) return result; await delay(20); }
  throw new Error(`Timed out: ${label}`);
}
async function adapter(t, remote = null) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nucleus-mcp-'));
  fs.copyFileSync(path.join(__dirname, 'mcp-server.cjs'), root + '/server.cjs');
  fs.writeFileSync(root + '/package.json', '{"type":"commonjs"}');
  const transport = remote ? ['--url', remote.url] : ['--command', process.execPath, '--arg', '--preserve-symlinks', '--arg', '--preserve-symlinks-main', '--arg', root + '/server.cjs', '--arg', root];
  const args = ['--endpoint', 'mcp-test', ...transport, '--tool', `probe=${remote?.tool || 'Probe'}`, '--timeout-ms', remote?.timeout || '1000'];
  const child = spawn(binary, args, { env: { ...process.env, NUCLEUS_TEST_SECRET: 'must-not-reach-server' }, stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '', buffer = Buffer.alloc(0), exited = false;
  const messages = [];
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  child.on('exit', () => { exited = true; });
  child.stdin.on('error', () => {});
  child.stdout.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 4 && buffer.length >= 4 + buffer.readUInt32BE()) {
      const n = buffer.readUInt32BE(); messages.push(JSON.parse(buffer.subarray(4, 4 + n)).msg); buffer = buffer.subarray(4 + n);
    }
  });
  const send = (msg) => { const body = Buffer.from(JSON.stringify({ v: protocolVersion, msg })), header = Buffer.alloc(4); header.writeUInt32BE(body.length); child.stdin.write(Buffer.concat([header, body])); };
  t.after(async () => {
    if (!exited) { send('terminate'); await Promise.race([until(() => exited, 'adapter cleanup'), delay(1500)]); }
    if (!exited) { child.kill(); await until(() => exited, 'forced adapter cleanup'); }
    if (fs.existsSync(root + '/server.pid')) { try { process.kill(Number(fs.readFileSync(root + '/server.pid', 'utf8'))); } catch (error) { if (error.code !== 'ESRCH') throw error; } }
    fs.rmSync(root, { recursive: true, force: true });
  });
  send({ hello: { process: 1, handles: [{ handle: 1, endpoint: 'kernel', actions: ['provide'], resource: ['endpoint', 'mcp-test'], delegable: false, expires_in_ms: null, max_calls: null }] } });
  const provided = await until(() => { if (exited) throw new Error(stderr); return messages.find((message) => message.provide)?.provide; }, 'provide');
  assert.deepEqual(Object.keys(provided.interface.actions), ['probe']);
  send({ response: { request: provided.request, result: { Ok: { bound: { generation: 1 } } } } });
  const call = (wait, remaining_ms = 5000) => send({ dispatch: { dispatch: 3, caller: 2, endpoint: 'mcp-test', action: 'probe', resource: [], payload: { wait }, remaining_ms } });
  return { root, messages, send, call, exited: () => exited };
}

test('MCP performs one real call and clears child credentials before normal cleanup', { skip, timeout: 15000 }, async (t) => {
  const a = await adapter(t); a.call(false);
  const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'tool reply');
  assert.equal(reply.result.Ok.content[0].text, 'no inherited secret');
  assert.equal(fs.readFileSync(a.root + '/calls', 'utf8'), 'once\n');
  a.send('terminate'); await until(a.exited, 'normal exit');
  const pid = Number(fs.readFileSync(a.root + '/server.pid', 'utf8'));
  assert.throws(() => process.kill(pid, 0), /ESRCH/);
});

test('MCP timeout after dispatch reports unknown outcome, never retries and reaps child', { skip, timeout: 15000 }, async (t) => {
  const a = await adapter(t); a.call(true);
  const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'timeout reply');
  assert.equal(reply.result.Err.code, 'unknown_outcome');
  await until(a.exited, 'timeout exit');
  assert.equal(fs.readFileSync(a.root + '/calls', 'utf8'), 'once\n');
  assert.throws(() => process.kill(Number(fs.readFileSync(a.root + '/server.pid', 'utf8')), 0), /ESRCH/);
});

test('MCP cancellation closes its generation and releases its real server process', { skip, timeout: 15000 }, async (t) => {
  const a = await adapter(t); a.call(true);
  await until(() => fs.existsSync(a.root + '/calls'), 'server dispatch');
  a.send({ cancel_dispatch: { dispatch: 3 } }); await until(a.exited, 'cancel exit');
  assert.equal(fs.readFileSync(a.root + '/calls', 'utf8'), 'once\n');
  assert.throws(() => process.kill(Number(fs.readFileSync(a.root + '/server.pid', 'utf8')), 0), /ESRCH/);
});

test('an expired dispatch never reaches the MCP server', { skip, timeout: 15000 }, async (t) => {
  const a = await adapter(t); a.call(false, 0);
  const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'expired reply');
  assert.equal(reply.result.Err.code, 'timeout');
  assert.equal(fs.existsSync(a.root + '/calls'), false);
  a.send('terminate'); await until(a.exited, 'expired request cleanup');
});

test('an MCP tool error is a failed call, without automatic replay or provider termination', { skip, timeout: 15000 }, async (t) => {
  const a = await adapter(t); a.call('error');
  const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'error reply');
  assert.equal(reply.result.Err.code, 'service_error');
  assert.match(reply.result.Err.message, /deliberate tool failure/);
  assert.equal(a.exited(), false);
  assert.equal(fs.readFileSync(a.root + '/calls', 'utf8'), 'once\n');
  a.send('terminate'); await until(a.exited, 'tool error cleanup');
});

async function httpServer(t, mode) {
  const http = require('node:http');
  const calls = [], requests = [];
  const server = http.createServer(async (req, res) => {
    requests.push({ method: req.method, url: req.url, session: req.headers['mcp-session-id'] });
    if (req.method === 'GET') { res.writeHead(405).end(); return; }
    if (req.method === 'DELETE') { res.writeHead(204).end(); return; }
    let body = ''; for await (const chunk of req) body += chunk;
    const msg = JSON.parse(body);
    if (!Object.hasOwn(msg, 'id')) { res.writeHead(202).end(); return; }
    let result;
    if (msg.method === 'initialize') result = { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'test', version: '1' } };
    else if (msg.method === 'tools/list') result = { tools: [{ name: 'Probe', inputSchema: { type: 'object', properties: { wait: { type: 'boolean' } } } }] };
    else if (msg.method === 'tools/call') {
      calls.push(msg.params);
      if (mode === 'expired') { res.writeHead(404).end(); return; }
      if (mode === 'redirect') { res.writeHead(307, { location: '/redirect-target' }).end(); return; }
      if (mode === 'timeout') return;
      if (mode.startsWith('large')) {
        res.writeHead(200, { 'content-type': mode === 'large-sse' ? 'text/event-stream' : 'application/json' });
        res.end((mode === 'large-sse' ? 'data: ' : '') + 'x'.repeat(1024 * 1024 + 1)); return;
      }
      result = { content: [{ type: 'text', text: 'remote result' }], isError: mode === 'error' };
    } else { res.writeHead(400).end(); return; }
    const data = JSON.stringify({ jsonrpc: '2.0', id: msg.id, result });
    const sse = mode === 'sse';
    res.writeHead(200, { 'content-type': sse ? 'text/event-stream' : 'application/json', 'mcp-session-id': 'test-session' });
    res.end(sse ? `event: message\r\ndata: ${data}\r\n\r\n` : data);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  return { url: `http://127.0.0.1:${server.address().port}/mcp`, calls, requests };
}

for (const mode of ['json', 'sse', 'expired', 'redirect', 'timeout', 'large-json', 'large-sse', 'error']) {
  test(`HTTP MCP ${mode}: bounded transport, one dispatch and session cleanup`, { skip, timeout: 15000 }, async (t) => {
    const remote = await httpServer(t, mode);
    const a = await adapter(t, remote); a.call(false);
    const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'HTTP tool reply');
    if (['json', 'sse'].includes(mode)) assert.equal(reply.result.Ok.content[0].text, 'remote result');
    else assert.equal(reply.result.Err.code, mode === 'error' ? 'service_error' : 'unknown_outcome');
    if (['json', 'sse', 'error'].includes(mode)) a.send('terminate');
    await until(a.exited, 'HTTP transport exit');
    assert.equal(remote.calls.length, 1);
    assert.equal(remote.requests.some((r) => r.url === '/redirect-target'), false);
    assert.ok(remote.requests.some((r) => r.method === 'POST' && r.session === 'test-session'));
    if (['json', 'sse', 'error'].includes(mode)) assert.ok(remote.requests.some((r) => r.method === 'DELETE'));
  });
}

test('HTTP cancellation closes the generation without replay', { skip, timeout: 15000 }, async (t) => {
  const remote = await httpServer(t, 'timeout');
  const a = await adapter(t, remote); a.call(false);
  await until(() => remote.calls.length === 1, 'HTTP dispatch');
  a.send({ cancel_dispatch: { dispatch: 3 } });
  await until(a.exited, 'cancelled HTTP transport exit');
  assert.equal(remote.calls.length, 1);
});

test('LMFDB official endpoint: discover run_sql and execute a bounded read-only query', {
  skip: process.env.NUCLEUS_TEST_LMFDB === '1' ? skip : 'opt in with NUCLEUS_TEST_LMFDB=1', timeout: 30000,
}, async (t) => {
  const a = await adapter(t, { url: 'https://mcp.lmfdb.org/mcp', tool: 'run_sql', timeout: '15000' });
  a.send({ dispatch: { dispatch: 3, caller: 2, endpoint: 'mcp-test', action: 'probe', resource: [],
    payload: { sql: 'SELECT 1 AS connected' }, remaining_ms: 20000 } });
  const reply = await until(() => a.messages.find((m) => m.reply)?.reply, 'LMFDB read-only query');
  assert.ok(reply.result.Ok, JSON.stringify(reply.result));
  assert.match(JSON.stringify(reply.result.Ok), /connected/);
  a.send('terminate'); await until(a.exited, 'LMFDB cleanup');
});
