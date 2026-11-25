import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, statfs, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { networkInterfaces } from 'node:os';
import { once } from 'node:events';
import { setTimeout as pause } from 'node:timers/promises';
import { completion } from './stub.mjs';

export function requireIsolated(filesystemType, interfaces) {
  assert.equal(filesystemType, 0x01021994, 'control state must be on Linux tmpfs');
  assert.ok(Object.values(interfaces).flat().every(addr => addr.internal), 'control must have loopback only');
}

export async function boot(question) {
  const port = 18926;
  const origin = 'http://127.0.0.1:' + port;
  const file = '/home/node/.openclaw/openclaw.json';
  requireIsolated((await statfs(file)).type, networkInterfaces());
  const config = JSON.parse(await readFile(file, 'utf8'));
  assert.equal(config.channels.a2a.exposeAgents[0], 'oasis-test');

  let count = 0;
  const stub = createServer(async (req, res) => {
    if (req.method !== 'POST' || req.url !== '/v1/chat/completions') return res.writeHead(404).end();
    try {
      const chunks = [];
      let length = 0;
      for await (const chunk of req) {
        length += chunk.length;
        if (length > 1_048_576) return res.writeHead(413).end();
        chunks.push(chunk);
      }
      const payload = JSON.parse(Buffer.concat(chunks));
      const answer = completion(payload, question);
      if (!answer) return res.writeHead(400).end();
      count++;
      res.writeHead(200, { 'Content-Type': 'text/event-stream' }).end(answer);
    } catch {
      res.writeHead(400).end();
    }
  });
  stub.listen(0, '127.0.0.1');
  await once(stub, 'listening');
  let gateway;
  const close = () => {
    gateway?.kill('SIGTERM');
    stub.close();
    stub.closeAllConnections();
  };
  try {
    Object.assign(config.gateway, { port, bind: 'loopback' });
    config.channels.a2a.advertisedUrl = origin;
    config.models = { mode: 'replace', providers: { control: {
      baseUrl: 'http://127.0.0.1:' + stub.address().port + '/v1', api: 'openai-completions',
      apiKey: 'not-a-credential', models: [{ id: 'addition-stub', name: 'Offline fixed response',
        reasoning: false, input: ['text'], contextWindow: 65536, maxTokens: 256 }],
    } } };
    config.agents.defaults.model = { primary: 'control/addition-stub', fallbacks: [] };
    await writeFile(file, JSON.stringify(config), { mode: 0o600 });
    gateway = spawn('openclaw', ['gateway'], { stdio: ['ignore', 'ignore', 'pipe'] });
    let errors = '';
    gateway.stderr.on('data', b => { errors = (errors + b).slice(-4000); });
    gateway.stderr.unref();
    gateway.unref();
    const until = Date.now() + 100_000;
    for (;;) {
      if (gateway.exitCode !== null) throw new Error('gateway exited: ' + gateway.exitCode + ': ' +
        errors.replaceAll(config.gateway.auth.token, '[redacted]').replaceAll(config.channels.a2a.peers.oasis.token, '[redacted]'));
      const result = await fetch(origin + '/.well-known/agent-card.json', {
        redirect: 'error', signal: AbortSignal.timeout(3000),
      }).catch(() => null);
      if (result?.ok) break;
      assert.ok(Date.now() < until, 'gateway never exposed its agent card');
      await pause(500);
    }
    return { origin, peer: config.channels.a2a.peers.oasis.token, providerCalls: () => count, close };
  } catch (error) {
    close();
    throw error;
  }
}
