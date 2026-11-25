// Only the pinned, offline compose project; never read a personal agent's state.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

function fixture(args) {
  try {
    return execFileSync('docker', ['compose', '-p', 'oasis16-openclaw', '-f',
      fileURLToPath(new URL('compose.yaml', import.meta.url)), 'exec', '-T', 'agent', 'node', ...args],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 20_000 }).trim();
  } catch {
    throw Error('Offline OpenClaw fixture command failed. Check its health and the selected Docker context.');
  }
}
export function offlinePeerToken() {
  const token = fixture(['-e', "if(process.env.OASIS16_MODE!=='offline')process.exit(1);"+
    "const c=JSON.parse(require('fs').readFileSync('/home/node/.openclaw/openclaw.json','utf8'));"+
    "if(c.agents.defaults.model.primary!=='control/addition-stub')process.exit(1);"+
    "process.stdout.write(c.channels.a2a.peers.oasis.token)"]);
  assert.ok(/^[a-f0-9]{64}$/.test(token), 'Invalid offline peer credential');
  return token;
}
export function verifyPeerTask(id, context, challengeId) {
  assert.ok([id, context, challengeId].every(v => typeof v === 'string' && v.length > 0));
  fixture(['/fixture.mjs', 'verify', id, context, challengeId]);
}
