-- 0054_workspace.sql (part 1: system connectors)
ALTER TABLE mcp_connectors DROP CONSTRAINT IF EXISTS mcp_connectors_provider_type_check;
ALTER TABLE mcp_connectors
    ADD CONSTRAINT mcp_connectors_provider_type_check
    CHECK (provider_type IN ('composio', 'mcp_server', 'system'));
-- A platform-owned backend served by the control plane itself (loopback). Exempt from
-- the SSRF guard, tools are exposed un-prefixed, never user-deletable.
ALTER TYPE mcp_connector_source_kind ADD VALUE IF NOT EXISTS 'system';
-- Harvested at probe today but only kept as `description`; instructions need their own column
-- so `initialize` can forward them verbatim.
ALTER TABLE mcp_connectors ADD COLUMN IF NOT EXISTS instructions TEXT;

-- 0003_mcp.sql's chk_connectors_provider_fields only allows the composio/
-- mcp_server field combinations, so a provider_type='system' row (however
-- source_kind/auth_type are set) still cannot be inserted without this.
-- Existing clauses reproduced verbatim; only the trailing `system` clause is
-- new. It requires url IS NOT NULL, source_kind='system' (ties the two
-- columns together — nothing above stops a 'system' provider_type from
-- pairing with, say, source_kind='external_url' otherwise), and
-- auth_type='none' (no per-user credential — see
-- credentials::build_server_config, which never reaches the other auth_type
-- arms for a provider_type='system' row today).
--
-- The auth_type comparison uses `IS NOT DISTINCT FROM` rather than `=`: a
-- CHECK constraint only rejects a row when its expression evaluates to
-- FALSE, and treats NULL as passing, same as every other row-level check —
-- so a plain `auth_type = 'none'` would let a system row with auth_type
-- NULL silently through (the clause evaluates NULL, not FALSE) rather than
-- rejecting it. `source_kind::text = 'system'` (not a bare `=` against the
-- enum) is required because `source_kind` is only widened to accept the
-- 'system' value a few statements above in this same migration file, and
-- Postgres refuses to use a newly added enum value inside the transaction
-- that added it; comparing as text sidesteps that restriction.
ALTER TABLE mcp_connectors DROP CONSTRAINT IF EXISTS chk_connectors_provider_fields;
ALTER TABLE mcp_connectors
    ADD CONSTRAINT chk_connectors_provider_fields CHECK (
        (provider_type = 'composio' AND auth_config_id IS NOT NULL AND url IS NULL) OR
        (provider_type = 'mcp_server' AND (
            (source_kind = 'external_url' AND url IS NOT NULL AND auth_config_id IS NULL) OR
            (source_kind = 'uploaded_build' AND auth_config_id IS NULL AND (
                (build_status IN ('pending', 'building', 'failed')) OR
                (build_status = 'running' AND url IS NOT NULL)
            ))
        )) OR
        (provider_type = 'system' AND url IS NOT NULL
            AND auth_type IS NOT DISTINCT FROM 'none' AND source_kind::text = 'system')
    );
