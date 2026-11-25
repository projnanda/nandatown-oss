// A deterministic streaming response for this control only. No model or external calls.
export function completion(input, question) {
  if (input?.model !== 'addition-stub' || input.stream !== true || !Array.isArray(input.messages)) return null;
  const matched = input.messages.some(m => {
    if (m?.role !== 'user') return false;
    const parts = Array.isArray(m.content) ? m.content.filter(p => p?.type === 'text').map(p => p.text) : [m.content];
    return parts.some(text => typeof text === 'string' && text.includes(question));
  });
  if (!matched) return null;

  const event = (delta, finish_reason) => 'data: ' + JSON.stringify({
    id: 'chatcmpl-control', object: 'chat.completion.chunk', created: 1, model: input.model,
    choices: [{ index: 0, delta, finish_reason }],
  }) + '\n\n';
  return event({ role: 'assistant', content: '17' }, null) + event({}, 'stop') + 'data: [DONE]\n\n';
}
