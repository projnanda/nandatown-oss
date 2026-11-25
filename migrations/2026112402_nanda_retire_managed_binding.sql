-- Rehosting through the ordinary catalog changes the agent's lifecycle:
-- the external-only disconnect operation must no longer be able to retire
-- a managed workload or continue using the old remote peer's credential.
CREATE TRIGGER nanda_retire_repurposed_connection
AFTER UPDATE OF status, image, url ON agents
FOR EACH ROW WHEN (
    (OLD.status = 'registered' AND NEW.status <> 'registered')
    OR (OLD.image IS NULL AND NEW.image IS NOT NULL)
    OR (OLD.url IS NULL AND NEW.url IS NOT NULL)
) EXECUTE FUNCTION nanda_retire_external_connection();

DELETE FROM nanda_external_connections c
USING agents a
WHERE a.id=c.agent_id
  AND (a.status <> 'registered' OR a.image IS NOT NULL OR a.url IS NOT NULL);
