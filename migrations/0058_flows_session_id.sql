-- =============================================================================
-- flows.session_id + the indexes session/list needs to be served from SQL.
--
-- `session/list` used to enrich every row from the trace store (one Tempo
-- search plus one full trace download per trace, per row). Everything it
-- returns except the failure status already has a SQL source -- `session_traces`
-- for the trace count, `trace_usage` for tokens/cost/latency, `chat_messages`
-- for the input/output previews. The one missing link was the flow outcome:
-- a flow's chat session lived only inside `flows.metadata` as `context_id`
-- (agent_proxy.rs), which cannot be indexed or grouped.
-- =============================================================================

ALTER TABLE flows ADD COLUMN session_id TEXT;

-- Backfill from where the value has always been written. Deliberately not a
-- FK to chat_sessions: a flow outlives a deleted session, and the column must
-- survive that cascade.
UPDATE flows
   SET session_id = metadata->>'context_id'
 WHERE session_id IS NULL
   AND metadata->>'context_id' IS NOT NULL;

-- Per-session flow rollup (count, failed count, running count).
CREATE INDEX idx_flows_session ON flows (session_id, created_at DESC)
    WHERE session_id IS NOT NULL;

-- session/list pages newest-first on (created_at, session_id) with a keyset
-- cursor. Neither existing index covers that ordering: idx_chat_sessions_recent
-- is (user_id, created_at DESC) with no tiebreaker, and
-- idx_chat_sessions_user_updated sorts on updated_at.
CREATE INDEX idx_chat_sessions_user_created
    ON chat_sessions (user_id, created_at DESC, session_id DESC);

-- The superuser path pages the same ordering without the user predicate.
CREATE INDEX idx_chat_sessions_created_all
    ON chat_sessions (created_at DESC, session_id DESC);
