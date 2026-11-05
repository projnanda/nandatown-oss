-- =============================================================================
-- Embedding routing — per-config and organization-wide embedding model selection
-- =============================================================================
-- `POST /v1/embeddings` previously resolved through the *chat* config, so a
-- configured agent sent a chat model to an embeddings endpoint and got a 400.
-- These columns give the embedding path its own provider/model/key, completely
-- independent of the chat columns in the same rows.
--
-- Selection is by presence, never by trial: config-with-override → the request's
-- own model → the config's embedding model → the organization default → 400.
-- There is no chaining, so a failure at the chosen level is final.

-- Per-config embedding routing. Independent of the chat columns in this table.
ALTER TABLE llm_configs
    ADD COLUMN embedding_provider            TEXT,
    ADD COLUMN embedding_model               TEXT,
    ADD COLUMN embedding_api_key_secret_name TEXT,
    -- When true this config's embedding model beats the model named on the
    -- request. Off by default, because the agent — not the platform — knows
    -- which vector space its stored data lives in.
    ADD COLUMN embedding_model_override      BOOLEAN NOT NULL DEFAULT false;

-- The model is what turns embeddings on; a model with no provider cannot be
-- routed, so the pair is all-or-nothing. The key stays optional: leaving it
-- empty means "use the platform key for this provider".
ALTER TABLE llm_configs ADD CONSTRAINT llm_configs_embedding_pair_ck
    CHECK (num_nonnulls(embedding_provider, embedding_model) <> 1);

-- Organization default embedding model. The key is stored encrypted in this
-- table rather than referenced from user_secrets, because settings is a single
-- global row and has no owning user. Same approach as
-- oidc_client_secret_encrypted: AES-256-GCM via SecretsCrypto::for_platform_settings().
ALTER TABLE settings
    ADD COLUMN default_embedding_provider          TEXT,
    ADD COLUMN default_embedding_model             TEXT,
    ADD COLUMN default_embedding_api_key_encrypted TEXT;

ALTER TABLE settings ADD CONSTRAINT settings_embedding_pair_ck
    CHECK (num_nonnulls(default_embedding_provider, default_embedding_model) <> 1);
