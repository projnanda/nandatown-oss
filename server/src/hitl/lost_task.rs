//! Recognising that the agent no longer holds the paused task.
//!
//! A paused A2A task lives in the agent's own memory: an `InMemoryTaskStore` loses it on restart,
//! and an answer that reaches a replica that never saw the task finds nothing to continue. The
//! agent says so with A2A's `TaskNotFoundError`, JSON-RPC code -32001, carried in the reply: an
//! SSE `data:` frame or a JSON body at HTTP 200 (the Python a2a-sdk), or the body of an HTTP 404
//! (the Rust a2a-server, which maps the error to that status,
//! `oss/docs/A2A_PROTOCOL.md` "HTTP Status Code Mapping").
//!
//! The code is the only signal. A bare 404 is not one: a proxy, an ingress or a mid-rollout
//! replica answers 404 with an HTML or empty body while the agent still holds the task, and
//! concluding "lost" from it would throw away an answer that a retry could still deliver.
//!
//! A single -32001 does not conclude anything either. A round-robin Service can route the resume
//! to a replica that never saw the task while another replica holds it, so the dispatcher re-sends
//! the resume a bounded number of times (`HITL_LOST_TASK_RETRIES`) before it records
//! `agent_lost_task`.

/// JSON-RPC code of A2A's `TaskNotFoundError`.
const TASK_NOT_FOUND_CODE: i64 = -32001;

/// Whether `data`, one SSE `data:` payload or a whole JSON body, is a JSON-RPC error carrying
/// A2A's `TaskNotFoundError` code. Reads only `error.code`: a -32001 mentioned anywhere else in a
/// reply (agent text, say) is not the agent reporting anything.
pub(super) fn is_task_not_found(data: &str) -> bool {
    let Ok(reply) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    reply
        .pointer("/error/code")
        .and_then(serde_json::Value::as_i64)
        == Some(TASK_NOT_FOUND_CODE)
}

/// What `resume_last_error` records, so an operator can see how many times the agent said it had
/// no such task before the row was concluded lost.
pub(super) fn describe(attempts: u32) -> String {
    format!(
        "agent no longer holds the paused task (A2A TaskNotFoundError, JSON-RPC -32001, on all \
         {attempts} delivery attempts)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_json_rpc_task_not_found_error_is_a_lost_task() {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "error": { "code": -32001, "message": "Task not found" },
        })
        .to_string();
        assert!(is_task_not_found(&reply));
    }

    #[test]
    fn the_error_message_and_id_do_not_matter() {
        // Only the code identifies the error; SDKs word the message differently and the id may be
        // a string, a number or null.
        for reply in [
            json!({ "id": 7, "error": { "code": -32001 } }),
            json!({ "id": null, "error": { "code": -32001, "message": "no such task: t-1" } }),
            json!({ "error": { "code": -32001, "message": "", "data": { "taskId": "t-1" } } }),
        ] {
            assert!(is_task_not_found(&reply.to_string()), "{reply}");
        }
    }

    #[test]
    fn any_other_json_rpc_error_is_not_a_lost_task() {
        for code in [
            -32700, -32600, -32601, -32602, -32603, -32002, -32006, -32009, 0, 404,
        ] {
            let reply = json!({ "id": 1, "error": { "code": code, "message": "x" } }).to_string();
            assert!(!is_task_not_found(&reply), "code {code}");
        }
    }

    #[test]
    fn a_reply_that_is_not_an_error_is_not_a_lost_task() {
        for reply in [
            // A task's own status update, even a failed one.
            json!({ "statusUpdate": { "taskId": "t-1", "status": { "state": "TASK_STATE_FAILED" } } }),
            json!({ "result": { "kind": "status-update", "final": true } }),
            // The code appearing somewhere other than `error.code` is not the agent reporting it.
            json!({ "result": { "message": "retry after error -32001" } }),
            json!({ "error": { "message": "Task not found" } }),
            json!({ "error": null }),
        ] {
            assert!(!is_task_not_found(&reply.to_string()), "{reply}");
        }
    }

    #[test]
    fn a_malformed_code_is_not_a_lost_task() {
        for code in [
            json!("-32001"),
            json!(-32001.5),
            json!(null),
            json!([-32001]),
        ] {
            let reply = json!({ "error": { "code": code } }).to_string();
            assert!(!is_task_not_found(&reply), "{reply}");
        }
    }

    #[test]
    fn a_body_that_is_not_json_is_not_a_lost_task() {
        // An HTTP 404 from a proxy or an ingress, which says nothing about the task.
        for reply in [
            "",
            "not json",
            "<html><body><h1>404 Not Found</h1></body></html>",
            "data: {\"error\":{\"code\":-32001}}",
            "{\"error\":",
        ] {
            assert!(!is_task_not_found(reply), "{reply:?}");
        }
    }

    #[test]
    fn the_description_names_the_signal_and_the_attempts() {
        let text = describe(4);
        assert!(text.contains("-32001"));
        assert!(text.contains("4 delivery attempts"));
    }
}
