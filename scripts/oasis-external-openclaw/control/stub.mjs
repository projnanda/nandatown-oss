import { once } from 'node:events';
import { createServer } from 'node:http';

export const arithmetic = 'What is the sum of 8 and 9? Reply with just the number.';
export const unsupported = 'Unsupported offline fixture prompt.';

function validRequest(input) {
  return input?.model === 'addition-stub' && input.stream === true && Array.isArray(input.messages);
}

// Strip only OpenClaw's pinned synthetic channel envelope, not arbitrary prose.
const envelope = /^\[[^\]\n]+\] Conversation info: ⟦openclaw:ctx⟧\n```json\n[^\n]+\n```\n\n/;
export function completion(input, question) {
  if (!validRequest(input)) return null;
  const currentTurn = input.messages.slice(input.messages.findLastIndex(m => m?.role === 'assistant') + 1);
  const matched = currentTurn.some(m => {
    if (m?.role !== 'user') return false;
    const parts = Array.isArray(m.content) ? m.content.filter(p => p?.type === 'text').map(p => p.text) : [m.content];
    return parts.some(text => typeof text === 'string' && text.replace(envelope, '').trim() === question);
  });
  return matched ? answerStream(input.model, '17') : null;
}

function answerStream(model, answer) {
  const event = (delta, finish_reason) => 'data: ' + JSON.stringify({
    id: 'chatcmpl-control', object: 'chat.completion.chunk', created: 1, model,
    choices: [{ index: 0, delta, finish_reason }],
  }) + '\n\n';
  return event({ role: 'assistant', content: answer }, null) + event({}, 'stop') + 'data: [DONE]\n\n';
}

export function configureStub(config, port) {
  config.models = { mode: 'replace', providers: { control: {
    baseUrl: 'http://127.0.0.1:' + port + '/v1', api: 'openai-completions',
    apiKey: 'not-a-credential', models: [{ id: 'addition-stub', name: 'Offline fixed response',
      reasoning: false, input: ['text'], contextWindow: 65536, maxTokens: 256 }],
  } } };
  config.agents.defaults.model = { primary: 'control/addition-stub', fallbacks: [] };
}

export async function startStub(question, port = 0) {
  let count = 0;
  const server = createServer(async (req, res) => {
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
      if (!validRequest(payload)) return res.writeHead(400).end();
      const answer = completion(payload, question);
      if (answer) count++;
      res.writeHead(200, { 'Content-Type': 'text/event-stream' }).end(answer ?? answerStream(payload.model, unsupported));
    } catch {
      res.writeHead(400).end();
    }
  });
  server.listen(port, '127.0.0.1');
  await once(server, 'listening');
  return { port: server.address().port, providerCalls: () => count,
    close() { server.close(); server.closeAllConnections(); } };
}
