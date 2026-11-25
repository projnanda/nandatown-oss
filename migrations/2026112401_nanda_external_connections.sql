-- Fork-owned migrations use date-based versions, separate from upstream's
-- sequential versions. These bindings are not generic agent runtime routes.
CREATE TABLE nanda_external_connections (
    agent_id UUID PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    owner_id UUID NOT NULL REFERENCES users(id),
    card_url TEXT NOT NULL CHECK (octet_length(card_url) BETWEEN 1 AND 2048),
    endpoint TEXT NOT NULL CHECK (octet_length(endpoint) BETWEEN 1 AND 2048),
    auth_mode TEXT NOT NULL CHECK (auth_mode IN ('none', 'bearer')),
    peer_token_enc TEXT,
    CHECK ((auth_mode = 'bearer') = (peer_token_enc IS NOT NULL)),
    UNIQUE (owner_id, card_url)
);

-- The generic catalog also supports soft deletion. Once it retires an agent
-- or changes ownership, even later restoring that row must not revive a peer
-- credential or grant a new owner a previous owner's external authority.
CREATE FUNCTION nanda_retire_external_connection() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM nanda_external_connections WHERE agent_id = NEW.id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER nanda_retire_external_connection
AFTER UPDATE OF deleted_at, owner_id ON agents
FOR EACH ROW WHEN (
    (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    OR NEW.owner_id IS DISTINCT FROM OLD.owner_id
) EXECUTE FUNCTION nanda_retire_external_connection();
