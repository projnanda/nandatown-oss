-- The Optimization page's remaining stored state: a master switch, run-time tier values, and a
-- record of when a user changed either preference.

-- CX-5. The switch is separate from `context_selection_strategy` on purpose. Folding "off" into
-- that enum as a fourth value would make turning selection off forget which strategy to return to,
-- and the two answer different questions: *whether* to select, and *how*.
--
-- Defaults true, so an existing deployment behaves exactly as it did before this column existed.
ALTER TABLE users ADD COLUMN context_selection_enabled BOOLEAN NOT NULL DEFAULT true;

-- CX-H. One row per actual change, so the savings chart can mark "you switched to PACMS here".
--
-- Written only when a value really moved: a PATCH that re-sends the current value is a no-op the
-- chart must not draw a mark for. Values are TEXT rather than the source enums because one table
-- records three differently-typed preferences, and the API serves them as strings either way.
CREATE TABLE user_setting_changes (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    field      TEXT NOT NULL CHECK (field IN ('strategy', 'level', 'enabled')),
    from_value TEXT NOT NULL,
    to_value   TEXT NOT NULL,
    changed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The only query: one user's changes over a window, newest first.
CREATE INDEX idx_user_setting_changes_user ON user_setting_changes (user_id, changed_at DESC);

-- CX-T1. Tier values become run-time settings with the environment as fallback.
--
-- Nullable, every one of them: NULL means "this deployment has not chosen, use its environment".
-- A NOT NULL DEFAULT would silently freeze today's env values into the database on migration and
-- make a later env change do nothing, which is the opposite of a fallback.
--
-- Deliberately NOT part of `PUT /api/settings`, which is a full-row replace owned by the OSS
-- settings page: that page knows nothing about these columns and would blank them. Same reasoning
-- the EE orchestrator guardrail columns are kept out of it.
ALTER TABLE settings ADD COLUMN pacms_budget_low      INT;
ALTER TABLE settings ADD COLUMN pacms_budget_medium   INT;
ALTER TABLE settings ADD COLUMN pacms_budget_high     INT;
ALTER TABLE settings ADD COLUMN context_k_low         INT;
ALTER TABLE settings ADD COLUMN context_k_medium      INT;
ALTER TABLE settings ADD COLUMN context_k_high        INT;
ALTER TABLE settings ADD COLUMN compress_history      BOOLEAN;
ALTER TABLE settings ADD COLUMN context_pool_size     INT;
ALTER TABLE settings ADD COLUMN context_mandatory_recent INT;
ALTER TABLE settings ADD COLUMN compress_min_bytes    INT;
-- The write guard the tiers page sends back as `expected_updated_at`.
ALTER TABLE settings ADD COLUMN context_tiers_updated_at TIMESTAMPTZ;
