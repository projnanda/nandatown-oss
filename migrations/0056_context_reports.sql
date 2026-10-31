-- One row per request that ran history selection: what selection chose from, and what it kept.
--
-- Why a table and not `token_savings`. The grain differs. `token_savings` is per (LLM call, layer)
-- and answers "what did each optimization layer remove"; the Optimization page asks "how much
-- history did this *request* carry, before and after", and one request is several calls. Counting
-- the same pool once per call would multiply every figure on the page by the ReAct loop's depth.
--
-- Why not `chat_messages.metadata`. Every query over this table filters by window and agent and
-- groups by day. A JSONB path under a per-session row answers that with a scan; this is a fact
-- table, so it answers with an index. The same reasoning migration 0052 records for `token_savings`.
--
-- Costs are deliberately NOT stored here. The report is written before any model call, so no price
-- is known at write time, and a price written later would be a second source of truth. The read
-- path joins the request's first `token_usage` row to `model_pricing` instead -- a request with no
-- priced call counts in `reports` but not in `priced_reports`, which is exactly what the page says.
CREATE TABLE context_reports (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- NULL only for a request whose agent was never resolved. An orchestrator-routed chat selects
    -- history *before* routing, so writers record after the decision and fill this in; a row that
    -- stays NULL counts in the totals and is left out of the per-agent breakdown.
    agent_id        UUID REFERENCES agents(id) ON DELETE SET NULL,
    -- A2A contextId -- the chat. Joins to `chat_sessions.session_id`.
    session_id      TEXT,
    -- W3C trace id of the request span, and that span's own id. Both are served to the UI so
    -- "biggest senders" can open the trace at the span carrying this report.
    trace_id        TEXT,
    span_id         TEXT,

    -- What ran. Recorded per request rather than read from `users` at query time, because the
    -- question the chart asks is what each request actually carried, not what the user prefers now.
    strategy        TEXT CHECK (strategy IN ('pacms', 'topk', 'lastk')),
    level           TEXT CHECK (level IN ('low', 'medium', 'high')),
    budget          INTEGER,
    -- Selection degraded to recency (embeddings down, or no complete pairs).
    fallback        BOOLEAN NOT NULL DEFAULT false,

    -- The measurement. Token figures are estimated from bytes at a fixed divisor -- there is no
    -- provider response at this seam to calibrate against -- which is why the API names the sent
    -- side `kept_tokens_est` rather than claiming a counted figure.
    pool_messages   INTEGER NOT NULL,
    pool_tokens     BIGINT  NOT NULL,
    kept_messages   INTEGER NOT NULL,
    kept_tokens     BIGINT  NOT NULL,
    -- Bytes history compression (IP-4) removed before selection scored anything. 0 when it did not
    -- run, which is the truth for every `lastk` request -- it reads uncompressed by construction.
    compressed_bytes_saved BIGINT NOT NULL DEFAULT 0,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Window scan: every query starts here, and `recorded_since` is this index's first row.
CREATE INDEX idx_context_reports_window  ON context_reports (created_at DESC);
-- The per-agent breakdown, and the agent filter shared with the rest of finops.
CREATE INDEX idx_context_reports_agent   ON context_reports (agent_id, created_at DESC) WHERE agent_id IS NOT NULL;
-- Redaction in "biggest senders" is decided by who owns the row.
CREATE INDEX idx_context_reports_user    ON context_reports (user_id, created_at DESC);
-- Join back to token_usage / trace_usage for one request.
CREATE INDEX idx_context_reports_trace   ON context_reports (trace_id) WHERE trace_id IS NOT NULL;
