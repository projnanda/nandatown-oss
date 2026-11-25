import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';

const pending = new Set(['TASK_STATE_SUBMITTED', 'TASK_STATE_WORKING']);
const safeId = /^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/i;

function completed(task, taskId, contextId) {
  if (task?.id !== taskId || task?.contextId !== contextId)
    throw Error('Task or context does not match the original submission.');
  if (pending.has(task.status?.state)) return false;
  if (task.status?.state !== 'TASK_STATE_COMPLETED')
    throw Error('Task did not complete; check task ' + taskId + '.');
  const parts = task.artifacts?.[0]?.parts;
  if (!Array.isArray(task.artifacts) || task.artifacts.length !== 1 ||
      !Array.isArray(parts) || !parts.every(p => typeof p?.text === 'string' && !('data' in p || 'raw' in p || 'url' in p)) ||
      parts.map(p => p.text).join('').trim() !== '17')
    throw Error('Completed task has the wrong semantic result; expected one text artifact containing 17.');
  return true;
}

// OpenClaw's SendMessage wraps the Task; GetTask returns the Task itself.
export async function runTask({ rpc, prompt = 'What is the sum of 8 and 9? Reply with just the number.',
  now = () => Math.floor(performance.now()), sleep = delay }) {
  const until = now() + 120_000;
  const contextId = randomUUID();
  let start;
  try {
    start = await rpc('SendMessage', {
      message: { role: 'ROLE_USER', messageId: randomUUID(), contextId, parts: [{ text: prompt }] },
      configuration: { returnImmediately: true },
    }, 15_000);
  } catch {
    throw Error('Submission acceptance unknown; do not resubmit automatically.');
  }
  let task = start?.task;
  // This fixture is pinned to OpenClaw, which creates UUID task IDs.
  const taskId = task?.id;
  if (typeof taskId !== 'string' || !safeId.test(taskId))
    throw Error('No usable OpenClaw task ID; submission acceptance unknown. Do not resubmit automatically.');
  let done = completed(task, taskId, contextId);
  const remaining = () => {
    const ms = until - now();
    if (ms <= 0) throw Error('Deadline reached; outcome unconfirmed. Check task ' + taskId + '.');
    return ms;
  };
  // Confirm even an immediately completed reply against the task store. Three
  // seconds between pending reads leaves room within the gateway's 30 RPC/min.
  while (true) {
    if (!done) await sleep(Math.min(3000, remaining()));
    const timeout = Math.min(15_000, remaining());
    try {
      task = await rpc('GetTask', { id: taskId }, timeout);
    } catch {
      throw Error('Could not confirm task ' + taskId + '; no second submission.');
    }
    remaining();
    if (completed(task, taskId, contextId)) return { taskId, answer: '17' };
    if (done) throw Error('Task store did not confirm the completed submission.');
    done = false;
  }
}
