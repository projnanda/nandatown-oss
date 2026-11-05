//! The JSON shapes on the wire. Field names are the contract with the agent SDKs: renaming or
//! retyping one is a breaking change, so additions only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::cursor::{KeyCursor, TaskCursor};
use super::error::StateError;
use super::page::ListedRow;
use super::repo::{KvQuery, KvRow, Stamp, TaskQuery, TaskRow};
use super::validation::{
    MAX_CLIENT_NAME_BYTES, MAX_CONTEXT_ID_BYTES, MAX_KEY_BYTES, MAX_TASK_STATE_BYTES, check_name,
    non_empty, page_size,
};

/// What an agent's store can do, as declared when it registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Capability {
    Tasks,
    Kv,
}

/// `POST /register`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegisterRequest {
    pub(super) capabilities: Vec<Capability>,
    /// Name and version of the SDK, for the log. Optional so an older SDK still registers.
    pub(super) client: Option<String>,
}

/// `PUT /tasks/{task_id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutTaskRequest {
    pub(super) context_id: Option<String>,
    pub(super) state: String,
    /// The SDK's serialized task. Opaque to the server and kept as the raw text received; any
    /// JSON value, `null` included.
    pub(super) body: Box<RawValue>,
}

/// `PUT /kv/{key}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutKvRequest {
    /// Opaque to the server and kept as the raw text received, like a task's `body`.
    pub(super) value: Box<RawValue>,
}

/// Query string of `GET /tasks`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListTasksQuery {
    pub(super) context_id: Option<String>,
    pub(super) state: Option<String>,
    pub(super) limit: Option<String>,
    pub(super) cursor: Option<String>,
}

/// Query string of `GET /kv`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListKvQuery {
    pub(super) prefix: Option<String>,
    pub(super) limit: Option<String>,
    pub(super) cursor: Option<String>,
}

impl TryFrom<ListTasksQuery> for TaskQuery {
    type Error = StateError;

    /// Checks every filter before any of it reaches SQL: an unstorable value (a control
    /// character, say) must be a 400 here rather than a database error there.
    fn try_from(query: ListTasksQuery) -> Result<Self, StateError> {
        let context_id = non_empty(query.context_id);
        if let Some(context_id) = &context_id {
            check_name("context_id", context_id, MAX_CONTEXT_ID_BYTES)?;
        }
        let state = non_empty(query.state);
        if let Some(state) = &state {
            check_name("state", state, MAX_TASK_STATE_BYTES)?;
        }
        Ok(Self {
            context_id,
            state,
            after: non_empty(query.cursor)
                .map(|raw| TaskCursor::decode(&raw))
                .transpose()?,
            page_size: page_size(non_empty(query.limit).as_deref())?,
        })
    }
}

impl TryFrom<ListKvQuery> for KvQuery {
    type Error = StateError;

    fn try_from(query: ListKvQuery) -> Result<Self, StateError> {
        let prefix = non_empty(query.prefix);
        if let Some(prefix) = &prefix {
            check_name("prefix", prefix, MAX_KEY_BYTES)?;
        }
        Ok(Self {
            prefix,
            after: non_empty(query.cursor)
                .map(|raw| KeyCursor::decode(&raw))
                .transpose()?,
            page_size: page_size(non_empty(query.limit).as_deref())?,
        })
    }
}

impl RegisterRequest {
    /// Refuses a registration that declares nothing, or reports a client name that cannot be
    /// logged safely.
    pub(super) fn check(&self) -> Result<(), StateError> {
        if self.capabilities.is_empty() {
            return Err(StateError::invalid(
                "capabilities must name at least one of: tasks, kv",
            ));
        }
        if let Some(client) = &self.client {
            check_name("client", client, MAX_CLIENT_NAME_BYTES)?;
        }
        Ok(())
    }

    /// Whether the agent keeps its tasks here, which is what makes it durable. A key-value store
    /// alone does not: nothing the platform schedules depends on it.
    pub(super) fn stores_tasks(&self) -> bool {
        self.capabilities.contains(&Capability::Tasks)
    }
}

