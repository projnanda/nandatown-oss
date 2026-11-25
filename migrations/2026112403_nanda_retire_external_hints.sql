-- Clean only when this transaction actually retires a binding. Do not
-- infer ownership of historical metadata from strings on arbitrary agents.
CREATE OR REPLACE FUNCTION nanda_retire_external_connection() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM nanda_external_connections WHERE agent_id = NEW.id;
    IF FOUND THEN
        UPDATE agents SET
            metadata = CASE WHEN jsonb_typeof(metadata) = 'object'
                            THEN metadata - 'nanda_external' ELSE metadata END,
            tags = array_remove(tags, 'nanda-external-a2a'),
            description = NULLIF(description,
              'Independently operated A2A agent. Use the External agents page to send a text task; the catalog URL is not an invocation endpoint.')
        WHERE id = NEW.id;
    END IF;
    RETURN NULL;
END;
$$;
