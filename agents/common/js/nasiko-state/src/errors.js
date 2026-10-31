/**
 * Errors raised by the state client and stores. Every failure is a NasikoStateError; the
 * subclasses name the answers a caller can act on. Nothing falls back silently.
 */

export class NasikoStateError extends Error {
  /**
   * @param {string} message
   * @param {{status?: number | null, body?: string | null, cause?: unknown}} [details]
   *   `status` and `body` carry the HTTP answer; both are null for transport and configuration
   *   failures, where there was no answer.
   */
  constructor(message, { status = null, body = null, cause } = {}) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = new.target.name;
    this.status = status;
    this.body = body;
  }
}

/** 401: the agent token is missing, unknown, or not a state-scoped credential. */
export class NasikoStateUnauthorized extends NasikoStateError {}

/** 404 on an operation that has no "missing" result (reads and deletes return null / false). */
export class NasikoStateNotFound extends NasikoStateError {}

/** 412: an If-Match precondition failed because another writer changed the row first. */
export class NasikoStateConflict extends NasikoStateError {
  /**
   * @param {string} message
   * @param {{currentVersion: number | null, body?: string | null}} details
   */
  constructor(message, { currentVersion, body = null }) {
    super(message, { status: 412, body });
    /** The version the server holds, or null if its answer did not say. */
    this.currentVersion = currentVersion;
  }
}

/** 413: a value above the platform's size limit. Retrying cannot help. */
export class NasikoStateTooLarge extends NasikoStateError {}

/** 429: over the per-agent request rate and not worth waiting out. */
export class NasikoStateRateLimited extends NasikoStateError {
  /**
   * @param {string} message
   * @param {{retryAfterSeconds: number | null, body?: string | null}} details
   */
  constructor(message, { retryAfterSeconds, body = null }) {
    super(message, { status: 429, body });
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

/**
 * A stored row cannot be read back as what it claims to be: written by the other a2a SDK major,
 * not by a Nasiko store at all, or damaged. Never answered with a half-decoded object.
 */
export class NasikoStateMalformedRow extends NasikoStateError {}

/** Whether the same request could succeed later: no answer at all, or a server-side failure. */
export function isTransient(error) {
  return error instanceof NasikoStateError && (error.status === null || error.status >= 500);
}
