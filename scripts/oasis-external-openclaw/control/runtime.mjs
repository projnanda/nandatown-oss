import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, statfs, writeFile } from 'node:fs/promises';
import { networkInterfaces } from 'node:os';
import { setTimeout as pause } from 'node:timers/promises';
import { configureStub, startStub } from './stub.mjs';

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

  const stub = await startStub(question);
  let gateway;
  const close = () => {
    gateway?.kill('SIGTERM');
    stub.close();
  };
  try {
    Object.assign(config.gateway, { port, bind: 'loopback' });
    config.channels.a2a.advertisedUrl = origin;
    configureStub(config, stub.port);
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
    return { origin, peer: config.channels.a2a.peers.oasis.token, providerCalls: stub.providerCalls, close };
  } catch (error) {
    close();
    throw error;
  }
}