impl PutTaskRequest {
    /// Checks the fields around the opaque `body`; the body itself is only size-checked.
    pub(super) fn check(&self) -> Result<(), StateError> {
        check_name("state", &self.state, MAX_TASK_STATE_BYTES)?;
        if let Some(context_id) = &self.context_id {
            check_name("context_id", context_id, MAX_CONTEXT_ID_BYTES)?;
        }
        Ok(())
    }
}

/// Response to `PUT /tasks/{task_id}`.
#[derive(Debug, Serialize)]
pub(super) struct TaskStamp {
    task_id: String,
    version: i64,
    updated_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl TaskStamp {
    pub(super) fn new(task_id: String, stamp: Stamp) -> Self {
        Self {
            task_id,
            version: stamp.version,
            updated_at: stamp.updated_at,
            expires_at: stamp.expires_at,
        }
    }
}

/// A stored task, as `GET /tasks/{task_id}` and each `GET /tasks` item return it.
#[derive(Debug, Serialize)]
pub(super) struct TaskView {
    task_id: String,
    context_id: Option<String>,
    state: String,
    body: Box<RawValue>,
    version: i64,
    updated_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl ListedRow for TaskRow {
    type View = TaskView;

    fn name(&self) -> &str {
        &self.task_id
    }

    fn resume_after(&self) -> String {
        TaskCursor {
            updated_at: self.updated_at,
            task_id: self.task_id.clone(),
        }
        .encode()
    }

    fn into_view(self, body: Box<RawValue>) -> TaskView {
        TaskView {
            body,
            task_id: self.task_id,
            context_id: self.context_id,
            state: self.state,
            version: self.version,
            updated_at: self.updated_at,
            expires_at: self.expires_at,
        }
    }
}

/// Response to `PUT /kv/{key}`.
#[derive(Debug, Serialize)]
pub(super) struct KvStamp {
    key: String,
    version: i64,
    updated_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl KvStamp {
    pub(super) fn new(key: String, stamp: Stamp) -> Self {
        Self {
            key,
            version: stamp.version,
            updated_at: stamp.updated_at,
            expires_at: stamp.expires_at,
        }
    }
}

/// A stored entry, as `GET /kv/{key}` and each `GET /kv` item return it.
#[derive(Debug, Serialize)]
pub(super) struct KvView {
    key: String,
    value: Box<RawValue>,
    version: i64,
    updated_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl ListedRow for KvRow {
    type View = KvView;

    fn name(&self) -> &str {
        &self.key
    }

    fn resume_after(&self) -> String {
        KeyCursor {
            key: self.key.clone(),
        }
        .encode()
    }

    fn into_view(self, value: Box<RawValue>) -> KvView {
        KvView {
            value,
            key: self.key,
            version: self.version,
            updated_at: self.updated_at,
            expires_at: self.expires_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Parses through the JSON text, as a request body is: a raw field cannot be read out of an
    /// already-built `Value`.
    fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
        serde_json::from_str(&value.to_string())
    }

    #[test]
    fn a_task_put_needs_a_state_and_a_body_but_not_a_context() {
        let with_null_body: PutTaskRequest =
            parse(json!({"state": "working", "body": null})).unwrap();
        assert_eq!(with_null_body.context_id, None);
        assert_eq!(with_null_body.body.get(), "null");

        assert!(parse::<PutTaskRequest>(json!({"state": "working"})).is_err());
        assert!(parse::<PutTaskRequest>(json!({"body": {}})).is_err());
    }

    #[test]
    fn a_task_put_may_carry_a_null_or_string_context() {
        let null: PutTaskRequest =
            parse(json!({"context_id": null, "state": "s", "body": 1})).unwrap();
        assert_eq!(null.context_id, None);
        let set: PutTaskRequest =
            parse(json!({"context_id": "ctx", "state": "s", "body": 1})).unwrap();
        assert_eq!(set.context_id.as_deref(), Some("ctx"));
    }

    #[test]
    fn unknown_fields_are_refused_on_every_body_and_query() {
        assert!(parse::<PutTaskRequest>(json!({"state": "s", "body": 1, "extra": 1})).is_err());
        assert!(parse::<PutKvRequest>(json!({"value": 1, "extra": 1})).is_err());
        assert!(parse::<RegisterRequest>(json!({"capabilities": ["kv"], "extra": 1})).is_err());
        assert!(parse::<ListTasksQuery>(json!({"contextid": "typo"})).is_err());
        assert!(parse::<ListKvQuery>(json!({"limit": "5", "order": "desc"})).is_err());
    }

    #[test]
    fn a_kv_put_needs_a_value_and_accepts_any_json_for_it() {
        for value in [
            json!(null),
            json!(0),
            json!("s"),
            json!([1]),
            json!({"a": {}}),
        ] {
            let request: PutKvRequest = parse(json!({ "value": value.clone() })).unwrap();
            assert_eq!(request.value.get(), value.to_string());
        }
        assert!(parse::<PutKvRequest>(json!({})).is_err());
    }

    #[test]
    fn a_body_is_held_as_the_exact_text_received() {
        let sent =
            r#"{"state":"s","body":{"z":1,  "a":123456789012345678901234567890,"e":"\u00e9"}}"#;
        let request: PutTaskRequest = serde_json::from_str(sent).unwrap();
        assert_eq!(
            request.body.get(),
            r#"{"z":1,  "a":123456789012345678901234567890,"e":"\u00e9"}"#
        );
    }

    #[test]
    fn capabilities_are_the_two_known_words() {
        let request: RegisterRequest =
            parse(json!({"capabilities": ["tasks", "kv"], "client": "sdk/1"})).unwrap();
        assert_eq!(request.capabilities, [Capability::Tasks, Capability::Kv]);
        assert_eq!(request.client.as_deref(), Some("sdk/1"));

        let without_client: RegisterRequest = parse(json!({"capabilities": ["kv"]})).unwrap();
        assert_eq!(without_client.client, None);

        assert!(parse::<RegisterRequest>(json!({"capabilities": ["blobs"]})).is_err());
        assert!(parse::<RegisterRequest>(json!({"capabilities": ["Tasks"]})).is_err());
        assert!(parse::<RegisterRequest>(json!({"client": "sdk"})).is_err());
    }

    fn task_query(params: Value) -> Result<TaskQuery, StateError> {
        TaskQuery::try_from(parse::<ListTasksQuery>(params).unwrap())
    }

    fn kv_query(params: Value) -> Result<KvQuery, StateError> {
        KvQuery::try_from(parse::<ListKvQuery>(params).unwrap())
    }

    #[test]
    fn a_bare_task_listing_takes_the_defaults() {
        let query = task_query(json!({})).unwrap();
        assert_eq!(query.context_id, None);
        assert_eq!(query.state, None);
        assert!(query.after.is_none());
        assert_eq!(query.page_size, 50);
    }

    #[test]
    fn empty_query_values_are_no_filter_rather_than_an_error() {
        let query =
            task_query(json!({"context_id": "", "state": "", "limit": "", "cursor": ""})).unwrap();
        assert_eq!(query.context_id, None);
        assert_eq!(query.state, None);
        assert!(query.after.is_none());
        assert_eq!(query.page_size, 50);

        let query = kv_query(json!({"prefix": "", "limit": "", "cursor": ""})).unwrap();
        assert_eq!(query.prefix, None);
        assert!(query.after.is_none());
    }

    #[test]
    fn task_filters_and_the_limit_are_carried_through() {
        let query =
            task_query(json!({"context_id": "c1", "state": "working", "limit": "7"})).unwrap();
        assert_eq!(query.context_id.as_deref(), Some("c1"));
        assert_eq!(query.state.as_deref(), Some("working"));
        assert_eq!(query.page_size, 7);
    }

    #[test]
    fn a_task_listing_refuses_what_the_database_cannot_hold_or_the_api_does_not_allow() {
        for bad in [
            json!({"limit": "0"}),
            json!({"limit": "101"}),
            json!({"limit": "x"}),
            json!({"context_id": "a\u{0}b"}),
            json!({"state": "a\nb"}),
            json!({"context_id": "c".repeat(257)}),
            json!({"cursor": "garbage"}),
        ] {
            assert!(task_query(bad.clone()).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn a_cursor_the_api_issued_is_accepted_back() {
        let issued = TaskCursor {
            updated_at: chrono::DateTime::from_timestamp_micros(1_790_000_000_000_001).unwrap(),
            task_id: "t-9".into(),
        };
        let query = task_query(json!({ "cursor": issued.encode() })).unwrap();
        assert_eq!(query.after, Some(issued));

        let issued = KeyCursor { key: "k-9".into() };
        let query = kv_query(json!({ "cursor": issued.encode() })).unwrap();
        assert_eq!(query.after, Some(issued));
    }

    #[test]
    fn a_kv_listing_refuses_an_unstorable_prefix_and_a_foreign_cursor() {
        assert!(kv_query(json!({"prefix": "a\u{0}"})).is_err());
        assert!(kv_query(json!({"prefix": "p".repeat(513)})).is_err());
        assert!(kv_query(json!({"limit": "101"})).is_err());
        let task_cursor = TaskCursor {
            updated_at: chrono::DateTime::from_timestamp_micros(1).unwrap(),
            task_id: "t".into(),
        };
        assert!(kv_query(json!({ "cursor": task_cursor.encode() })).is_err());
    }

    fn task_put(fields: Value) -> PutTaskRequest {
        parse(fields).unwrap()
    }

    #[test]
    fn a_task_put_is_checked_for_a_usable_state_and_context() {
        assert!(
            task_put(json!({"state": "working", "body": 1}))
                .check()
                .is_ok()
        );
        assert!(
            task_put(json!({"context_id": "c", "state": "s", "body": 1}))
                .check()
                .is_ok()
        );
        for bad in [
            json!({"state": "", "body": 1}),
            json!({"state": "s".repeat(129), "body": 1}),
            json!({"state": "a\nb", "body": 1}),
            json!({"context_id": "", "state": "s", "body": 1}),
            json!({"context_id": "c".repeat(257), "state": "s", "body": 1}),
            json!({"context_id": "a\u{0}b", "state": "s", "body": 1}),
        ] {
            assert!(
                task_put(bad.clone()).check().is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_registration_must_declare_something_and_name_a_loggable_client() {
        let register = |fields: Value| parse::<RegisterRequest>(fields).unwrap();
        assert!(register(json!({"capabilities": ["kv"]})).check().is_ok());
        assert!(
            register(json!({"capabilities": ["tasks"], "client": "sdk/1"}))
                .check()
                .is_ok()
        );
        for bad in [
            json!({"capabilities": []}),
            json!({"capabilities": ["tasks"], "client": ""}),
            json!({"capabilities": ["tasks"], "client": "c".repeat(129)}),
            json!({"capabilities": ["tasks"], "client": "two\nlines"}),
        ] {
            assert!(
                register(bad.clone()).check().is_err(),
                "{bad} must be refused"
            );
        }
    }

    fn at(micros: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(micros).unwrap()
    }

    #[test]
    fn a_listing_resumes_just_past_the_task_its_page_ended_on() {
        let row = TaskRow {
            task_id: "b".into(),
            context_id: None,
            state: "working".into(),
            sealed_body: String::new(),
            version: 1,
            updated_at: at(10),
            expires_at: at(11),
        };
        assert_eq!(
            TaskCursor::decode(&row.resume_after()).unwrap(),
            TaskCursor {
                updated_at: at(10),
                task_id: "b".into()
            }
        );
    }

    #[test]
    fn a_listing_resumes_just_past_the_key_its_page_ended_on() {
        let row = KvRow {
            key: "b".into(),
            sealed_value: String::new(),
            version: 1,
            updated_at: at(1),
            expires_at: at(2),
        };
        assert_eq!(
            KeyCursor::decode(&row.resume_after()).unwrap(),
            KeyCursor { key: "b".into() }
        );
    }

    #[test]
    fn only_a_task_store_makes_an_agent_durable() {
        let stores_tasks = |capabilities: Value| {
            parse::<RegisterRequest>(json!({ "capabilities": capabilities }))
                .unwrap()
                .stores_tasks()
        };
        assert!(stores_tasks(json!(["tasks"])));
        assert!(stores_tasks(json!(["kv", "tasks"])));
        assert!(!stores_tasks(json!(["kv"])));
    }
}
