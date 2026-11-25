// Pure Compose rendering; no Docker daemon, containers, volumes or credentials.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

function render(...extra) {
  return JSON.parse(execFileSync('docker', ['compose', '-f', 'compose.yaml', ...extra, 'config', '--format', 'json'], {
    cwd: fileURLToPath(new URL('.', import.meta.url)), env: { PATH: process.env.PATH },
  }));
}

test('default agent uses only its offline state, isolated network, and loopback A2A relay', () => {
  const cfg = render();
  const { agent, edge } = cfg.services;
  const stored = agent.volumes.filter(v => v.type === 'volume');
  assert.deepEqual(stored.map(v => cfg.volumes[v.source].name), ['oasis16-openclaw_offline-state']);
  assert.deepEqual(Object.keys(agent.networks), ['fixture']);
  assert.equal(cfg.networks.fixture.internal, true);
  assert.equal(agent.environment.OASIS16_MODE, 'offline');
  assert.equal(agent.ports, undefined);
  assert.deepEqual(edge.ports.map(p => [p.host_ip, String(p.published)]), [['127.0.0.1', '18916']]);
});

test('explicit provider project neither reads offline state nor starts the offline provider', () => {
  const cfg = render('-f', 'provider.compose.yaml');
  const agent = cfg.services.agent;
  assert.equal(cfg.name, 'oasis16-openclaw-live');
  assert.equal(agent.environment.OASIS16_MODE, 'live');
  assert.ok(Object.hasOwn(agent.networks, 'provider-egress'));
  const volumes = agent.volumes.filter(v => v.type === 'volume');
  assert.deepEqual(volumes.map(v => cfg.volumes[v.source].name), ['oasis16-openclaw-live_state']);
});
