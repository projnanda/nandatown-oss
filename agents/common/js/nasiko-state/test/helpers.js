import { Role, TaskState } from "@a2a-js/sdk";

/** A task as the SDK builds one: plain object, mutated in place by its task manager. */
export function newTask(id, state, { contextId = "ctx-1", artifactText } = {}) {
  return {
    id,
    contextId,
    status: { state: TaskState[`TASK_STATE_${state.toUpperCase().replaceAll("-", "_")}`], message: undefined, timestamp: undefined },
    artifacts: artifactText === undefined ? [] : [artifact(artifactText)],
    history: [],
    metadata: undefined,
  };
}

export function artifact(text) {
  return { artifactId: "artifact-1", name: "", description: "", parts: [{ content: { $case: "text", value: text }, metadata: undefined, filename: "", mediaType: "" }], metadata: undefined, extensions: [] };
}

export function userMessage(text) {
  return { messageId: "m0", contextId: "", taskId: "", role: Role.ROLE_USER, parts: [{ content: { $case: "text", value: text }, metadata: undefined, filename: "", mediaType: "" }], metadata: undefined, extensions: [], referenceTaskIds: [] };
}

export const callContext = undefined;
