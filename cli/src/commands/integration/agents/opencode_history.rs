//! OpenCode's past sessions, read from its SQLite store (`$XDG_DATA_HOME/opencode/opencode.db`).
//!
//! The store is opened read-only (safe beside a running OpenCode: SQLite's WAL lets a reader in) and
//! only the `session`, `message` and `part` tables are read — never `account`, `credential` or
//! `auth.json`. Each session is rebuilt into the V1 payload the OpenCode plugin sends
//! (`{session_id, messages: [{info, parts}]}`, plus `parent_id`, `agent` and the spawning `task` call's
//! `tool_call_id` for a subagent's session), so the hook's own `snapshot` parses it unchanged.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::super::catalog;
use super::super::model::SessionSnapshot;
use super::{PastSession, SessionSource, epoch_millis, opencode};

/// How long a read waits for OpenCode to finish a write before the session counts as failed.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Every OpenCode session, a subagent's too (linked to the session that spawned it), oldest first, each
/// read only when the import reaches it. No store is no sessions; a store that can't be opened is an
/// error.
pub fn past_sessions() -> Result<Vec<PastSession>> {
    past_sessions_in(&database_path())
}

fn database_path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| catalog::home().join(".local").join("share"))
        .join("opencode")
        .join("opencode.db")
}

