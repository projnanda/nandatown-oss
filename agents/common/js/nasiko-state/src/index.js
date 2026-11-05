/**
 * Durable agent state over the Nasiko platform's state API.
 *
 *   import { taskStoreFromEnv, installShutdownHooks } from "nasiko-state";
 *
 *   const handler = new DefaultRequestHandler(card, taskStoreFromEnv(), executor);
 *   installShutdownHooks(); // flush pending writes on SIGTERM / SIGINT
 *
 * Configuration comes from the environment the platform injects: NASIKO_STATE_URL (base URL of
 * the state API) and NASIKO_AGENT_TOKEN (this agent's state credential, the only one accepted).
 * With NASIKO_STATE_URL unset every factory returns the SDK's in-memory implementation.
 */

export * from "./errors.js";
export {
  AGENT_TOKEN_ENV,
  CLIENT_NAME,
  MAX_PAGE_SIZE,
  NasikoStateClient,
  STATE_URL_ENV,
  percentEncode,
} from "./client.js";
export {
  COALESCE_WINDOW_MS,
  PENDING_MAX_AGE_MS,
  NasikoTaskStore,
  TASK_FORMAT,
  closeOpenTaskStores,
  decodeStoredTask,
  encodeStoredTask,
  installShutdownHooks,
  stateName,
  taskStoreFromEnv,
} from "./task-store.js";
