-- =============================================================================
-- trace_usage.error_count: unrecovered error spans per (trace, agent).
--
-- The materializer already fetches every trace in its window and walks the span
-- tree to pull tokens and cost; counting unrecovered errors in that same pass
-- costs nothing extra. Before this, `session/list.error_count` had no source and
-- was always null.
--
-- Counted PER AGENT ROW, not per trace: trace_usage is keyed
-- (trace_id, agent_name), so writing a trace-wide total into every row would
-- multiply the count by the number of agents the moment anything sums it.
--
-- NULL means "not yet materialized", never "zero errors" -- readers must keep
-- that distinction, because a trace the materializer has not reached yet is not
-- evidence of success.
-- =============================================================================

ALTER TABLE trace_usage ADD COLUMN error_count INTEGER;