fn past_sessions_in(path: &Path) -> Result<Vec<PastSession>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let db = Rc::new(open_read_only(path)?);
    // Stores written before OpenCode kept a session's agent have no `agent` column.
    let agent = if has_column(&db, "session", "agent")? {
        "agent"
    } else {
        "NULL"
    };
    let sessions: Vec<StoredSession> = db
        .prepare(&format!(
            "SELECT id, title, time_updated, parent_id, {agent} FROM session
             ORDER BY time_created, id"
        ))?
        .query_map([], |row| {
            Ok(StoredSession {
                id: row.get(0)?,
                title: row.get(1)?,
                updated: row.get(2)?,
                parent_id: row.get(3)?,
                agent: row.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()
        .context("failed to list OpenCode sessions")?;
    Ok(sessions
        .into_iter()
        .map(|session| {
            let db = db.clone();
            PastSession {
                key: session.id.clone(),
                last_active: epoch_millis(session.updated),
                source: SessionSource::Load(Box::new(move || {
                    let mut payload = session_payload(&db, &session.id)?;
                    if let Some(parent_id) = &session.parent_id {
                        payload["parent_id"] = json!(parent_id);
                        payload["agent"] = json!(session.agent);
                        payload["tool_call_id"] =
                            json!(spawning_call(&db, parent_id, &session.id)?);
                    }
                    let mut snapshot = opencode::snapshot(&payload.to_string())?;
                    snapshot.title = session.title.filter(|title| !title.trim().is_empty());
                    Ok::<SessionSnapshot, anyhow::Error>(snapshot)
                })),
            }
        })
        .collect())
}

fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = db.query_row(
        "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// One `session` row: what an import lists, and links a subagent's session by.
struct StoredSession {
    id: String,
    title: Option<String>,
    updated: i64,
    parent_id: Option<String>,
    agent: Option<String>,
}

/// The parent's `task` tool call that spawned session `child_id`: the one whose result names it
/// (`state.metadata.sessionId`). None when the parent kept no such call.
fn spawning_call(db: &Connection, parent_id: &str, child_id: &str) -> Result<Option<String>> {
    let mut statement =
        db.prepare("SELECT data FROM part WHERE session_id = ?1 AND instr(data, ?2) > 0")?;
    let rows = statement.query_map([parent_id, child_id], |row| row.get::<_, String>(0))?;
    for row in rows {
        let Ok(part) = serde_json::from_str::<Value>(&row?) else {
            continue;
        };
        let spawned_child = part["type"] == "tool"
            && part["state"]["metadata"]["sessionId"].as_str() == Some(child_id);
        if spawned_child {
            return Ok(part["callID"].as_str().map(str::to_string));
        }
    }
    Ok(None)
}

/// Opens an SQLite store for reading only; nothing this does can change it.
pub(super) fn open_read_only(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", path.display()))?;
    db.busy_timeout(BUSY_TIMEOUT)?;
    Ok(db)
}

/// One session as the OpenCode plugin would send it. The stored rows keep a message's and a part's id
/// in their own columns, so they are put back where the plugin's JSON has them.
fn session_payload(db: &Connection, session_id: &str) -> Result<Value> {
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    let mut statement = db.prepare(
        "SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY message_id, id",
    )?;
    let rows = statement.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, message_id, data) = row?;
        let part = with_fields(&data, [("id", &id), ("messageID", &message_id)])?;
        parts.entry(message_id).or_default().push(part);
    }

    let mut statement =
        db.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row?;
        let info = with_fields(&data, [("id", &id)])?;
        messages.push(json!({"info": info, "parts": parts.remove(&id).unwrap_or_default()}));
    }
    Ok(json!({"session_id": session_id, "messages": messages}))
}

/// A stored JSON object with `fields` set on it.
fn with_fields<const N: usize>(data: &str, fields: [(&str, &str); N]) -> Result<Value> {
    let mut object: Map<String, Value> =
        serde_json::from_str(data).context("OpenCode stored a row that is not a JSON object")?;
    for (name, value) in fields {
        object.insert(name.to_string(), Value::String(value.to_string()));
    }
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tables this reads, as OpenCode creates them (other columns left out).
    fn fixture(path: &Path) {
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            r#"
            CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT NOT NULL,
                                  agent TEXT,
                                  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                                  time_created INTEGER NOT NULL, data TEXT NOT NULL);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                               time_created INTEGER NOT NULL, data TEXT NOT NULL);
            INSERT INTO session VALUES ('ses_root', NULL, 'Fix the build', 'build', 1000, 1500),
                                       ('ses_child', 'ses_root', 'Subagent', 'explore', 1001, 1001);
            INSERT INTO message VALUES
              ('msg_u', 'ses_root', 1000, '{"role":"user","time":{"created":1000}}'),
              ('msg_a', 'ses_root', 1100, '{"role":"assistant","parentID":"msg_u","providerID":"openai","modelID":"gpt-5","finish":"stop","time":{"created":1100,"completed":1500},"tokens":{"input":20,"output":5,"cache":{"read":6,"write":0}}}');
            INSERT INTO part VALUES
              ('prt_1', 'msg_u', 'ses_root', 1000, '{"type":"text","text":"Do it"}'),
              ('prt_2', 'msg_a', 'ses_root', 1200, '{"type":"tool","tool":"bash","callID":"call-1","state":{"status":"completed","input":{"command":"ls"},"output":"ok","time":{"start":1200,"end":1300}}}'),
              ('prt_3', 'msg_a', 'ses_root', 1400, '{"type":"text","text":"Done"}'),
              ('prt_4', 'msg_a', 'ses_root', 1450, '{"type":"tool","tool":"task","callID":"call-task","state":{"status":"completed","input":{"prompt":"Look"},"output":"found","metadata":{"parentSessionId":"ses_root","sessionId":"ses_child"},"time":{"start":1001,"end":1440}}}');
            INSERT INTO message VALUES
              ('msg_cu', 'ses_child', 1001, '{"role":"user","time":{"created":1001}}'),
              ('msg_ca', 'ses_child', 1002, '{"role":"assistant","parentID":"msg_cu","providerID":"openai","modelID":"gpt-5","finish":"stop","time":{"created":1002,"completed":1430},"tokens":{"input":9,"output":3,"cache":{"read":0,"write":0}}}');
            INSERT INTO part VALUES
              ('prt_c1', 'msg_cu', 'ses_child', 1001, '{"type":"text","text":"Look"}'),
              ('prt_c2', 'msg_ca', 'ses_child', 1420, '{"type":"text","text":"Found"}');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn a_subagents_session_is_its_own_linked_to_the_session_and_call_that_spawned_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        fixture(&path);

        let sessions = past_sessions_in(&path).unwrap();

        let keys: Vec<&str> = sessions.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["ses_root", "ses_child"]);
        let SessionSource::Load(load) = sessions.into_iter().nth(1).unwrap().source else {
            panic!("OpenCode sessions load from the store");
        };
        let child = load().unwrap();
        assert_eq!(child.session_id, "ses_child");
        let parent = child.parent.as_ref().unwrap();
        assert_eq!(parent.source_id, "ses_root");
        assert_eq!(parent.tool_call_id.as_deref(), Some("call-task"));
        assert_eq!(parent.agent_type.as_deref(), Some("explore"));
        // Its turn is its own: the parent's turn keeps only the parent's usage.
        assert_eq!(child.turns.len(), 1);
        assert_eq!(child.turns[0].calls[0].input_tokens, 9);
        assert_eq!(child.turns[0].response.as_deref(), Some("Found"));
    }

    #[test]
    fn a_store_without_the_agent_column_still_links_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute("ALTER TABLE session DROP COLUMN agent", [])
            .unwrap();

        let sessions = past_sessions_in(&path).unwrap();

        let SessionSource::Load(load) = sessions.into_iter().nth(1).unwrap().source else {
            panic!("OpenCode sessions load from the store");
        };
        let parent = load().unwrap().parent.unwrap();
        assert_eq!(parent.source_id, "ses_root");
        assert_eq!(parent.agent_type, None);
    }

    #[test]
    fn a_subagent_whose_spawning_call_is_gone_is_linked_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM part WHERE id = 'prt_4'", [])
            .unwrap();
        let db = open_read_only(&path).unwrap();

        assert_eq!(spawning_call(&db, "ses_root", "ses_child").unwrap(), None);
    }

    #[test]
    fn root_sessions_are_rebuilt_into_the_plugin_payload_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        fixture(&path);

        let sessions = past_sessions_in(&path).unwrap();

        let session = sessions.into_iter().next().unwrap();
        assert_eq!(session.key, "ses_root");
        assert_eq!(session.last_active.timestamp_millis(), 1500);
        let SessionSource::Load(load) = session.source else {
            panic!("OpenCode sessions load from the store");
        };
        let snapshot = load().unwrap();
        assert_eq!(snapshot.session_id, "ses_root");
        assert!(snapshot.parent.is_none());
        assert_eq!(snapshot.title.as_deref(), Some("Fix the build"));
        assert_eq!(snapshot.turns.len(), 1);
        let turn = &snapshot.turns[0];
        assert_eq!(turn.uuid, "msg_u");
        assert_eq!(turn.prompt, "Do it");
        assert_eq!(turn.response.as_deref(), Some("Done"));
        assert_eq!(turn.calls[0].input_tokens, 20);
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].name, "bash");
    }

    #[test]
    fn the_payload_puts_back_the_ids_the_rows_keep_in_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        fixture(&path);
        let db = open_read_only(&path).unwrap();

        let payload = session_payload(&db, "ses_root").unwrap();

        assert_eq!(payload["messages"][0]["info"]["id"], "msg_u");
        assert_eq!(payload["messages"][1]["parts"][0]["messageID"], "msg_a");
        assert_eq!(payload["messages"][1]["parts"][0]["id"], "prt_2");
    }

    #[test]
    fn a_missing_store_is_no_sessions_and_a_read_only_store_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            past_sessions_in(&dir.path().join("missing.db"))
                .unwrap()
                .is_empty()
        );

        let path = dir.path().join("opencode.db");
        fixture(&path);
        let db = open_read_only(&path).unwrap();
        assert!(db.execute("DELETE FROM session", []).is_err());
    }
}
